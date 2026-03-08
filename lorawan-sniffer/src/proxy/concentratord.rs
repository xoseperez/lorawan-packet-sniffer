/// ChirpStack Concentratord ZMQ MITM proxy.
///
/// Sits between a concentratord instance and a ChirpStack Gateway Bridge:
///
/// ```text
/// Concentratord ──[ZMQ PUB]──▶ Sniffer SUB/PUB ──▶ Gateway Bridge
/// Concentratord ◀──[ZMQ REP]── Sniffer REP/REQ ◀── Gateway Bridge
/// ```
///
/// ## Thread model
///
/// ZMQ sockets are not async-compatible, so two dedicated `std::thread`s are
/// used.  They communicate with the tokio runtime via:
///   - `mpsc::Sender<MessageRecord>` → SQLite writer
///   - `broadcast::Sender<Arc<String>>` → WebSocket fan-out
///
/// Shutdown is signalled via an `Arc<AtomicBool>` that the threads poll on
/// every `POLL_TIMEOUT_MS` iteration.
///
/// ## Wire format (chirpstack-concentratord v4)
///
/// **Events (PUB/SUB):** single-frame protobuf, encoded as `gw::Event`
/// (a `oneof` wrapper — `UplinkFrame`, `GatewayStats`, `DownlinkTxAck`, …).
///
/// **Commands (REQ/REP):** single-frame protobuf, encoded as `gw::Command`
/// (a `oneof` wrapper — `SendDownlinkFrame`, `GetGatewayId`, …).
/// The REP socket responds with the appropriate proto response bytes.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Result;
use prost::Message as _;
use chrono::Utc;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::core::decoder;
use crate::core::filter::{self, FilterRule};
use crate::core::metadata::{CrcStatus, Direction, MessageRecord, Protocol, Status};
use crate::core::toa;

use crate::config::ConcentratordConfig;

use super::gw;
use super::{broadcast_record, decision_to_status, log_message, send_record, unpack_frame};

