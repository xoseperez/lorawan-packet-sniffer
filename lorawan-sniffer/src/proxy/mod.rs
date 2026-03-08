pub mod concentratord;
pub mod gw;
pub mod udp;

// ---------------------------------------------------------------------------
// Shared proxy utilities (used by both udp and concentratord backends)
// ---------------------------------------------------------------------------

use std::sync::Arc;

use tokio::sync::{broadcast, mpsc};

use crate::core::decoder;
use crate::core::filter;
use crate::core::metadata::{Direction, MessageRecord, MType, Status};

use crate::api;

/// Unpack a `DecodedFrame` (or `None`) into a flat tuple for record building.
pub(super) fn unpack_frame(
    frame: Option<decoder::DecodedFrame>,
    fallback_size: u16,
) -> (MType, Option<String>, Option<String>, Option<u32>, Option<u8>, u16) {
    match frame {
        Some(f) => (f.mtype, f.dev_addr, f.dev_eui, f.fcnt, f.fport, f.payload_size),
        None => (MType::Unknown, None, None, None, None, fallback_size),
    }
}

pub(super) fn decision_to_status(fd: &filter::FilterDecision) -> (Status, Option<String>) {
    match &fd.blocked_by {
        Some(name) => (Status::Blocked, Some(name.clone())),
        None => (Status::Forwarded, None),
    }
}

/// Enqueue a `MessageRecord` for persistence without blocking the proxy loop.
pub(super) fn send_record(record: MessageRecord, tx: &mpsc::Sender<MessageRecord>) {
    if let Err(e) = tx.try_send(record) {
        warn!("Storage channel full, dropping record: {}", e);
    }
}

/// Broadcast a `MessageRecord` to all connected WebSocket clients.
pub(super) fn broadcast_record(record: &MessageRecord, tx: &broadcast::Sender<Arc<String>>) {
    if tx.receiver_count() == 0 {
        return;
    }
    if let Some(json) = api::make_ws_json(record) {
        let _ = tx.send(json);
    }
}

/// Log a captured message at INFO level in a compact, structured format.
pub(super) fn log_message(rec: &MessageRecord) {
    let status_tag = match rec.status {
        Status::Forwarded => "FORWARDED",
        Status::Blocked => "BLOCKED  ",
    };
    let dir_tag = match rec.direction {
        Direction::Up => "UP  ",
        Direction::Down => "DOWN",
    };

    let device = rec
        .dev_addr
        .as_deref()
        .or(rec.dev_eui.as_deref())
        .unwrap_or("n/a");

    let fcnt_str = rec.fcnt.map(|v| format!(" fcnt={}", v)).unwrap_or_default();
    let fport_str = rec.fport.map(|v| format!(" fport={}", v)).unwrap_or_default();
    let toa_str = rec.toa_ms.map(|v| format!(" toa={:.0}ms", v)).unwrap_or_default();

    let rf_str = match rec.direction {
        Direction::Up => {
            let rssi = rec.rssi.map(|v| format!("{}", v)).unwrap_or_else(|| "n/a".to_string());
            let snr = rec.snr.map(|v| format!("{:.1}", v)).unwrap_or_else(|| "n/a".to_string());
            format!(" rssi={} snr={}", rssi, snr)
        }
        Direction::Down => String::new(),
    };

    let blocked_str = rec
        .blocked_by
        .as_deref()
        .map(|name| format!(" [blocked_by=\"{}\"]", name))
        .unwrap_or_default();

    info!(
        "[{}] {} {} {}{}{}  {:.3}MHz {} size={}{}{}",
        status_tag, dir_tag, rec.mtype, device,
        fcnt_str, fport_str,
        rec.freq_mhz, rec.datarate,
        rec.payload_size, toa_str, rf_str,
    );

    if !blocked_str.is_empty() {
        debug!("{}", blocked_str);
    }
}
