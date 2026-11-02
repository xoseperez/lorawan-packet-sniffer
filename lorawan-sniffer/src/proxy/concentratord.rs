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
/// used.  They hand every captured record to the rest of the program through
/// [`Outputs::emit`], which only does non-blocking sends, so it is safe to call
/// from a plain thread.
///
/// ## Gateway identity
///
/// A Concentratord serves exactly one gateway, but not every message says
/// which.  Uplinks carry the gateway ID in `rx_info`, and so does the reply to
/// a `GetGatewayId` command; downlinks carry it only when the gateway bridge
/// fills it in.  Both threads share a [`LearnedGateway`] so a downlink without
/// an ID can still be attributed to the gateway seen on the uplinks.
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
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::Result;
use chrono::Utc;
use prost::Message as _;
use tokio_util::sync::CancellationToken;

use crate::core::decoder;
use crate::core::eui::Eui64;
use crate::core::filter::FilterRule;
use crate::core::metadata::{CrcStatus, Direction, MessageRecord, Protocol};
use crate::core::ratelimit::RateLimitedLog;
use crate::core::toa;

use crate::config::ConcentratordConfig;

use super::gw;
use super::{classify, frame_fields, FrameFields, Outputs, Verdict};

/// ZMQ `poll` timeout in milliseconds — governs shutdown responsiveness.
const POLL_TIMEOUT_MS: i64 = 100;

/// Timeout (ms) for the downstream REQ socket when relaying commands to
/// concentratord.  Prevents the command thread from blocking forever if
/// concentratord is unreachable (e.g. during startup).
const REQ_TIMEOUT_MS: i32 = 5000;

// ---------------------------------------------------------------------------
// Gateway identity
// ---------------------------------------------------------------------------

/// The ID of the gateway behind this Concentratord, once it has been seen.
///
/// Written by the event thread (from uplinks) and by the command thread (from
/// `GetGatewayId` replies), read by both.
#[derive(Clone, Default)]
struct LearnedGateway(Arc<Mutex<Option<Eui64>>>);

impl LearnedGateway {
    fn get(&self) -> Option<Eui64> {
        // A poisoned lock still holds a valid ID: the value is only ever
        // replaced as a whole.
        *self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn set(&self, id: Eui64) {
        let changed = {
            let mut slot = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            let changed = *slot != Some(id);
            *slot = Some(id);
            changed
        };
        if changed {
            info!("Gateway ID: {}", id);
        }
    }
}

/// Rate limit for the "unusable gateway ID" warning: a gateway bridge that
/// sends a broken ID sends it with every frame.
static BAD_GATEWAY_ID: RateLimitedLog = RateLimitedLog::new();

/// Parse a gateway ID as the protobuf messages carry it: 16 hex digits.
///
/// An empty string means the sender did not fill the field in, which is
/// normal and not worth a message.
fn parse_gateway_id(id: &str) -> Option<Eui64> {
    if id.is_empty() {
        return None;
    }
    match id.parse() {
        Ok(eui) => Some(eui),
        Err(e) => {
            BAD_GATEWAY_ID.warn(format_args!("Ignoring unusable gateway ID: {}", e));
            None
        }
    }
}

/// Learn the gateway ID from the reply to a relayed `GetGatewayId` command.
fn learn_from_gateway_id_reply(reply: &[u8], learned: &LearnedGateway) {
    match gw::GetGatewayIdResponse::decode(reply) {
        Ok(reply) => {
            if let Some(id) = parse_gateway_id(&reply.gateway_id) {
                learned.set(id);
            }
        }
        Err(e) => debug!("GetGatewayId reply could not be decoded: {}", e),
    }
}

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
    outputs: Outputs,
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

    // Single ZMQ context shared by both threads — one I/O thread pool
    // handles all sockets, avoiding potential issues with separate contexts
    // in static musl builds.
    let zmq_ctx = zmq::Context::new();

    // The gateway ID, learned by whichever thread sees it first.
    let learned = LearnedGateway::default();

    // ── Event thread ─────────────────────────────────────────────────────
    let ev_config = config.clone();
    let ev_filters = filters.to_vec();
    let ev_outputs = outputs.clone();
    let ev_learned = learned.clone();
    let ev_shutdown = Arc::clone(&shutdown);
    let ev_ctx = zmq_ctx.clone();

    let event_handle = std::thread::Builder::new()
        .name("zmq-event".to_string())
        .spawn(move || {
            run_event_thread(
                &ev_ctx,
                &ev_config,
                &ev_filters,
                &ev_outputs,
                &ev_learned,
                &ev_shutdown,
            );
        })?;

    // ── Command thread ────────────────────────────────────────────────────
    let cmd_config = config.clone();
    let cmd_filters = filters.to_vec();
    let cmd_outputs = outputs;
    let cmd_learned = learned;
    let cmd_shutdown = Arc::clone(&shutdown);
    let cmd_ctx = zmq_ctx.clone();

