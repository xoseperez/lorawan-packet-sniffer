/// Semtech UDP Packet Forwarder MITM proxy.
///
/// Protocol reference: <https://github.com/Lora-net/packet_forwarder/blob/master/PROTOCOL.TXT>
///
/// ## Message types
///
/// | ID   | Name       | Direction       |
/// |------|------------|-----------------|
/// | 0x00 | PUSH_DATA  | Gateway → LNS   |
/// | 0x01 | PUSH_ACK   | LNS → Gateway   |
/// | 0x02 | PULL_DATA  | Gateway → LNS   |
/// | 0x03 | PULL_RESP  | LNS → Gateway   |
/// | 0x04 | PULL_ACK   | LNS → Gateway   |
/// | 0x05 | TX_ACK     | Gateway → LNS   |
///
/// ## Blocking behaviour
///
/// ```text
/// suppress_ack = false (default):
///   Uplink blocked   → drop forward, send synthetic PUSH_ACK to GW
///                       (GW believes the LNS received it, no retry)
///   Downlink blocked → drop forward, send synthetic TX_ACK to LNS
///                       (LNS believes it was transmitted, no retry)
///
/// suppress_ack = true (stealth mode):
///   Uplink blocked   → drop forward, NO PUSH_ACK
///                       (GW will retry until timeout)
///   Downlink blocked → drop forward, NO TX_ACK
///                       (LNS will retry until timeout)
/// ```
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use chrono::Utc;
use serde::Deserialize;
use tokio::net::UdpSocket;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::core::decoder;
use crate::core::filter::{self, FilterRule};
use crate::core::metadata::{CrcStatus, Direction, MessageRecord, Protocol, Status};
use crate::core::toa;

use crate::config::UdpConfig;

use super::{broadcast_record, decision_to_status, log_message, send_record, unpack_frame};

// ---------------------------------------------------------------------------
// Semtech protocol constants
// ---------------------------------------------------------------------------

const PUSH_DATA: u8 = 0x00;
const PUSH_ACK: u8 = 0x01;
const PULL_DATA: u8 = 0x02;
const PULL_RESP: u8 = 0x03;
const PULL_ACK: u8 = 0x04;
const TX_ACK: u8 = 0x05;
const PROTOCOL_VERSION: u8 = 2;

/// Minimum header size: version(1) + token(2) + identifier(1).
const HDR_LEN: usize = 4;
/// Gateway EUI field length in PUSH_DATA / PULL_DATA / TX_ACK.
const EUI_LEN: usize = 8;

// ---------------------------------------------------------------------------
// Semtech JSON structs (uplink)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct PushDataPayload {
    #[serde(default)]
    rxpk: Vec<RxPk>,
    stat: Option<GwStat>,
}

#[derive(Deserialize)]
struct GwStat {
    /// Total number of radio packets received.
    #[serde(default)]
    rxnb: u32,
    /// Number of radio packets received with valid CRC.
    #[serde(default)]
    rxok: u32,
    /// Number of radio packets forwarded.
    #[serde(default)]
    rxfw: u32,
    /// Number of downlink datagrams sent.
    #[serde(default)]
    txnb: u32,
}

#[derive(Deserialize)]
struct RxPk {
    /// Centre frequency in MHz.
    freq: f64,
    /// Data rate string, e.g. `"SF7BW125"`.
    datr: String,
    /// Coding rate string, e.g. `"4/5"`.
    #[serde(default = "default_codr")]
    codr: String,
    /// RSSI in dBm.
    rssi: Option<i16>,
    /// Signal-to-noise ratio in dB.
    lsnr: Option<f32>,
    /// CRC status: 1=OK, −1=fail, 0=no CRC.
    #[serde(default)]
    stat: i8,
    /// PHY payload size in bytes.
    size: u16,
    /// Base64-encoded PHY payload.
    data: String,
}

// ---------------------------------------------------------------------------
// Semtech JSON structs (downlink)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct PullRespPayload {
    txpk: TxPk,
}