/// ZMQ `poll` timeout in milliseconds — governs shutdown responsiveness.
const POLL_TIMEOUT_MS: i64 = 100;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run the Concentratord MITM proxy until `cancel` is triggered.
///
/// Spawns two `std::thread`s (event + command) and awaits the cancellation
/// token.  On cancel, signals both threads to stop and joins them.
pub async fn run(
    config: &ConcentratordConfig,
    filters: &[FilterRule],
    record_tx: mpsc::Sender<MessageRecord>,
    broadcast_tx: broadcast::Sender<Arc<String>>,
    cancel: CancellationToken,
) -> Result<()> {
    info!(
        "Starting Concentratord proxy (downstream event={} cmd={}, upstream event={} cmd={})",
        config.downstream.event_url,
        config.downstream.command_url,
        config.upstream.event_bind,
        config.upstream.command_bind,
    );

    let shutdown = Arc::new(AtomicBool::new(false));

    // ── Event thread ─────────────────────────────────────────────────────
    let ev_config = config.clone();
    let ev_filters = filters.to_vec();
    let ev_record_tx = record_tx.clone();
    let ev_broadcast_tx = broadcast_tx.clone();
    let ev_shutdown = Arc::clone(&shutdown);

    let event_handle = std::thread::Builder::new()
        .name("zmq-event".to_string())
        .spawn(move || {
            run_event_thread(&ev_config, &ev_filters, ev_record_tx, ev_broadcast_tx, &ev_shutdown);
        })?;

    // ── Command thread ────────────────────────────────────────────────────
    let cmd_config = config.clone();
    let cmd_filters = filters.to_vec();
    let cmd_record_tx = record_tx.clone();
    let cmd_broadcast_tx = broadcast_tx.clone();
    let cmd_shutdown = Arc::clone(&shutdown);

    let cmd_handle = std::thread::Builder::new()
        .name("zmq-command".to_string())
        .spawn(move || {
            run_command_thread(
                &cmd_config,
                &cmd_filters,
                cmd_record_tx,
                cmd_broadcast_tx,
                &cmd_shutdown,
            );
        })?;

    // ── Wait for shutdown ─────────────────────────────────────────────────
    cancel.cancelled().await;
    info!("Concentratord proxy shutting down");
    shutdown.store(true, Ordering::Release);

    tokio::task::spawn_blocking(move || {
        if let Err(e) = event_handle.join() {
            error!("zmq-event thread panicked: {:?}", e);
        }
        if let Err(e) = cmd_handle.join() {
            error!("zmq-command thread panicked: {:?}", e);
        }
    })
    .await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Event thread  (SUB ← concentratord)  →  (PUB → gateway bridge)
// ---------------------------------------------------------------------------

fn run_event_thread(
    config: &ConcentratordConfig,
    filters: &[FilterRule],
    record_tx: mpsc::Sender<MessageRecord>,
    broadcast_tx: broadcast::Sender<Arc<String>>,
    shutdown: &AtomicBool,
) {
    let ctx = zmq::Context::new();

    let sub = match ctx.socket(zmq::SUB) {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to create ZMQ SUB socket: {}", e);
            return;
        }
    };
    if let Err(e) = sub.connect(&config.downstream.event_url) {
        error!("ZMQ SUB connect to {} failed: {}", config.downstream.event_url, e);
        return;
    }
    if let Err(e) = sub.set_subscribe(b"") {
        error!("ZMQ SUB subscribe failed: {}", e);
        return;
    }

    let pub_sock = match ctx.socket(zmq::PUB) {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to create ZMQ PUB socket: {}", e);
            return;
        }
    };
    if let Err(e) = pub_sock.bind(&config.upstream.event_bind) {
        error!("ZMQ PUB bind to {} failed: {}", config.upstream.event_bind, e);
        return;
    }

    info!("zmq-event: SUB {} → PUB {}", config.downstream.event_url, config.upstream.event_bind);

    loop {
        if shutdown.load(Ordering::Acquire) {
            break;
        }

        let mut items = [sub.as_poll_item(zmq::POLLIN)];
        if let Err(e) = zmq::poll(&mut items, POLL_TIMEOUT_MS) {
            warn!("zmq-event poll error: {}", e);
            continue;
        }
        if !items[0].is_readable() {
            continue;
        }

        let bytes = match sub.recv_bytes(0) {
            Ok(b) => b,
            Err(e) => {
                warn!("zmq-event SUB recv error: {}", e);
                continue;
            }
        };

        let should_forward = match gw::Event::decode(bytes.as_slice()) {
            Err(e) => {
                warn!("gw::Event decode error: {} — forwarding raw bytes", e);
                true
            }
            Ok(event) => match event.event {
                Some(gw::event::Event::UplinkFrame(ref frame)) => {
                    process_uplink(frame, filters, &record_tx, &broadcast_tx)
                }
                Some(gw::event::Event::GatewayStats(ref stats)) => {
                    debug!(
                        "Concentratord stats: rx_received={} rx_ok={} tx_received={} tx_emitted={}",
                        stats.rx_packets_received,
                        stats.rx_packets_received_ok,
                        stats.tx_packets_received,
                        stats.tx_packets_emitted,
                    );
                    true // always forward
                }
                _ => true, // unknown / future events: always forward
            },
        };

        if should_forward {
            if let Err(e) = pub_sock.send(bytes, 0) {
                warn!("zmq-event PUB send error: {}", e);
            }
        }
    }

    info!("zmq-event thread stopped");
}

// ---------------------------------------------------------------------------
// Command thread  (REP ← gateway bridge)  ↔  (REQ → concentratord)
// ---------------------------------------------------------------------------

