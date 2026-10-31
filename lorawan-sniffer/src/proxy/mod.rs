pub mod concentratord;
pub mod gw;
pub mod udp;

// ---------------------------------------------------------------------------
// Shared proxy utilities (used by both udp and concentratord backends)
// ---------------------------------------------------------------------------

use std::sync::atomic::Ordering;
use std::sync::Arc;

use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{broadcast, mpsc};

use crate::core::decoder::{self, FCtrl};
use crate::core::eui::Eui64;
use crate::core::filter::{self, FilterRule};
use crate::core::metadata::{Direction, MType, MessageRecord, Status};
use crate::core::ratelimit::RateLimitedLog;
use crate::mqtt::MqttStats;

use crate::api;

/// The header fields of a decoded frame, flattened for building a
/// [`MessageRecord`].
pub(super) struct FrameFields {
    pub mtype: MType,
    pub dev_addr: Option<String>,
    pub dev_eui: Option<String>,
    pub join_eui: Option<String>,
    pub dev_nonce: Option<u16>,
    pub fcnt: Option<u32>,
    pub fport: Option<u8>,
    pub fctrl: Option<FCtrl>,
    pub payload_size: u16,
}

/// Flatten a `DecodedFrame` into the fields a record needs.
///
/// A payload that could not be decoded at all (`None`) becomes
/// [`MType::Unknown`] with no header fields, and `fallback_size` as its size,
/// taken from the transport's own length field.
pub(super) fn frame_fields(
    frame: Option<decoder::DecodedFrame>,
    fallback_size: u16,
) -> FrameFields {
    match frame {
        Some(f) => FrameFields {
            mtype: f.mtype,
            dev_addr: f.dev_addr,
            dev_eui: f.dev_eui,
            join_eui: f.join_eui,
            dev_nonce: f.dev_nonce,
            fcnt: f.fcnt,
            fport: f.fport,
            fctrl: f.fctrl,
            payload_size: f.payload_size,
        },
        None => FrameFields {
            mtype: MType::Unknown,
            dev_addr: None,
            dev_eui: None,
            join_eui: None,
            dev_nonce: None,
            fcnt: None,
            fport: None,
            fctrl: None,
            payload_size: fallback_size,
        },
    }
}

/// What the proxy does with one message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Verdict {
    /// Pass the message on unchanged.
    Forward,
    /// A filter rule matched: do not forward the message.
    Block {
        /// Name of the rule that matched.
        rule: String,
        /// When `true`, also withhold the protocol ACK (stealth mode).
        suppress_ack: bool,
    },
    /// The proxy is only listening, so there is nothing to forward or block;
    /// the message is recorded as seen.
    Capture,
}

impl Verdict {
    /// The status a record of this message carries.
    pub(super) fn status(&self) -> Status {
        match self {
            Verdict::Forward => Status::Forwarded,
            Verdict::Block { .. } => Status::Blocked,
            Verdict::Capture => Status::Captured,
        }
    }

    /// Name of the rule that blocked the message, if one did.
    pub(super) fn blocked_by(&self) -> Option<&str> {
        match self {
            Verdict::Block { rule, .. } => Some(rule),
            Verdict::Forward | Verdict::Capture => None,
        }
    }
}

/// Decide what to do with a message.
///
/// When `listen` is set the filters are not consulted at all: a listening
/// proxy never forwards, so a rule could neither block anything nor be honored
/// in any meaningful way, and the message is simply captured.  Otherwise the
/// first enabled rule matching `direction`, `mtype` and `gateway` blocks it,
/// and no match means forward.  `gateway` is `None` when it is not known.
pub(super) fn classify(
    listen: bool,
    filters: &[FilterRule],
    direction: &Direction,
    mtype: &MType,
    gateway: Option<&Eui64>,
) -> Verdict {
    if listen {
        return Verdict::Capture;
    }
    let decision = filter::evaluate(filters, direction, mtype, gateway);
    match decision.blocked_by {
        Some(rule) => Verdict::Block {
            rule,
            suppress_ack: decision.suppress_ack,
        },
        None => Verdict::Forward,
    }
}