#[derive(Deserialize)]
struct TxPk {
    freq: f64,
    datr: String,
    #[serde(default = "default_codr")]
    codr: String,
    size: u16,
    data: String,
}

fn default_codr() -> String {
    "4/5".to_string()
}

// ---------------------------------------------------------------------------
// Blocking decision returned by process_* helpers
// ---------------------------------------------------------------------------

/// The outcome of evaluating filter rules against a single decoded message.
struct BlockDecision {
    /// `true` if forwarding should be suppressed (a rule matched).
    blocked: bool,
    /// `true` if the protocol ACK should also be suppressed (stealth mode).
    suppress_ack: bool,
}

impl BlockDecision {
    fn forward() -> Self {
        Self { blocked: false, suppress_ack: false }
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run the UDP MITM proxy until `cancel` is triggered.
///
/// `record_tx` receives every captured `MessageRecord` for SQLite persistence.
/// `broadcast_tx` fans out pre-serialized JSON to all connected WebSocket clients.
/// Both use non-blocking sends so a slow consumer never stalls packet forwarding.
pub async fn run(
    config: &UdpConfig,
    filters: &[FilterRule],
    record_tx: mpsc::Sender<MessageRecord>,
    broadcast_tx: broadcast::Sender<Arc<String>>,
    cancel: CancellationToken,
) -> Result<()> {
    // Gateway-facing socket — the gateway sends its packets here.
    let gw_sock = UdpSocket::bind(&config.downstream.bind).await?;
    info!("UDP downstream bound to {}", config.downstream.bind);

    // LNS-facing socket — connected to the real LNS.
    let upstream_addr = format!("{}:{}", config.upstream.server, config.upstream.port);
    let lns_sock = UdpSocket::bind("0.0.0.0:0").await?;
    lns_sock.connect(&upstream_addr).await?;
    info!("UDP upstream connected to {}", upstream_addr);

    // Token → gateway socket-address map for routing PUSH_ACK / PULL_ACK back.
    let mut token_map: HashMap<[u8; 2], SocketAddr> = HashMap::new();

    // Last-seen gateway EUI; needed to build synthetic TX_ACK packets.
    let mut gateway_eui: [u8; 8] = [0u8; 8];

    // Last-seen gateway pull address (source of PULL_DATA).
    //
    // PULL_RESP uses a *new random token* (Semtech protocol spec §3.3), so it
    // cannot be routed via token_map.  Since this is a single-gateway proxy we
    // always forward PULL_RESP to the most recently seen gateway address.
    let mut gw_pull_addr: Option<SocketAddr> = None;

    let mut gw_buf = vec![0u8; 65_536];
    let mut lns_buf = vec![0u8; 65_536];

    loop {
        tokio::select! {
            // ----------------------------------------------------------------
            // Packet from gateway → forward (or block) to LNS
            // ----------------------------------------------------------------
            result = gw_sock.recv_from(&mut gw_buf) => {
                let (len, gw_addr) = result?;
                handle_from_gateway(
                    &gw_sock,
                    &lns_sock,
                    &gw_buf[..len],
                    gw_addr,
                    filters,
                    &mut token_map,
                    &mut gateway_eui,
                    &mut gw_pull_addr,
                    &record_tx,
                    &broadcast_tx,
                )
                .await;
            }

            // ----------------------------------------------------------------
            // Packet from LNS → forward (or block) to gateway
            // ----------------------------------------------------------------
            result = lns_sock.recv(&mut lns_buf) => {
                let len = result?;
                handle_from_lns(
                    &gw_sock,
                    &lns_sock,
                    &lns_buf[..len],
                    filters,
                    &token_map,
                    &gateway_eui,
                    gw_pull_addr,
                    &record_tx,
                    &broadcast_tx,
                )
                .await;
            }

            // ----------------------------------------------------------------
            // Shutdown
            // ----------------------------------------------------------------
            _ = cancel.cancelled() => {
                info!("UDP proxy shutting down");
                break;
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Per-direction handlers
// ---------------------------------------------------------------------------

async fn handle_from_gateway(
    gw_sock: &UdpSocket,
    lns_sock: &UdpSocket,
    buf: &[u8],
    gw_addr: SocketAddr,
    filters: &[FilterRule],
    token_map: &mut HashMap<[u8; 2], SocketAddr>,
    gateway_eui: &mut [u8; 8],
    gw_pull_addr: &mut Option<SocketAddr>,
    record_tx: &mpsc::Sender<MessageRecord>,
    broadcast_tx: &broadcast::Sender<Arc<String>>,
) {
    if buf.len() < HDR_LEN {
        warn!(
            "Received undersized UDP packet from {} ({} bytes), dropping",
            gw_addr,
            buf.len()
        );
        return;
    }

    let token: [u8; 2] = [buf[1], buf[2]];
    let msg_type = buf[3];

    match msg_type {
        // ----------------------------------------------------------------
        // PUSH_DATA — uplink RF frames from gateway
        // ----------------------------------------------------------------
        PUSH_DATA => {
            if buf.len() < HDR_LEN + EUI_LEN {
                warn!("PUSH_DATA too short ({} bytes)", buf.len());
                return;
            }
            token_map.insert(token, gw_addr);
            extract_eui(&buf[HDR_LEN..HDR_LEN + EUI_LEN], gateway_eui);

            let gw_eui_str = format_eui(gateway_eui);
            let json_payload = &buf[HDR_LEN + EUI_LEN..];

            debug!(
                "PUSH_DATA  token={:02X}{:02X}  gw={} eui={}  {} B payload",
                token[0], token[1], gw_addr, gw_eui_str, json_payload.len()
            );

            let decision = process_push_data(json_payload, filters, record_tx, broadcast_tx);

            match decision {
                BlockDecision { blocked: false, .. } => {
                    // Forward transparently.
                    if let Err(e) = lns_sock.send(buf).await {
                        error!("PUSH_DATA forward error: {}", e);
                    }
                }
                BlockDecision { blocked: true, suppress_ack: false } => {
                    // Blocked, non-stealth: gateway needs an ACK or it will retry.
                    debug!("PUSH_DATA blocked (non-stealth) — sending synthetic PUSH_ACK to {}", gw_addr);
                    let ack = build_push_ack(token);
                    if let Err(e) = gw_sock.send_to(&ack, gw_addr).await {
                        error!("synthetic PUSH_ACK send error: {}", e);
                    }
                }
                BlockDecision { blocked: true, suppress_ack: true } => {
                    // Stealth: gateway is unaware the packet was intercepted.
                    debug!("PUSH_DATA blocked (stealth) — no ACK sent");
                }
            }
        }

        // ----------------------------------------------------------------
        // PULL_DATA — keepalive + poll for downlinks
        // ----------------------------------------------------------------
        PULL_DATA => {
            if buf.len() < HDR_LEN + EUI_LEN {
                warn!("PULL_DATA too short ({} bytes)", buf.len());
                return;
            }
            token_map.insert(token, gw_addr);
            *gw_pull_addr = Some(gw_addr);
            extract_eui(&buf[HDR_LEN..HDR_LEN + EUI_LEN], gateway_eui);

            let gw_eui_str = format_eui(gateway_eui);
            debug!(
                "PULL_DATA  token={:02X}{:02X}  gw={} eui={}",
                token[0], token[1], gw_addr, gw_eui_str
            );

            // Always forward PULL_DATA so LNS knows the gateway is alive.
            if let Err(e) = lns_sock.send(buf).await {
                error!("PULL_DATA forward error: {}", e);
            }
        }

        // ----------------------------------------------------------------
        // TX_ACK — gateway confirms (or reports error for) a downlink
        // ----------------------------------------------------------------
        TX_ACK => {
            if buf.len() >= HDR_LEN + EUI_LEN {
                extract_eui(&buf[HDR_LEN..HDR_LEN + EUI_LEN], gateway_eui);
            }
            debug!(
                "TX_ACK  token={:02X}{:02X}  gw={} eui={}",
                token[0], token[1], gw_addr, format_eui(gateway_eui)
            );
            if let Err(e) = lns_sock.send(buf).await {
                error!("TX_ACK forward error: {}", e);
            }
        }

        other => {
            warn!(
                "Unknown message type 0x{:02X} from {} ({} bytes), dropping",
                other, gw_addr, buf.len()
            );
        }
    }
}

async fn handle_from_lns(
    gw_sock: &UdpSocket,
    lns_sock: &UdpSocket,
    buf: &[u8],
    filters: &[FilterRule],
    token_map: &HashMap<[u8; 2], SocketAddr>,
    gateway_eui: &[u8; 8],
    gw_pull_addr: Option<SocketAddr>,
    record_tx: &mpsc::Sender<MessageRecord>,
    broadcast_tx: &broadcast::Sender<Arc<String>>,
) {
    if buf.len() < HDR_LEN {
        warn!("Received undersized UDP packet from LNS ({} bytes), dropping", buf.len());
        return;
    }

    let token: [u8; 2] = [buf[1], buf[2]];
    let msg_type = buf[3];

    // PULL_RESP uses a new random token (Semtech spec §3.3) — route via the
    // last known downstream address instead of the token map.
    let gw_addr = if msg_type == PULL_RESP {
        match gw_pull_addr {
            Some(addr) => addr,
            None => {
                warn!("PULL_RESP received but no gateway pull address known yet, dropping");
                return;
            }
        }
    } else {
        match token_map.get(&token) {
            Some(addr) => *addr,
            None => {
                warn!(
                    "No gateway address for token {:02X}{:02X} (msg_type=0x{:02X}), dropping",
                    token[0], token[1], msg_type
                );
                return;
            }
        }
    };

    match msg_type {
        // ----------------------------------------------------------------
        // PUSH_ACK — LNS acknowledges an uplink batch
        // ----------------------------------------------------------------
        PUSH_ACK => {
            debug!("PUSH_ACK  token={:02X}{:02X}  → {}", token[0], token[1], gw_addr);
            if let Err(e) = gw_sock.send_to(buf, gw_addr).await {
                error!("PUSH_ACK forward error: {}", e);
            }
        }

        // ----------------------------------------------------------------
        // PULL_ACK — LNS acknowledges a keepalive
        // ----------------------------------------------------------------
        PULL_ACK => {
            debug!("PULL_ACK  token={:02X}{:02X}  → {}", token[0], token[1], gw_addr);
            if let Err(e) = gw_sock.send_to(buf, gw_addr).await {
                error!("PULL_ACK forward error: {}", e);
            }
        }

        // ----------------------------------------------------------------
        // PULL_RESP — LNS sends a downlink to the gateway
        // ----------------------------------------------------------------
        PULL_RESP => {
            let json_payload = &buf[HDR_LEN..];
            if json_payload.is_empty() {
                warn!("PULL_RESP too short ({} bytes)", buf.len());
                return;
            }

            debug!(
                "PULL_RESP  token={:02X}{:02X}  {} B payload  → {}",
                token[0], token[1], json_payload.len(), gw_addr
            );

            let decision = process_pull_resp(json_payload, filters, record_tx, broadcast_tx);

            match decision {
                BlockDecision { blocked: false, .. } => {
                    if let Err(e) = gw_sock.send_to(buf, gw_addr).await {
                        error!("PULL_RESP forward error: {}", e);
                    }
                }
                BlockDecision { blocked: true, suppress_ack: false } => {
                    // Blocked, non-stealth: LNS needs a TX_ACK or it may retry.
                    debug!("PULL_RESP blocked (non-stealth) — sending synthetic TX_ACK to LNS");
                    let ack = build_tx_ack(token, gateway_eui);
                    if let Err(e) = lns_sock.send(&ack).await {
                        error!("synthetic TX_ACK send error: {}", e);
                    }
                }
                BlockDecision { blocked: true, suppress_ack: true } => {
                    debug!("PULL_RESP blocked (stealth) — no TX_ACK sent");
                }
            }
        }

        other => {
            warn!(
                "Unknown message type 0x{:02X} from LNS ({} bytes), dropping",
                other, buf.len()
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Payload processors — decode, evaluate filters, log, return decision
// ---------------------------------------------------------------------------

/// Decode, log, and persist all `rxpk` entries in a PUSH_DATA JSON payload.
///
/// If any entry is blocked the whole PUSH_DATA is blocked.
/// ACK is suppressed only when **all** blocked entries request stealth.
fn process_push_data(
    payload: &[u8],
    filters: &[FilterRule],
    record_tx: &mpsc::Sender<MessageRecord>,
    broadcast_tx: &broadcast::Sender<Arc<String>>,
) -> BlockDecision {
    let json: PushDataPayload = match serde_json::from_slice(payload) {
        Ok(v) => v,
        Err(e) => {
            debug!("PUSH_DATA JSON parse error: {}", e);
            return BlockDecision::forward();
        }
    };

    if let Some(stat) = &json.stat {
        debug!(
            "PUSH_DATA stat  rxnb={} rxok={} rxfw={} txnb={}",
            stat.rxnb, stat.rxok, stat.rxfw, stat.txnb
        );
    }

    if json.rxpk.is_empty() {
        return BlockDecision::forward();
    }

    let mut any_blocked = false;
    let mut all_suppress = true; // pessimistic: only true if every blocked entry wants stealth

    for rxpk in &json.rxpk {
        let (record, fd) = build_uplink_record(rxpk, filters);
        let blocked = record.status == Status::Blocked;

        if blocked {
            any_blocked = true;
            if !fd.suppress_ack {
                all_suppress = false;
            }
        }

        log_message(&record);
        broadcast_record(&record, broadcast_tx);
        send_record(record, record_tx);
    }

    if any_blocked {
        BlockDecision { blocked: true, suppress_ack: all_suppress }
    } else {
        BlockDecision::forward()
    }
}

/// Decode, log, and persist the `txpk` in a PULL_RESP JSON payload.
fn process_pull_resp(
    payload: &[u8],
    filters: &[FilterRule],
    record_tx: &mpsc::Sender<MessageRecord>,
    broadcast_tx: &broadcast::Sender<Arc<String>>,
) -> BlockDecision {
    let json: PullRespPayload = match serde_json::from_slice(payload) {
        Ok(v) => v,
        Err(e) => {
            debug!("PULL_RESP JSON parse error: {}", e);
            return BlockDecision::forward();
        }
    };

    let (record, fd) = build_downlink_record(&json.txpk, filters);
    let blocked = record.status == Status::Blocked;
    log_message(&record);
    broadcast_record(&record, broadcast_tx);

    let decision_out = if blocked {
        BlockDecision { blocked: true, suppress_ack: fd.suppress_ack }
    } else {
        BlockDecision::forward()
    };

    send_record(record, record_tx);
    decision_out
}

// ---------------------------------------------------------------------------
// MessageRecord builders
// ---------------------------------------------------------------------------

fn build_uplink_record(rxpk: &RxPk, filters: &[FilterRule]) -> (MessageRecord, filter::FilterDecision) {
    let frame = decoder::decode_phy(&rxpk.data);
    let (mtype, dev_addr, dev_eui, fcnt, fport, payload_size) = unpack_frame(frame, rxpk.size);

    let fd = filter::evaluate(filters, &Direction::Up, &mtype);
    let (status, blocked_by) = decision_to_status(&fd);

    let crc = match rxpk.stat {
        1 => Some(CrcStatus::Ok),
        -1 => Some(CrcStatus::Fail),
        0 => Some(CrcStatus::NoCrc),
        _ => None,
    };

    (MessageRecord {
        id: MessageRecord::new_id(),
        timestamp: Utc::now(),
        direction: Direction::Up,
        status,
        blocked_by,
        source_protocol: Protocol::Udp,
        freq_mhz: rxpk.freq,
        datarate: rxpk.datr.clone(),
        rssi: rxpk.rssi,
        snr: rxpk.lsnr,
        crc_status: crc,
        mtype,
        dev_addr,
        dev_eui,
        fcnt,
        fport,
        payload_size,
        toa_ms: toa::compute_lora_toa(payload_size, &rxpk.datr, &rxpk.codr),
    }, fd)
}

fn build_downlink_record(txpk: &TxPk, filters: &[FilterRule]) -> (MessageRecord, filter::FilterDecision) {
    let frame = decoder::decode_phy(&txpk.data);
    let (mtype, dev_addr, dev_eui, fcnt, fport, payload_size) = unpack_frame(frame, txpk.size);

    let fd = filter::evaluate(filters, &Direction::Down, &mtype);
    let (status, blocked_by) = decision_to_status(&fd);

    (MessageRecord {
        id: MessageRecord::new_id(),
        timestamp: Utc::now(),
        direction: Direction::Down,
        status,
        blocked_by,
        source_protocol: Protocol::Udp,
        freq_mhz: txpk.freq,
        datarate: txpk.datr.clone(),
        rssi: None,
        snr: None,
        crc_status: None,
        mtype,
        dev_addr,
        dev_eui,
        fcnt,
        fport,
        payload_size,
        toa_ms: toa::compute_lora_toa(payload_size, &txpk.datr, &txpk.codr),
    }, fd)
}

// ---------------------------------------------------------------------------
// Synthetic ACK builders
// ---------------------------------------------------------------------------

/// Build a 4-byte PUSH_ACK packet: `[version, token[0], token[1], 0x01]`.
fn build_push_ack(token: [u8; 2]) -> [u8; 4] {
    [PROTOCOL_VERSION, token[0], token[1], PUSH_ACK]
}

/// Build a TX_ACK packet pretending to be the gateway.
///
/// Format: `[version, token[0], token[1], 0x05, gw_eui(8), JSON]`
/// JSON payload: `{"txpk_ack":{"error":"NONE"}}` (success).
fn build_tx_ack(token: [u8; 2], gw_eui: &[u8; 8]) -> Vec<u8> {
    const JSON: &[u8] = b"{\"txpk_ack\":{\"error\":\"NONE\"}}";
    let mut pkt = Vec::with_capacity(HDR_LEN + EUI_LEN + JSON.len());
    pkt.push(PROTOCOL_VERSION);
    pkt.extend_from_slice(&token);
    pkt.push(TX_ACK);
    pkt.extend_from_slice(gw_eui);
    pkt.extend_from_slice(JSON);
    pkt
}

// ---------------------------------------------------------------------------
// Utilities
// ---------------------------------------------------------------------------

/// Copy EUI bytes from `src` into `dst`.
fn extract_eui(src: &[u8], dst: &mut [u8; 8]) {
    dst.copy_from_slice(&src[..EUI_LEN]);
}

/// Format a gateway EUI (big-endian, as in UDP header) as colon-separated hex.
fn format_eui(eui: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(eui.len() * 3);
    for (i, b) in eui.iter().enumerate() {
        if i > 0 {
            s.push(':');
        }
        write!(s, "{:02X}", b).ok();
    }
    s
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_ack_format() {
        let ack = build_push_ack([0xAB, 0xCD]);
        assert_eq!(ack, [PROTOCOL_VERSION, 0xAB, 0xCD, PUSH_ACK]);
    }

    #[test]
    fn tx_ack_format() {
        let eui = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];
        let pkt = build_tx_ack([0x11, 0x22], &eui);
        assert_eq!(pkt[0], PROTOCOL_VERSION);
        assert_eq!(&pkt[1..3], &[0x11, 0x22]);
        assert_eq!(pkt[3], TX_ACK);
        assert_eq!(&pkt[4..12], &eui);
        let json = std::str::from_utf8(&pkt[12..]).unwrap();
        assert!(json.contains("NONE"));
    }

    #[test]
    fn format_eui_big_endian() {
        let eui = [0xAA, 0xBB, 0xCC, 0xDD, 0x00, 0x11, 0x22, 0x33];
        assert_eq!(format_eui(&eui), "AA:BB:CC:DD:00:11:22:33");
    }
}