    let cmd_handle = std::thread::Builder::new()
        .name("zmq-command".to_string())
        .spawn(move || {
            run_command_thread(
                &cmd_ctx,
                &cmd_config,
                &cmd_filters,
                &cmd_outputs,
                &cmd_learned,
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
    ctx: &zmq::Context,
    config: &ConcentratordConfig,
    filters: &[FilterRule],
    outputs: &Outputs,
    learned: &LearnedGateway,
    shutdown: &AtomicBool,
) {
    let sub = match ctx.socket(zmq::SUB) {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to create ZMQ SUB socket: {}", e);
            return;
        }
    };
    if let Err(e) = sub.connect(&config.downstream.event_url) {
        error!(
            "ZMQ SUB connect to {} failed: {}",
            config.downstream.event_url, e
        );
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
    // Remove any stale socket file from a previous run before binding.
    if config.upstream.event_bind.starts_with("ipc://") {
        let ipc_path = &config.upstream.event_bind["ipc://".len()..];
        if std::path::Path::new(ipc_path).exists() {
            warn!("zmq-event: removing stale socket file {}", ipc_path);
            let _ = std::fs::remove_file(ipc_path);
        }
    }
    if let Err(e) = pub_sock.bind(&config.upstream.event_bind) {
        error!(
            "ZMQ PUB bind to {} failed: {}",
            config.upstream.event_bind, e
        );
        return;
    }
    chmod_ipc_socket(&config.upstream.event_bind);

    info!(
        "zmq-event: SUB {} → PUB {}",
        config.downstream.event_url, config.upstream.event_bind
    );

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

        if handle_event(&bytes, filters, learned, outputs) {
            if let Err(e) = pub_sock.send(bytes, 0) {
                warn!("zmq-event PUB send error: {}", e);
            }
        }
    }

    info!("zmq-event thread stopped");
}

/// Process one event from concentratord.  Returns `true` if the original
/// bytes should be forwarded to the gateway bridge.
fn handle_event(
    bytes: &[u8],
    filters: &[FilterRule],
    learned: &LearnedGateway,
    outputs: &Outputs,
) -> bool {
    match gw::Event::decode(bytes) {
        Err(e) => {
            warn!("gw::Event decode error: {} — forwarding raw bytes", e);
            true
        }
        Ok(event) => match event.event {
            Some(gw::event::Event::UplinkFrame(ref frame)) => {
                process_uplink(frame, filters, learned, outputs)
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
    }
}

// ---------------------------------------------------------------------------
// Command thread  (REP ← gateway bridge)  ↔  (REQ → concentratord)
// ---------------------------------------------------------------------------

fn run_command_thread(
    ctx: &zmq::Context,
    config: &ConcentratordConfig,
    filters: &[FilterRule],
    outputs: &Outputs,
    learned: &LearnedGateway,
    shutdown: &AtomicBool,
) {
    let rep = match ctx.socket(zmq::REP) {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to create ZMQ REP socket: {}", e);
            return;
        }
    };

    // Remove any stale socket file from a previous run before binding.
    if config.upstream.command_bind.starts_with("ipc://") {
        let ipc_path = &config.upstream.command_bind["ipc://".len()..];
        if std::path::Path::new(ipc_path).exists() {
            warn!("zmq-command: removing stale socket file {}", ipc_path);
            let _ = std::fs::remove_file(ipc_path);
        }
    }

    if let Err(e) = rep.bind(&config.upstream.command_bind) {
        error!(
            "ZMQ REP bind to {} failed: {}",
            config.upstream.command_bind, e
        );
        return;
    }
    chmod_ipc_socket(&config.upstream.command_bind);

    let req = match ctx.socket(zmq::REQ) {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to create ZMQ REQ socket: {}", e);
            return;
        }
    };
    // REQ_RELAXED + REQ_CORRELATE allow the socket to recover after a
    // send/recv timeout instead of getting permanently stuck in the
    // "waiting-for-reply" state.
    let _ = req.set_req_relaxed(true);
    let _ = req.set_req_correlate(true);
    let _ = req.set_rcvtimeo(REQ_TIMEOUT_MS);
    let _ = req.set_sndtimeo(REQ_TIMEOUT_MS);
    if let Err(e) = req.connect(&config.downstream.command_url) {
        error!(
            "ZMQ REQ connect to {} failed: {}",
            config.downstream.command_url, e
        );
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

        debug!("zmq-command: received command ({} bytes)", bytes.len());

        let response = match gw::Command::decode(bytes.as_slice()) {
            Err(e) => {
                warn!(
                    "gw::Command decode error: {} — forwarding to concentratord",
                    e
                );
                relay(&req, &bytes, shutdown)
            }
            Ok(ref cmd) => match &cmd.command {
                Some(gw::command::Command::SendDownlinkFrame(frame)) => {
                    match process_downlink(frame, filters, learned, outputs) {
                        DownlinkAction::Relay => relay(&req, &bytes, shutdown),
                        DownlinkAction::Reply(response) => response,
                    }
                }
                Some(gw::command::Command::GetGatewayId(_)) => {
                    debug!("Command: GetGatewayId");
                    let response = relay(&req, &bytes, shutdown);
                    learn_from_gateway_id_reply(&response, learned);
                    response
                }
                Some(gw::command::Command::SetGatewayConfiguration(_)) => {
                    debug!("Command: SetGatewayConfiguration");
                    relay(&req, &bytes, shutdown)
                }
                Some(gw::command::Command::GetLocation(_)) => {
                    debug!("Command: GetLocation");
                    relay(&req, &bytes, shutdown)
                }
                None => relay(&req, &bytes, shutdown),
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
///
/// The uplink's own gateway ID is remembered for later messages that lack
/// one, and stands in for the ID itself when the uplink does not carry it.
fn process_uplink(
    frame: &gw::UplinkFrame,
    filters: &[FilterRule],
    learned: &LearnedGateway,
    outputs: &Outputs,
) -> bool {
    let from_frame = frame
        .rx_info
        .as_ref()
        .and_then(|rx| parse_gateway_id(&rx.gateway_id));
    if let Some(id) = from_frame {
        learned.set(id);
    }
    let gateway = from_frame.or_else(|| learned.get());

    let (record, verdict) = build_uplink_record(frame, filters, gateway.as_ref());
    outputs.emit(record);
    !matches!(verdict, Verdict::Block { .. })
}

/// What to do with a `SendDownlinkFrame` command after it has been recorded.
enum DownlinkAction {
    /// Pass the original command on to concentratord and return its answer.
    Relay,
    /// Answer the gateway bridge with these bytes; concentratord is not
    /// contacted.
    Reply(Vec<u8>),
}

/// Decode and filter a downlink command.
///
/// - Not blocked: [`DownlinkAction::Relay`].
/// - Blocked: a synthetic `DownlinkTxAck` (IGNORED) to reply with.
///
/// The gateway is the one named in the command, or the one learned from the
/// uplinks when the command leaves it empty.
///
/// **Note on stealth mode**: ZMQ REQ/REP *requires* a response; the bridge
/// blocks waiting for one.  Stealth mode therefore behaves identically to
/// non-stealth for concentratord commands — the bridge will still receive an
/// IGNORED ack rather than nothing.
fn process_downlink(
    frame: &gw::DownlinkFrame,
    filters: &[FilterRule],
    learned: &LearnedGateway,
    outputs: &Outputs,
) -> DownlinkAction {
    let gateway = parse_gateway_id(&frame.gateway_id).or_else(|| learned.get());

    let (record, verdict) = build_downlink_record(frame, filters, gateway.as_ref());
    outputs.emit(record);

    if matches!(verdict, Verdict::Block { .. }) {
        debug!("Downlink blocked — sending synthetic DownlinkTxAck IGNORED to bridge");
        let ack = gw::DownlinkTxAck {
            downlink_id: frame.downlink_id,
            items: frame
                .items
                .iter()
                .map(|_| gw::DownlinkTxAckItem {
                    status: gw::TxAckStatus::Ignored as i32,
                })
                .collect(),
        };
        DownlinkAction::Reply(ack.encode_to_vec())
    } else {
        DownlinkAction::Relay
    }
}

/// Forward raw bytes to concentratord on the REQ socket and return the
/// response.
///
/// The REQ socket has `RCVTIMEO`/`SNDTIMEO` + `REQ_RELAXED` set, so
/// individual send/recv calls will not block forever.  If concentratord is
/// not yet ready (common during startup while hardware initialises), this
/// function retries every second until a response arrives or the shutdown
/// flag is raised.
fn relay(req: &zmq::Socket, bytes: &[u8], shutdown: &AtomicBool) -> Vec<u8> {
    loop {
        if shutdown.load(Ordering::Acquire) {
            return Vec::new();
        }

        if let Err(e) = req.send(bytes, 0) {
            warn!(
                "zmq REQ send to concentratord failed: {} — retrying in 1 s",
                e
            );
            interruptible_sleep(shutdown, 1000);
            continue;
        }
        match req.recv_bytes(0) {
            Ok(resp) => return resp,
            Err(e) => {
                warn!(
                    "zmq REQ recv from concentratord timed out: {} — retrying in 1 s",
                    e
                );
                interruptible_sleep(shutdown, 1000);
            }
        }
    }
}

/// Sleep for up to `ms` milliseconds, checking the shutdown flag every
/// `POLL_TIMEOUT_MS` so we stay responsive to cancellation.
fn interruptible_sleep(shutdown: &AtomicBool, ms: u64) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(ms);
    while std::time::Instant::now() < deadline {
        if shutdown.load(Ordering::Acquire) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(POLL_TIMEOUT_MS as u64));
    }
}

// ---------------------------------------------------------------------------
// MessageRecord builders
// ---------------------------------------------------------------------------

/// Build the record for an uplink and decide its fate.  `gateway` is the
/// gateway that received it, when known.
fn build_uplink_record(
    frame: &gw::UplinkFrame,
    filters: &[FilterRule],
    gateway: Option<&Eui64>,
) -> (MessageRecord, Verdict) {
    let (freq_mhz, datarate, codr) = extract_uplink_rf(frame);
    let (rssi, snr, crc) = extract_rx_info(frame);
    let raw_size = frame.phy_payload.len() as u16;

    let decoded = decoder::decode_phy_bytes(&frame.phy_payload, crc.as_ref());
    let fields = frame_fields(decoded, raw_size, crc.as_ref());
    let verdict = classify(false, filters, &Direction::Up, fields.rule_mtype(), gateway);
    let FrameFields {
        mtype,
        dev_addr,
        dev_eui,
        join_eui,
        dev_nonce,
        fcnt,
        fport,
        fctrl,
        payload_size,
        malformed,
    } = fields;

    let toa = toa::compute_lora_toa(payload_size, &datarate, &codr);

    let record = MessageRecord {
        id: MessageRecord::new_id(),
        timestamp: Utc::now(),
        direction: Direction::Up,
        status: verdict.status(),
        blocked_by: verdict.blocked_by().map(str::to_owned),
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
        gateway_eui: gateway.map(|g| g.to_string()),
        join_eui,
        dev_nonce,
        adr: fctrl.map(|c| c.adr),
        adr_ack_req: fctrl.and_then(|c| c.adr_ack_req),
        ack: fctrl.map(|c| c.ack),
        class_b: fctrl.and_then(|c| c.class_b),
        f_pending: fctrl.and_then(|c| c.f_pending),
        f_opts_len: fctrl.map(|c| c.f_opts_len),
        malformed,
    };
    (record, verdict)
}

/// Build the record for a downlink and decide its fate.  `gateway` is the
/// gateway that will transmit it, when known.
fn build_downlink_record(
    frame: &gw::DownlinkFrame,
    filters: &[FilterRule],
    gateway: Option<&Eui64>,
) -> (MessageRecord, Verdict) {
    let first = frame.items.first();
    let phy = first.map(|i| i.phy_payload.as_slice()).unwrap_or(&[]);
    let raw_size = phy.len() as u16;

    let (freq_mhz, datarate, codr) = extract_downlink_rf(frame);
    // A downlink has no CRC to report.
    let decoded = decoder::decode_phy_bytes(phy, None);
    let fields = frame_fields(decoded, raw_size, None);
    let verdict = classify(
        false,
        filters,
        &Direction::Down,
        fields.rule_mtype(),
        gateway,
    );
    let FrameFields {
        mtype,
        dev_addr,
        dev_eui,
        join_eui,
        dev_nonce,
        fcnt,
        fport,
        fctrl,
        payload_size,
        malformed,
    } = fields;

    let toa = toa::compute_lora_toa(payload_size, &datarate, &codr);

    let record = MessageRecord {
        id: MessageRecord::new_id(),
        timestamp: Utc::now(),
        direction: Direction::Down,
        status: verdict.status(),
        blocked_by: verdict.blocked_by().map(str::to_owned),
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
        gateway_eui: gateway.map(|g| g.to_string()),
        join_eui,
        dev_nonce,
        adr: fctrl.map(|c| c.adr),
        adr_ack_req: fctrl.and_then(|c| c.adr_ack_req),
        ack: fctrl.map(|c| c.ack),
        class_b: fctrl.and_then(|c| c.class_b),
        f_pending: fctrl.and_then(|c| c.f_pending),
        f_opts_len: fctrl.map(|c| c.f_opts_len),
        malformed,
    };
    (record, verdict)
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
fn extract_rx_info(frame: &gw::UplinkFrame) -> (Option<i16>, Option<f32>, Option<CrcStatus>) {
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

// ---------------------------------------------------------------------------
// IPC socket permissions
// ---------------------------------------------------------------------------

/// Make an IPC socket file world-accessible (0o777) so that processes running
/// as a different user (e.g. `nobody` in chirpstack-mqtt-forwarder) can
/// connect.  Unix domain socket `connect()` requires write permission on the
/// socket file.
fn chmod_ipc_socket(zmq_url: &str) {
    use std::os::unix::fs::PermissionsExt;
    if let Some(path) = zmq_url.strip_prefix("ipc://") {
        if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o777)) {
            warn!("Failed to chmod IPC socket {}: {}", path, e);
        } else {
            debug!("IPC socket {} permissions set to 0777", path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tokio::sync::{broadcast, mpsc};

    use crate::core::metadata::{assert_body_cleared, MType, Malformed, Status};

    /// Gateway IDs as Concentratord writes them: 16 lowercase hex digits.
    const GW_ID: &str = "0016c001ff10d3f6";
    const GW_DISPLAY: &str = "00:16:C0:01:FF:10:D3:F6";
    const OTHER_ID: &str = "58a0cbfffe801234";
    const OTHER_DISPLAY: &str = "58:A0:CB:FF:FE:80:12:34";

    fn outputs() -> (Outputs, mpsc::Receiver<MessageRecord>) {
        let (storage_tx, storage_rx) = mpsc::channel(16);
        let (live_tx, _live_rx) = broadcast::channel(16);
        (Outputs::new(storage_tx, live_tx), storage_rx)
    }

    fn eui(id: &str) -> Eui64 {
        id.parse().unwrap()
    }

    fn lora(spreading_factor: u32) -> gw::Modulation {
        gw::Modulation {
            parameters: Some(gw::modulation::Parameters::Lora(gw::LoraModulationInfo {
                bandwidth: 125_000,
                spreading_factor,
                code_rate_legacy: "4/5".to_string(),
                code_rate: 0,
            })),
        }
    }

    /// A JoinRequest PHY payload.
    fn join_request_phy() -> Vec<u8> {
        let mut pkt = vec![0x00u8];
        pkt.extend_from_slice(&[0x01; 8]); // JoinEUI
        pkt.extend_from_slice(&[0x02; 8]); // DevEUI
        pkt.extend_from_slice(&[0x34, 0x12]); // DevNonce
        pkt.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]); // MIC
        pkt
    }

    /// A JoinAccept PHY payload (the body is encrypted, so only the MHDR matters).
    fn join_accept_phy() -> Vec<u8> {
        let mut pkt = vec![0x20u8];
        pkt.extend_from_slice(&[0x55; 16]);
        pkt
    }

    fn uplink_frame(gateway_id: &str) -> gw::UplinkFrame {
        gw::UplinkFrame {
            phy_payload: join_request_phy(),
            tx_info: Some(gw::UplinkTxInfo {
                frequency: 868_100_000,
                modulation: Some(lora(7)),
            }),
            rx_info: Some(gw::UplinkRxInfo {
                gateway_id: gateway_id.to_string(),
                rssi: -60,
                snr: 7.5,
                crc_status: gw::CrcStatus::CrcOk as i32,
            }),
        }
    }

    fn uplink_event(gateway_id: &str) -> Vec<u8> {
        gw::Event {
            event: Some(gw::event::Event::UplinkFrame(uplink_frame(gateway_id))),
        }
        .encode_to_vec()
    }

    fn downlink_command(gateway_id: &str) -> Vec<u8> {
        gw::Command {
            command: Some(gw::command::Command::SendDownlinkFrame(gw::DownlinkFrame {
                downlink_id: 7,
                items: vec![gw::DownlinkFrameItem {
                    phy_payload: join_accept_phy(),
                    tx_info: Some(gw::DownlinkTxInfo {
                        frequency: 868_100_000,
                        modulation: Some(lora(7)),
                    }),
                }],
                gateway_id: gateway_id.to_string(),
            })),
        }
        .encode_to_vec()
    }

    /// An UnconfirmedDataUp PHY payload from `dev_addr`: 15 bytes with an
    /// FPort and two bytes of payload.
    fn data_up_phy(dev_addr: u32) -> Vec<u8> {
        let mut pkt = vec![0x40u8];
        pkt.extend_from_slice(&dev_addr.to_le_bytes());
        pkt.push(0x00); // FCtrl
        pkt.extend_from_slice(&[0x05, 0x00]); // FCnt
        pkt.push(0x01); // FPort
        pkt.extend_from_slice(&[0x11, 0x22]); // FRMPayload
        pkt.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]); // MIC
        pkt
    }

    /// An uplink event carrying `phy_payload` with the given CRC status.
    fn uplink_event_of(phy_payload: Vec<u8>, crc: gw::CrcStatus) -> Vec<u8> {
        let mut frame = uplink_frame(GW_ID);
        frame.phy_payload = phy_payload;
        frame.rx_info.as_mut().unwrap().crc_status = crc as i32;
        gw::Event {
            event: Some(gw::event::Event::UplinkFrame(frame)),
        }
        .encode_to_vec()
    }

    /// A downlink command for `GW_ID` carrying `phy_payload`.
    fn downlink_command_of(phy_payload: Vec<u8>) -> Vec<u8> {
        let mut command = gw::Command::decode(downlink_command(GW_ID).as_slice()).unwrap();
        let Some(gw::command::Command::SendDownlinkFrame(frame)) = command.command.as_mut() else {
            unreachable!("downlink_command builds a downlink");
        };
        frame.items[0].phy_payload = phy_payload;
        command.encode_to_vec()
    }

    /// Run encoded downlink-command bytes through the same decode step the
    /// command thread uses.
    fn send_downlink(
        command: &[u8],
        filters: &[FilterRule],
        learned: &LearnedGateway,
        outputs: &Outputs,
    ) -> DownlinkAction {
        let command = gw::Command::decode(command).unwrap();
        match command.command {
            Some(gw::command::Command::SendDownlinkFrame(frame)) => {
                process_downlink(&frame, filters, learned, outputs)
            }
            other => panic!("not a downlink command: {other:?}"),
        }
    }

    fn rule(name: &str, gateways: Vec<Eui64>) -> FilterRule {
        FilterRule {
            name: name.to_string(),
            enabled: true,
            direction: None,
            suppress_ack: false,
            mtypes: vec![],
            gateways,
        }
    }

    #[test]
    fn concentratord_records_carry_gateway_id() {
        let (out, mut records) = outputs();
        let learned = LearnedGateway::default();

        // Uplink: the ID in rx_info labels the record and is remembered.
        assert!(handle_event(&uplink_event(GW_ID), &[], &learned, &out));
        let rec = records.try_recv().unwrap();
        assert_eq!(rec.direction, Direction::Up);
        assert_eq!(rec.source_protocol, Protocol::Concentratord);
        assert_eq!(rec.gateway_eui.as_deref(), Some(GW_DISPLAY));
        assert_eq!(learned.get(), Some(eui(GW_ID)));

        // Downlink naming a gateway: that ID wins over the learned one.
        let action = send_downlink(&downlink_command(OTHER_ID), &[], &learned, &out);
        assert!(matches!(action, DownlinkAction::Relay));
        let rec = records.try_recv().unwrap();
        assert_eq!(rec.direction, Direction::Down);
        assert_eq!(rec.gateway_eui.as_deref(), Some(OTHER_DISPLAY));

        // Downlink with an empty ID: falls back to the gateway learned from
        // the uplink.
        send_downlink(&downlink_command(""), &[], &learned, &out);
        let rec = records.try_recv().unwrap();
        assert_eq!(rec.gateway_eui.as_deref(), Some(GW_DISPLAY));

        assert!(records.try_recv().is_err());
    }

    #[test]
    fn downlink_without_any_known_gateway_has_none() {
        let (out, mut records) = outputs();
        let learned = LearnedGateway::default();

        send_downlink(&downlink_command(""), &[], &learned, &out);

        assert_eq!(records.try_recv().unwrap().gateway_eui, None);
    }

    #[test]
    fn uplink_without_gateway_id_uses_the_learned_one() {
        let (out, mut records) = outputs();
        let learned = LearnedGateway::default();
        learned.set(eui(GW_ID));

        assert!(handle_event(&uplink_event(""), &[], &learned, &out));

        assert_eq!(
            records.try_recv().unwrap().gateway_eui.as_deref(),
            Some(GW_DISPLAY)
        );
        // An uplink that names no gateway leaves the learned one alone.
        assert_eq!(learned.get(), Some(eui(GW_ID)));
    }

    #[test]
    fn latest_uplink_gateway_id_replaces_the_learned_one() {
        let (out, mut records) = outputs();
        let learned = LearnedGateway::default();

        handle_event(&uplink_event(GW_ID), &[], &learned, &out);
        handle_event(&uplink_event(OTHER_ID), &[], &learned, &out);

        assert_eq!(learned.get(), Some(eui(OTHER_ID)));
        records.try_recv().unwrap();
        assert_eq!(
            records.try_recv().unwrap().gateway_eui.as_deref(),
            Some(OTHER_DISPLAY)
        );
    }

    #[test]
    fn get_gateway_id_reply_is_learned() {
        let learned = LearnedGateway::default();
        let reply = gw::GetGatewayIdResponse {
            gateway_id: GW_ID.to_string(),
        };

        learn_from_gateway_id_reply(&reply.encode_to_vec(), &learned);

        assert_eq!(learned.get(), Some(eui(GW_ID)));
    }

    #[test]
    fn unusable_gateway_id_replies_are_ignored() {
        let learned = LearnedGateway::default();
        learned.set(eui(GW_ID));

        // Empty reply (the relay returns nothing on shutdown), an empty ID,
        // an ID that is not an EUI, and bytes that are not protobuf at all.
        learn_from_gateway_id_reply(&[], &learned);
        let empty_id = gw::GetGatewayIdResponse::default();
        learn_from_gateway_id_reply(&empty_id.encode_to_vec(), &learned);
        let not_an_eui = gw::GetGatewayIdResponse {
            gateway_id: "not-an-eui".to_string(),
        };
        learn_from_gateway_id_reply(&not_an_eui.encode_to_vec(), &learned);
        learn_from_gateway_id_reply(&[0xFF, 0xFF, 0xFF], &learned);

        assert_eq!(learned.get(), Some(eui(GW_ID)));
    }

    #[test]
    fn unusable_gateway_id_in_a_frame_leaves_the_record_unlabelled() {
        let (out, mut records) = outputs();
        let learned = LearnedGateway::default();

        assert!(handle_event(&uplink_event("garbage"), &[], &learned, &out));

        assert_eq!(records.try_recv().unwrap().gateway_eui, None);
        assert_eq!(learned.get(), None);
    }

    #[test]
    fn gateway_id_fields_use_the_published_tags() {
        // Hand-encoded protobuf: tag byte = (field number << 3) | 2 for a
        // string, then a one-byte length of 16, then the ID.  This pins the
        // field numbers independently of the stubs' own attributes.
        let mut id_field = vec![0x10u8];
        id_field.extend_from_slice(GW_ID.as_bytes());

        let mut rx_info = vec![0x0A]; // UplinkRxInfo.gateway_id = 1
        rx_info.extend_from_slice(&id_field);
        let decoded = gw::UplinkRxInfo::decode(rx_info.as_slice()).unwrap();
        assert_eq!(decoded.gateway_id, GW_ID);

        let mut downlink = vec![0x3A]; // DownlinkFrame.gateway_id = 7
        downlink.extend_from_slice(&id_field);
        let decoded = gw::DownlinkFrame::decode(downlink.as_slice()).unwrap();
        assert_eq!(decoded.gateway_id, GW_ID);

        let mut reply = vec![0x0A]; // GetGatewayIdResponse.gateway_id = 1
        reply.extend_from_slice(&id_field);
        let decoded = gw::GetGatewayIdResponse::decode(reply.as_slice()).unwrap();
        assert_eq!(decoded.gateway_id, GW_ID);
    }

    #[test]
    fn gateway_rule_limits_blocking_to_that_gateway() {
        let (out, mut records) = outputs();
        let learned = LearnedGateway::default();
        let rules = [rule("only-gw", vec![eui(GW_ID)])];

        // The listed gateway's traffic is blocked, in both directions.
        assert!(!handle_event(&uplink_event(GW_ID), &rules, &learned, &out));
        let rec = records.try_recv().unwrap();
        assert_eq!(rec.status, Status::Blocked);
        assert_eq!(rec.blocked_by.as_deref(), Some("only-gw"));

        let action = send_downlink(&downlink_command(GW_ID), &rules, &learned, &out);
        let DownlinkAction::Reply(ack) = action else {
            panic!("a blocked downlink must be answered locally");
        };
        let ack = gw::DownlinkTxAck::decode(ack.as_slice()).unwrap();
        assert_eq!(ack.downlink_id, 7);
        assert_eq!(ack.items.len(), 1);
        assert_eq!(records.try_recv().unwrap().status, Status::Blocked);

        // Another gateway is untouched.
        let other_uplink = uplink_event(OTHER_ID);
        assert!(handle_event(&other_uplink, &rules, &learned, &out));
        assert_eq!(records.try_recv().unwrap().status, Status::Forwarded);
        let action = send_downlink(&downlink_command(OTHER_ID), &rules, &learned, &out);
        assert!(matches!(action, DownlinkAction::Relay));
        assert_eq!(records.try_recv().unwrap().status, Status::Forwarded);
    }

    #[test]
    fn gateway_rule_does_not_match_when_the_gateway_is_unknown() {
        let (out, mut records) = outputs();
        let learned = LearnedGateway::default();
        let rules = [rule("only-gw", vec![eui(GW_ID)])];

        // No ID anywhere: the rule cannot be shown to apply, so nothing is blocked.
        let action = send_downlink(&downlink_command(""), &rules, &learned, &out);
        assert!(matches!(action, DownlinkAction::Relay));
        assert_eq!(records.try_recv().unwrap().status, Status::Forwarded);
    }

    #[test]
    fn a_bad_crc_uplink_is_recorded_as_malformed_and_still_forwarded() {
        let (out, mut records) = outputs();
        let learned = LearnedGateway::default();

        // The bytes read as a data frame from DevAddr 6874FA22, which is what
        // a failed CRC would otherwise have put in the table.
        let event = uplink_event_of(data_up_phy(0x6874FA22), gw::CrcStatus::BadCrc);
        assert!(handle_event(&event, &[], &learned, &out));

        let rec = records.try_recv().unwrap();
        assert_eq!(rec.malformed, Some(Malformed::Crc));
        assert_body_cleared(&rec);
        // What was measured about the reception stays.
        assert_eq!(rec.status, Status::Forwarded);
        assert_eq!(rec.direction, Direction::Up);
        assert_eq!(rec.mtype, MType::UnconfirmedDataUp);
        assert_eq!(rec.freq_mhz, 868.1);
        assert_eq!(rec.datarate, "SF7BW125");
        assert_eq!(rec.rssi, Some(-60));
        assert_eq!(rec.snr, Some(7.5));
        assert_eq!(rec.crc_status, Some(CrcStatus::Fail));
        assert_eq!(rec.payload_size, 15);
        assert!(rec.toa_ms.is_some_and(|t| t > 0.0), "{:?}", rec.toa_ms);
        assert_eq!(rec.gateway_eui.as_deref(), Some(GW_DISPLAY));
        assert!(records.try_recv().is_err());

        // The same bytes with a passed or absent check keep their fields.
        for crc in [gw::CrcStatus::CrcOk, gw::CrcStatus::NoCrc] {
            let event = uplink_event_of(data_up_phy(0x6874FA22), crc);
            assert!(handle_event(&event, &[], &learned, &out));
            let rec = records.try_recv().unwrap();
            assert_eq!(rec.malformed, None, "{crc:?}");
            assert_eq!(rec.dev_addr.as_deref(), Some("6874FA22"), "{crc:?}");
            assert_eq!(rec.fcnt, Some(5), "{crc:?}");
        }
    }

    #[test]
    fn a_reserved_mhdr_bit_makes_an_uplink_malformed() {
        let (out, mut records) = outputs();
        let learned = LearnedGateway::default();

        let mut phy = data_up_phy(0x6874FA22);
        phy[0] = 0x44;
        let event = uplink_event_of(phy, gw::CrcStatus::CrcOk);
        assert!(handle_event(&event, &[], &learned, &out));

        let rec = records.try_recv().unwrap();
        assert_eq!(rec.malformed, Some(Malformed::Mhdr));
        assert_body_cleared(&rec);
        assert_eq!(rec.mtype, MType::UnconfirmedDataUp);
        assert_eq!(rec.crc_status, Some(CrcStatus::Ok));
        assert_eq!(rec.payload_size, 15);
        assert_eq!(rec.status, Status::Forwarded);
    }

    #[test]
    fn a_downlink_of_the_wrong_length_is_malformed_and_relayed() {
        let (out, mut records) = outputs();
        let learned = LearnedGateway::default();

        // A data frame needs at least 12 bytes.
        let mut phy = data_up_phy(0x6874FA22);
        phy[0] = 0x60;
        phy.truncate(9);
        let command = downlink_command_of(phy);
        let action = send_downlink(&command, &[], &learned, &out);
        assert!(matches!(action, DownlinkAction::Relay));

        let rec = records.try_recv().unwrap();
        assert_eq!(rec.malformed, Some(Malformed::Length));
        assert_body_cleared(&rec);
        assert_eq!(rec.direction, Direction::Down);
        assert_eq!(rec.mtype, MType::UnconfirmedDataDown);
        // A downlink has no CRC to report.
        assert_eq!(rec.crc_status, None);
        assert_eq!(rec.payload_size, 9);
        assert_eq!(rec.status, Status::Forwarded);
    }

    #[test]
    fn type_rules_do_not_match_malformed_uplinks_or_downlinks() {
        let (out, mut records) = outputs();
        let learned = LearnedGateway::default();
        let type_rule = |name: &str, direction: Direction, mtype: MType| FilterRule {
            direction: Some(direction),
            mtypes: vec![mtype],
            ..rule(name, vec![])
        };

        // A BAD_CRC frame whose first byte reads as a JoinRequest passes a
        // rule on JoinRequests...
        let noise = uplink_event_of(vec![0x00; 20], gw::CrcStatus::BadCrc);
        let join_rule = [type_rule("no-joins", Direction::Up, MType::JoinRequest)];
        assert!(handle_event(&noise, &join_rule, &learned, &out));
        let rec = records.try_recv().unwrap();
        assert_eq!(rec.status, Status::Forwarded);
        assert_eq!(rec.blocked_by, None);
        assert_eq!(rec.mtype, MType::JoinRequest);
        assert_eq!(rec.malformed, Some(Malformed::Crc));

        // ...which does block a valid JoinRequest.
        assert!(!handle_event(
            &uplink_event(GW_ID),
            &join_rule,
            &learned,
            &out
        ));
        assert_eq!(records.try_recv().unwrap().status, Status::Blocked);

        // A rule that lists no type still blocks the noise.
        let direction_rule = [FilterRule {
            direction: Some(Direction::Up),
            ..rule("all-up", vec![])
        }];
        assert!(!handle_event(&noise, &direction_rule, &learned, &out));
        let rec = records.try_recv().unwrap();
        assert_eq!(rec.status, Status::Blocked);
        assert_eq!(rec.blocked_by.as_deref(), Some("all-up"));
        assert_eq!(rec.malformed, Some(Malformed::Crc));

        // The same holds for a downlink of the wrong length: a rule on its
        // type does not stop it, a rule on the direction does.
        let short = downlink_command_of(join_accept_phy()[..10].to_vec());
        let accept_rule = [type_rule("no-accepts", Direction::Down, MType::JoinAccept)];
        let action = send_downlink(&short, &accept_rule, &learned, &out);
        assert!(matches!(action, DownlinkAction::Relay));
        let rec = records.try_recv().unwrap();
        assert_eq!(rec.status, Status::Forwarded);
        assert_eq!(rec.malformed, Some(Malformed::Length));

        let down_rule = [FilterRule {
            direction: Some(Direction::Down),
            ..rule("all-down", vec![])
        }];
        let action = send_downlink(&short, &down_rule, &learned, &out);
        assert!(matches!(action, DownlinkAction::Reply(_)));
        assert_eq!(records.try_recv().unwrap().status, Status::Blocked);
    }
}