/// Everywhere a captured record goes.
///
/// Both backends build one `Outputs` and call [`Outputs::emit`] for every
/// message.  `emit` is a plain synchronous function and every hand-off inside
/// it is a non-blocking send, so the forwarding path cannot end up waiting on
/// storage, the WebSocket feed or MQTT: an output that falls behind loses
/// records, and a rate-limited warning says so.
///
/// Cloning is cheap and clones share the same queues and drop accounting, so
/// ZeroMQ threads and tokio tasks can each hold one.
#[derive(Clone)]
pub struct Outputs {
    storage: mpsc::Sender<MessageRecord>,
    live: broadcast::Sender<Arc<String>>,
    mqtt: Option<MqttSink>,
    /// Rate limit for the storage drop warning.  Each output has its own, so
    /// a flood of one kind of drop cannot hide the other.
    storage_drops: Arc<RateLimitedLog>,
}

/// The MQTT side of [`Outputs`]: the queue to the publisher plus the
/// counters describing what happened to records offered to it.
#[derive(Clone)]
struct MqttSink {
    tx: mpsc::Sender<MessageRecord>,
    stats: Arc<MqttStats>,
    drops: Arc<RateLimitedLog>,
}

impl Outputs {
    /// Outputs for the SQLite writer queue and the WebSocket broadcast
    /// channel.  MQTT is off until [`Outputs::with_mqtt`] adds it.
    pub fn new(storage: mpsc::Sender<MessageRecord>, live: broadcast::Sender<Arc<String>>) -> Self {
        Self {
            storage,
            live,
            mqtt: None,
            storage_drops: Arc::new(RateLimitedLog::new()),
        }
    }

    /// Also queue every record for the MQTT publisher.  `stats.dropped` is
    /// incremented for each record the queue refuses.
    pub fn with_mqtt(self, tx: mpsc::Sender<MessageRecord>, stats: Arc<MqttStats>) -> Self {
        Self {
            mqtt: Some(MqttSink {
                tx,
                stats,
                drops: Arc::new(RateLimitedLog::new()),
            }),
            ..self
        }
    }