fn run_command_thread(
    config: &ConcentratordConfig,
    filters: &[FilterRule],
    record_tx: mpsc::Sender<MessageRecord>,
    broadcast_tx: broadcast::Sender<Arc<String>>,
    shutdown: &AtomicBool,
) {
    let ctx = zmq::Context::new();

    let rep = match ctx.socket(zmq::REP) {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to create ZMQ REP socket: {}", e);
            return;
        }
    };
    if let Err(e) = rep.bind(&config.upstream.command_bind) {
        error!("ZMQ REP bind to {} failed: {}", config.upstream.command_bind, e);
        return;
    }

    let req = match ctx.socket(zmq::REQ) {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to create ZMQ REQ socket: {}", e);
            return;
        }
    };
    if let Err(e) = req.connect(&config.downstream.command_url) {
        error!("ZMQ REQ connect to {} failed: {}", config.downstream.command_url, e);
        return;
    }

    info!(
        "zmq-command: REP {} ↔ REQ {}",
        config.upstream.command_bind, config.downstream.command_url
    );

    loop {
        if shutdown.load(Ordering::Acquire) {
            break;
        }

        let mut items = [rep.as_poll_item(zmq::POLLIN)];
        if let Err(e) = zmq::poll(&mut items, POLL_TIMEOUT_MS) {
            warn!("zmq-command poll error: {}", e);
            continue;
        }
        if !items[0].is_readable() {
            continue;
        }

        let bytes = match rep.recv_bytes(0) {
            Ok(b) => b,
            Err(e) => {
                warn!("zmq-command REP recv error: {}", e);
                continue;
            }
        };

        let response = match gw::Command::decode(bytes.as_slice()) {
            Err(e) => {
                warn!("gw::Command decode error: {} — forwarding to concentratord", e);
                relay(&req, &bytes)
            }
            Ok(ref cmd) => match &cmd.command {
                Some(gw::command::Command::SendDownlinkFrame(frame)) => {
                    process_downlink(frame, filters, &record_tx, &broadcast_tx, &req, &bytes)
                }
                Some(gw::command::Command::GetGatewayId(_)) => {
                    debug!("Command: GetGatewayId");
                    relay(&req, &bytes)
                }
                Some(gw::command::Command::SetGatewayConfiguration(_)) => {
                    debug!("Command: SetGatewayConfiguration");
                    relay(&req, &bytes)
                }
                Some(gw::command::Command::GetLocation(_)) => {
                    debug!("Command: GetLocation");
                    relay(&req, &bytes)
                }
                None => relay(&req, &bytes),
            },
        };

        if let Err(e) = rep.send(response, 0) {
            warn!("zmq-command REP send error: {}", e);
        }
    }

    info!("zmq-command thread stopped");
}

// ---------------------------------------------------------------------------
// Payload processors
// ---------------------------------------------------------------------------

/// Decode and filter an uplink frame. Returns `true` if it should be forwarded.
fn process_uplink(
    frame: &gw::UplinkFrame,
    filters: &[FilterRule],
    record_tx: &mpsc::Sender<MessageRecord>,
    broadcast_tx: &broadcast::Sender<Arc<String>>,
) -> bool {
    let record = build_uplink_record(frame, filters);
    let blocked = record.status == Status::Blocked;
    log_message(&record);
    broadcast_record(&record, broadcast_tx);
    send_record(record, record_tx);
    !blocked
}