    /// Log the record, broadcast it to WebSocket clients, queue it for MQTT
    /// (when configured) and queue it for storage.
    ///
    /// Never waits.  A full or closed queue drops the record, with a
    /// rate-limited warning, and the other outputs are unaffected.
    pub fn emit(&self, record: MessageRecord) {
        log_message(&record);

        // Serialising costs real time, so skip it when nobody is listening.
        if self.live.receiver_count() > 0 {
            if let Some(json) = api::make_ws_json(&record) {
                // `send` only fails when every receiver went away in the
                // meantime, which loses nothing.
                let _ = self.live.send(json);
            }
        }

        if let Some(mqtt) = &self.mqtt {
            if !try_queue(&mqtt.tx, record.clone(), "MQTT", &mqtt.drops) {
                mqtt.stats.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }

        try_queue(&self.storage, record, "Storage", &self.storage_drops);
    }
}

/// Queue `record` on `tx` without waiting.  Returns `false`, after a
/// rate-limited log line, when the record was dropped.
///
/// A full queue means the consumer is slow, which can clear.  A closed queue
/// means the consumer is gone for good, so it is logged as an error.
fn try_queue(
    tx: &mpsc::Sender<MessageRecord>,
    record: MessageRecord,
    output: &str,
    drops: &RateLimitedLog,
) -> bool {
    match tx.try_send(record) {
        Ok(()) => true,
        Err(TrySendError::Full(_)) => {
            drops.warn(format_args!("{output} queue full, dropping record"));
            false
        }
        Err(TrySendError::Closed(_)) => {
            drops.error(format_args!("{output} queue closed, dropping record"));
            false
        }
    }
}

/// Log a captured message at DEBUG level in a compact, structured format.
///
/// One line per packet is too much for INFO: on a server in front of many
/// gateways it would fill the journal. The database, the WebUI and MQTT
/// already record every message.
pub(super) fn log_message(rec: &MessageRecord) {
    debug!("{}", log_line(rec));

    if let Some(name) = rec.blocked_by.as_deref() {
        debug!("[blocked_by=\"{}\"]", name);
    }
}

/// The INFO line for a record.
fn log_line(rec: &MessageRecord) -> String {
    let status_tag = match rec.status {
        Status::Forwarded => "FORWARDED",
        Status::Blocked => "BLOCKED  ",
        Status::Captured => "CAPTURED ",
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
    let gateway = rec.gateway_eui.as_deref().unwrap_or("n/a");

    let fcnt_str = rec.fcnt.map(|v| format!(" fcnt={}", v)).unwrap_or_default();
    let fport_str = rec
        .fport
        .map(|v| format!(" fport={}", v))
        .unwrap_or_default();
    let toa_str = rec
        .toa_ms
        .map(|v| format!(" toa={:.0}ms", v))
        .unwrap_or_default();

    let rf_str = match rec.direction {
        Direction::Up => {
            let rssi = rec
                .rssi
                .map(|v| format!("{}", v))
                .unwrap_or_else(|| "n/a".to_string());
            let snr = rec
                .snr
                .map(|v| format!("{:.1}", v))
                .unwrap_or_else(|| "n/a".to_string());
            format!(" rssi={} snr={}", rssi, snr)
        }
        Direction::Down => String::new(),
    };

    format!(
        "[{}] {} {} {} gw={}{}{}  {:.3}MHz {} size={}{}{}",
        status_tag,
        dir_tag,
        rec.mtype,
        device,
        gateway,
        fcnt_str,
        fport_str,
        rec.freq_mhz,
        rec.datarate,
        rec.payload_size,
        toa_str,
        rf_str,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// JoinRequest with JoinEUI 0x0000000000000001, DevEUI 0102030405060708
    /// and DevNonce 0x1234 (all little-endian on the wire).
    fn join_request() -> Vec<u8> {
        let mut pkt = vec![0x00u8];
        pkt.extend_from_slice(&[0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
        pkt.extend_from_slice(&[0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]);
        pkt.extend_from_slice(&[0x34, 0x12]);
        pkt.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]);
        pkt
    }

    #[test]
    fn frame_fields_carry_join_fields() {
        let frame = decoder::decode_phy_bytes(&join_request());
        let fields = frame_fields(frame, 0);

        assert_eq!(fields.mtype, MType::JoinRequest);
        assert_eq!(fields.join_eui.as_deref(), Some("00:00:00:00:00:00:00:01"));
        assert_eq!(fields.dev_eui.as_deref(), Some("01:02:03:04:05:06:07:08"));
        assert_eq!(fields.dev_nonce, Some(0x1234));
        assert_eq!(fields.payload_size, 23);
        // Join requests have no frame header.
        assert_eq!(fields.dev_addr, None);
        assert_eq!(fields.fcnt, None);
        assert_eq!(fields.fport, None);
        assert_eq!(fields.fctrl, None);
    }

    #[test]
    fn frame_fields_carry_data_frame_flags() {
        // UnconfirmedDataUp: ADR + ACK set, 2 bytes of FOpts, then FPort 9.
        let mut pkt = vec![0x40u8, 0xEF, 0xCD, 0xAB, 0x01, 0xA2, 0x05, 0x00];
        pkt.extend_from_slice(&[0x02, 0x01]); // FOpts
        pkt.push(0x09); // FPort
        pkt.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]); // MIC
        let fields = frame_fields(decoder::decode_phy_bytes(&pkt), 0);

        assert_eq!(fields.mtype, MType::UnconfirmedDataUp);
        assert_eq!(fields.dev_addr.as_deref(), Some("01ABCDEF"));
        assert_eq!(fields.fcnt, Some(5));
        assert_eq!(fields.fport, Some(9));
        assert_eq!(fields.dev_nonce, None);
        assert_eq!(fields.join_eui, None);
        let fctrl = fields.fctrl.unwrap();
        assert!(fctrl.adr);
        assert!(fctrl.ack);
        assert_eq!(fctrl.f_opts_len, 2);
        assert_eq!(fctrl.adr_ack_req, Some(false));
        assert_eq!(fctrl.class_b, Some(false));
        assert_eq!(fctrl.f_pending, None);
    }

    #[test]
    fn undecodable_payload_uses_the_transport_size() {
        let fields = frame_fields(None, 42);
        assert_eq!(fields.mtype, MType::Unknown);
        assert_eq!(fields.payload_size, 42);
        assert_eq!(fields.dev_addr, None);
        assert_eq!(fields.dev_eui, None);
        assert_eq!(fields.join_eui, None);
        assert_eq!(fields.dev_nonce, None);
        assert_eq!(fields.fctrl, None);
    }

    // --- classify / Verdict ---

    const GW_A: Eui64 = Eui64([0xAA, 0xBB, 0xCC, 0xDD, 0x00, 0x11, 0x22, 0x33]);
    const GW_B: Eui64 = Eui64([0x58, 0xA0, 0xCB, 0xFF, 0xFE, 0x80, 0x12, 0x34]);

    fn rule(name: &str, suppress_ack: bool, gateways: Vec<Eui64>) -> FilterRule {
        FilterRule {
            name: name.to_string(),
            enabled: true,
            direction: None,
            suppress_ack,
            mtypes: vec![],
            gateways,
        }
    }

    #[test]
    fn matching_rule_blocks_with_its_name_and_stealth_flag() {
        let rules = [rule("quiet", true, vec![]), rule("loud", false, vec![])];
        let verdict = classify(false, &rules, &Direction::Up, &MType::JoinRequest, None);
        assert_eq!(
            verdict,
            Verdict::Block {
                rule: "quiet".to_string(),
                suppress_ack: true
            }
        );
        assert_eq!(verdict.status(), Status::Blocked);
        assert_eq!(verdict.blocked_by(), Some("quiet"));
    }

    #[test]
    fn no_matching_rule_forwards() {
        let verdict = classify(false, &[], &Direction::Up, &MType::JoinRequest, None);
        assert_eq!(verdict, Verdict::Forward);
        assert_eq!(verdict.status(), Status::Forwarded);
        assert_eq!(verdict.blocked_by(), None);
    }

    #[test]
    fn listen_mode_captures_without_evaluating_filters() {
        // A rule that would block everything must not matter when listening.
        let rules = [rule("block-all", false, vec![])];
        let (dir, mtype) = (Direction::Up, MType::JoinRequest);
        let verdict = classify(true, &rules, &dir, &mtype, Some(&GW_A));
        assert_eq!(verdict, Verdict::Capture);
        assert_eq!(verdict.status(), Status::Captured);
        assert_eq!(verdict.blocked_by(), None);
    }

    #[test]
    fn classify_passes_the_gateway_to_the_filters() {
        let rules = [rule("only-a", false, vec![GW_A])];
        let blocks = |gw: Option<&Eui64>| {
            matches!(
                classify(false, &rules, &Direction::Down, &MType::JoinAccept, gw),
                Verdict::Block { .. }
            )
        };
        assert!(blocks(Some(&GW_A)));
        assert!(!blocks(Some(&GW_B)));
        assert!(!blocks(None));
    }

    // --- log line ---

    #[test]
    fn log_line_names_the_gateway() {
        let mut rec = test_record();
        rec.gateway_eui = Some(GW_A.to_string());
        let line = log_line(&rec);
        assert!(line.contains("gw=AA:BB:CC:DD:00:11:22:33"), "{line}");
        assert!(line.starts_with("[FORWARDED] UP"), "{line}");

        rec.gateway_eui = None;
        assert!(log_line(&rec).contains("gw=n/a"));
    }

    // --- Outputs ---

    fn test_record() -> MessageRecord {
        MessageRecord {
            id: MessageRecord::new_id(),
            timestamp: chrono::Utc::now(),
            direction: Direction::Up,
            status: Status::Forwarded,
            blocked_by: None,
            source_protocol: crate::core::metadata::Protocol::Udp,
            freq_mhz: 868.1,
            datarate: "SF7BW125".to_string(),
            rssi: Some(-80),
            snr: Some(7.5),
            crc_status: None,
            mtype: MType::UnconfirmedDataUp,
            dev_addr: Some("01ABCDEF".to_string()),
            dev_eui: None,
            fcnt: Some(1),
            fport: Some(1),
            payload_size: 17,
            toa_ms: None,
            gateway_eui: None,
            join_eui: None,
            dev_nonce: None,
            adr: None,
            adr_ack_req: None,
            ack: None,
            class_b: None,
            f_pending: None,
            f_opts_len: None,
        }
    }

    /// `(lines logged, messages suppressed)` of a drop-warning limiter.
    fn drops(limiter: &RateLimitedLog) -> (u64, u64) {
        (limiter.logged(), limiter.suppressed())
    }

    /// Outputs with a storage queue of `storage_cap` and a subscribed
    /// WebSocket channel; the receivers are returned so they stay alive.
    fn outputs(
        storage_cap: usize,
    ) -> (
        Outputs,
        mpsc::Receiver<MessageRecord>,
        broadcast::Receiver<Arc<String>>,
    ) {
        let (storage_tx, storage_rx) = mpsc::channel(storage_cap);
        let (live_tx, live_rx) = broadcast::channel(256);
        (Outputs::new(storage_tx, live_tx), storage_rx, live_rx)
    }

    #[test]
    fn emit_reaches_storage_and_live_feed() {
        let (out, mut storage_rx, mut live_rx) = outputs(8);
        let record = test_record();
        let id = record.id.clone();

        out.emit(record);

        assert_eq!(storage_rx.try_recv().unwrap().id, id);
        let json: serde_json::Value = serde_json::from_str(&live_rx.try_recv().unwrap()).unwrap();
        assert_eq!(json["type"], "message");
        assert_eq!(json["data"]["id"], id.as_str());
        assert_eq!(drops(&out.storage_drops), (0, 0));
    }

    #[test]
    fn emit_without_live_listeners_still_stores() {
        let (storage_tx, mut storage_rx) = mpsc::channel(8);
        let (live_tx, live_rx) = broadcast::channel(8);
        drop(live_rx);
        let out = Outputs::new(storage_tx, live_tx);

        out.emit(test_record());

        assert!(storage_rx.try_recv().is_ok());
    }

    #[test]
    fn full_storage_queue_drops_without_blocking() {
        // `emit` is synchronous, so returning at all shows it did not wait.
        let (out, mut storage_rx, mut live_rx) = outputs(1);
        let first = test_record();
        let first_id = first.id.clone();

        out.emit(first);
        for _ in 0..50 {
            out.emit(test_record());
        }

        // The queued record survives; the 50 that did not fit were dropped,
        // and only one warning was let through for them.
        assert_eq!(storage_rx.try_recv().unwrap().id, first_id);
        assert!(storage_rx.try_recv().is_err());
        assert_eq!(drops(&out.storage_drops), (1, 49));

        // A slow database must not cost the WebSocket feed anything.
        let mut live = 0;
        while live_rx.try_recv().is_ok() {
            live += 1;
        }
        assert_eq!(live, 51);
    }

    #[test]
    fn closed_storage_queue_drops_and_is_reported() {
        let (out, storage_rx, mut live_rx) = outputs(4);
        drop(storage_rx);

        out.emit(test_record());
        out.emit(test_record());

        assert_eq!(drops(&out.storage_drops), (1, 1));
        assert!(live_rx.try_recv().is_ok());
    }

    #[test]
    fn mqtt_receives_every_record_when_healthy() {
        let (out, mut storage_rx, _live_rx) = outputs(8);
        let (mqtt_tx, mut mqtt_rx) = mpsc::channel(8);
        let stats = Arc::new(MqttStats::default());
        let out = out.with_mqtt(mqtt_tx, Arc::clone(&stats));

        let a = test_record();
        let b = test_record();
        let (a_id, b_id) = (a.id.clone(), b.id.clone());
        out.emit(a);
        out.emit(b);

        assert_eq!(mqtt_rx.try_recv().unwrap().id, a_id);
        assert_eq!(mqtt_rx.try_recv().unwrap().id, b_id);
        assert_eq!(stats.dropped.load(Ordering::Relaxed), 0);
        assert_eq!(storage_rx.try_recv().unwrap().id, a_id);
        assert_eq!(storage_rx.try_recv().unwrap().id, b_id);
    }

    #[test]
    fn full_mqtt_queue_counts_drops_and_spares_the_other_outputs() {
        let (out, mut storage_rx, mut live_rx) = outputs(16);
        let (mqtt_tx, mut mqtt_rx) = mpsc::channel(1);
        let stats = Arc::new(MqttStats::default());
        let out = out.with_mqtt(mqtt_tx, Arc::clone(&stats));

        for _ in 0..5 {
            out.emit(test_record());
        }

        // One record fit; the other four were dropped and counted.
        assert!(mqtt_rx.try_recv().is_ok());
        assert!(mqtt_rx.try_recv().is_err());
        assert_eq!(stats.dropped.load(Ordering::Relaxed), 4);
        assert_eq!(drops(&out.mqtt.as_ref().unwrap().drops), (1, 3));

        // Storage and the live feed saw all five, with no warnings of their own.
        for _ in 0..5 {
            assert!(storage_rx.try_recv().is_ok());
            assert!(live_rx.try_recv().is_ok());
        }
        assert_eq!(out.storage_drops.logged(), 0);
    }

    #[test]
    fn closed_mqtt_queue_counts_drops() {
        let (out, mut storage_rx, _live_rx) = outputs(8);
        let (mqtt_tx, mqtt_rx) = mpsc::channel(8);
        drop(mqtt_rx);
        let stats = Arc::new(MqttStats::default());
        let out = out.with_mqtt(mqtt_tx, Arc::clone(&stats));

        out.emit(test_record());
        out.emit(test_record());

        assert_eq!(stats.dropped.load(Ordering::Relaxed), 2);
        assert!(storage_rx.try_recv().is_ok());
        assert!(storage_rx.try_recv().is_ok());
    }

    #[test]
    fn drops_are_rate_limited_per_output() {
        let (out, _storage_rx, _live_rx) = outputs(1);
        let (mqtt_tx, _mqtt_rx) = mpsc::channel(1);
        let out = out.with_mqtt(mqtt_tx, Arc::new(MqttStats::default()));

        for _ in 0..10 {
            out.emit(test_record());
        }

        // Each output keeps its own budget: one line each, not one in total.
        assert_eq!(drops(&out.storage_drops), (1, 8));
        assert_eq!(drops(&out.mqtt.as_ref().unwrap().drops), (1, 8));
    }

    #[test]
    fn clones_share_queues_and_drop_accounting() {
        let (out, mut storage_rx, _live_rx) = outputs(1);
        let other = out.clone();

        out.emit(test_record());
        other.emit(test_record());
        other.emit(test_record());

        assert!(storage_rx.try_recv().is_ok());
        assert!(storage_rx.try_recv().is_err());
        assert_eq!(drops(&out.storage_drops), (1, 1));
    }
}