/// Decode and filter a downlink command.
///
/// Returns the response bytes for the gateway bridge's REP socket:
/// - Not blocked: relay to concentratord and return its response.
/// - Blocked: synthetic `DownlinkTxAck` (IGNORED), concentratord not contacted.
///
/// **Note on stealth mode**: ZMQ REQ/REP *requires* a response; the bridge
/// blocks waiting for one.  Stealth mode therefore behaves identically to
/// non-stealth for concentratord commands — the bridge will still receive an
/// IGNORED ack rather than nothing.
fn process_downlink(
    frame: &gw::DownlinkFrame,
    filters: &[FilterRule],
    record_tx: &mpsc::Sender<MessageRecord>,
    broadcast_tx: &broadcast::Sender<Arc<String>>,
    req: &zmq::Socket,
    raw_bytes: &[u8],
) -> Vec<u8> {
    let record = build_downlink_record(frame, filters);
    let blocked = record.status == Status::Blocked;
    log_message(&record);
    broadcast_record(&record, broadcast_tx);
    send_record(record, record_tx);

    if blocked {
        debug!("Downlink blocked — sending synthetic DownlinkTxAck IGNORED to bridge");
        let ack = gw::DownlinkTxAck {
            downlink_id: frame.downlink_id,
            items: frame
                .items
                .iter()
                .map(|_| gw::DownlinkTxAckItem {
                    status: gw::TxAckStatus::Ignored as i32,
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        ack.encode_to_vec()
    } else {
        relay(req, raw_bytes)
    }
}

/// Forward raw bytes to concentratord on the REQ socket and return the response.
fn relay(req: &zmq::Socket, bytes: &[u8]) -> Vec<u8> {
    if let Err(e) = req.send(bytes, 0) {
        warn!("zmq REQ send error: {}", e);
        return Vec::new();
    }
    match req.recv_bytes(0) {
        Ok(resp) => resp,
        Err(e) => {
            warn!("zmq REQ recv error: {}", e);
            Vec::new()
        }
    }
}

// ---------------------------------------------------------------------------
// MessageRecord builders
// ---------------------------------------------------------------------------

fn build_uplink_record(frame: &gw::UplinkFrame, filters: &[FilterRule]) -> MessageRecord {
    let (freq_mhz, datarate, codr) = extract_uplink_rf(frame);
    let (rssi, snr, crc) = extract_rx_info(frame);
    let raw_size = frame.phy_payload.len() as u16;

    let decoded = decoder::decode_phy_bytes(&frame.phy_payload);
    let (mtype, dev_addr, dev_eui, fcnt, fport, payload_size) = unpack_frame(decoded, raw_size);

    let fd = filter::evaluate(filters, &Direction::Up, &mtype);
    let (status, blocked_by) = decision_to_status(&fd);
    let toa = toa::compute_lora_toa(payload_size, &datarate, &codr);

    MessageRecord {
        id: MessageRecord::new_id(),
        timestamp: Utc::now(),
        direction: Direction::Up,
        status,
        blocked_by,
        source_protocol: Protocol::Concentratord,
        freq_mhz,
        datarate,
        rssi,
        snr,
        crc_status: crc,
        mtype,
        dev_addr,
        dev_eui,
        fcnt,
        fport,
        payload_size,
        toa_ms: toa,
    }
}

fn build_downlink_record(frame: &gw::DownlinkFrame, filters: &[FilterRule]) -> MessageRecord {
    let first = frame.items.first();
    let phy = first.map(|i| i.phy_payload.as_slice()).unwrap_or(&[]);
    let raw_size = phy.len() as u16;

    let (freq_mhz, datarate, codr) = extract_downlink_rf(frame);
    let decoded = decoder::decode_phy_bytes(phy);
    let (mtype, dev_addr, dev_eui, fcnt, fport, payload_size) = unpack_frame(decoded, raw_size);

    let fd = filter::evaluate(filters, &Direction::Down, &mtype);
    let (status, blocked_by) = decision_to_status(&fd);
    let toa = toa::compute_lora_toa(payload_size, &datarate, &codr);

    MessageRecord {
        id: MessageRecord::new_id(),
        timestamp: Utc::now(),
        direction: Direction::Down,
        status,
        blocked_by,
        source_protocol: Protocol::Concentratord,
        freq_mhz,
        datarate,
        rssi: None,
        snr: None,
        crc_status: None,
        mtype,
        dev_addr,
        dev_eui,
        fcnt,
        fport,
        payload_size,
        toa_ms: toa,
    }
}

// ---------------------------------------------------------------------------
// RF metadata extraction
// ---------------------------------------------------------------------------

/// Extract `(freq_mhz, datarate_str, code_rate_str)` from an uplink frame.
fn extract_uplink_rf(frame: &gw::UplinkFrame) -> (f64, String, String) {
    let tx_info = match &frame.tx_info {
        Some(t) => t,
        None => return (0.0, "unknown".to_string(), "4/5".to_string()),
    };
    let freq_mhz = tx_info.frequency as f64 / 1_000_000.0;
    lora_modulation(tx_info.modulation.as_ref(), freq_mhz)
}

/// Extract `(freq_mhz, datarate_str, code_rate_str)` from a downlink frame.
fn extract_downlink_rf(frame: &gw::DownlinkFrame) -> (f64, String, String) {
    let tx_info = match frame.items.first().and_then(|i| i.tx_info.as_ref()) {
        Some(t) => t,
        None => return (0.0, "unknown".to_string(), "4/5".to_string()),
    };
    let freq_mhz = tx_info.frequency as f64 / 1_000_000.0;
    lora_modulation(tx_info.modulation.as_ref(), freq_mhz)
}

/// Decode a `Modulation` message into `(freq_mhz, datarate, code_rate)`.
fn lora_modulation(modulation: Option<&gw::Modulation>, freq_mhz: f64) -> (f64, String, String) {
    match modulation.and_then(|m| m.parameters.as_ref()) {
        Some(gw::modulation::Parameters::Lora(lora)) => {
            // bandwidth is in Hz in chirpstack_api v4 (125000 = 125 kHz)
            let bw_khz = lora.bandwidth / 1_000;
            let dr = format!("SF{}BW{}", lora.spreading_factor, bw_khz);
            let cr = code_rate_str(lora);
            (freq_mhz, dr, cr)
        }
        Some(gw::modulation::Parameters::Fsk(fsk)) => {
            (freq_mhz, format!("FSK{}", fsk.datarate), String::new())
        }
        _ => (freq_mhz, "unknown".to_string(), "4/5".to_string()),
    }
}

/// Convert `LoraModulationInfo` code rate to a "4/5" style string.
///
/// Prefers the legacy string field (populated by older concentratord versions)
/// and falls back to the `CodeRate` enum.
fn code_rate_str(lora: &gw::LoraModulationInfo) -> String {
    if !lora.code_rate_legacy.is_empty() {
        return lora.code_rate_legacy.clone();
    }
    match gw::CodeRate::try_from(lora.code_rate).unwrap_or(gw::CodeRate::CrUndefined) {
        gw::CodeRate::Cr45 => "4/5",
        gw::CodeRate::Cr46 => "4/6",
        gw::CodeRate::Cr47 => "4/7",
        gw::CodeRate::Cr48 => "4/8",
        gw::CodeRate::Cr38 => "3/8",
        gw::CodeRate::Cr26 => "2/6",
        gw::CodeRate::Cr14 => "1/4",
        gw::CodeRate::Cr16 => "1/6",
        gw::CodeRate::Cr56 => "5/6",
        _ => "4/5",
    }
    .to_string()
}

/// Extract `(rssi, snr, crc_status)` from `UplinkFrame.rx_info`.
fn extract_rx_info(
    frame: &gw::UplinkFrame,
) -> (Option<i16>, Option<f32>, Option<CrcStatus>) {
    let rx = match &frame.rx_info {
        Some(r) => r,
        None => return (None, None, None),
    };
    let rssi = Some(rx.rssi as i16);
    let snr = Some(rx.snr);
    let crc = Some(
        match gw::CrcStatus::try_from(rx.crc_status).unwrap_or(gw::CrcStatus::NoCrc) {
            gw::CrcStatus::CrcOk => CrcStatus::Ok,
            gw::CrcStatus::BadCrc => CrcStatus::Fail,
            _ => CrcStatus::NoCrc,
        },
    );
    (rssi, snr, crc)
}
