use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// LoRaWAN message type decoded from MHDR.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum MType {
    JoinRequest,
    JoinAccept,
    UnconfirmedDataUp,
    UnconfirmedDataDown,
    ConfirmedDataUp,
    ConfirmedDataDown,
    RejoinRequest,
    Proprietary,
    /// MHDR was present but bits do not map to a known type.
    #[default]
    Unknown,
}

impl std::fmt::Display for MType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

/// Packet direction relative to the LoRaWAN Network Server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// Gateway → LNS (uplink).
    Up,
    /// LNS → Gateway (downlink).
    Down,
}

impl std::fmt::Display for Direction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Direction::Up => write!(f, "up"),
            Direction::Down => write!(f, "down"),
        }
    }
}

/// What happened to a captured message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Status {
    Forwarded,
    Blocked,
    /// Seen but never forwarded or blocked, because the sniffer is only
    /// listening and has no upstream to relay to.
    Captured,
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

/// Source protocol that carried the captured message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Udp,
    Concentratord,
}

/// RF CRC check result as reported by the concentrator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CrcStatus {
    Ok,
    Fail,
    NoCrc,
}

/// Why a captured frame cannot be a valid LoRaWAN 1.0 / 1.1 frame.
///
/// When several reasons apply, the first in this order is the one recorded:
/// `Crc`, then `Mhdr`, then `Length`.  The lowercase name is the text used in
/// JSON and in the database.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Malformed {
    /// The gateway reported that the CRC check of the radio frame failed.
    Crc,
    /// The MHDR's RFU bits or Major version are not zero.
    Mhdr,
    /// The length does not fit the frame type.
    Length,
}

impl Malformed {
    /// The reason as text: `"crc"`, `"mhdr"` or `"length"`.
    pub fn as_str(self) -> &'static str {
        match self {
            Malformed::Crc => "crc",
            Malformed::Mhdr => "mhdr",
            Malformed::Length => "length",
        }
    }
}

/// Canonical representation of a captured LoRaWAN message.
///
/// This struct is protocol-agnostic: fields are populated from whatever
/// information is available (UDP JSON, Concentratord Protobuf, etc.).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageRecord {
    /// Unique message identifier (UUIDv4).
    pub id: String,
    /// Wall-clock capture time (UTC).
    pub timestamp: DateTime<Utc>,
    /// Packet direction (up = gateway→LNS, down = LNS→gateway).
    pub direction: Direction,
    /// Whether the packet was forwarded or blocked by a filter.
    pub status: Status,
    /// Name of the filter rule that blocked this packet, if any.
    pub blocked_by: Option<String>,
    /// Protocol that delivered this packet.
    pub source_protocol: Protocol,
    /// Centre frequency in MHz.
    pub freq_mhz: f64,
    /// Data rate string, e.g. "SF7BW125" or "50000" (FSK).
    pub datarate: String,
    /// RSSI in dBm (uplinks only).
    pub rssi: Option<i16>,
    /// Signal-to-noise ratio in dB (uplinks only, LoRa only).
    pub snr: Option<f32>,
    /// RF CRC status as reported by the concentrator.
    pub crc_status: Option<CrcStatus>,
    /// LoRaWAN MType decoded from MHDR.
    pub mtype: MType,
    /// DevAddr (hex, 8 uppercase chars), present for data frames.
    pub dev_addr: Option<String>,
    /// DevEUI (colon-separated uppercase hex), present for join frames.
    pub dev_eui: Option<String>,
    /// Frame counter (16-bit as seen in FHDR; wraps at 65535).
    pub fcnt: Option<u32>,
    /// FPort number, present when a port byte is included.
    pub fport: Option<u8>,
    /// PHY payload size in bytes.
    pub payload_size: u16,
    /// Estimated Time-on-Air in milliseconds.
    pub toa_ms: Option<f64>,
    /// EUI of the gateway that received (uplink) or transmits (downlink) the
    /// frame, colon-separated uppercase hex, when known.
    pub gateway_eui: Option<String>,
    /// JoinEUI / AppEUI (colon-separated uppercase hex), present for join requests.
    pub join_eui: Option<String>,
    /// DevNonce, present for join requests.
    pub dev_nonce: Option<u16>,
    /// FCtrl ADR flag, present for data frames.
    pub adr: Option<bool>,
    /// FCtrl ADRACKReq flag, present for uplink data frames only.
    pub adr_ack_req: Option<bool>,
    /// FCtrl ACK flag, present for data frames.
    pub ack: Option<bool>,
    /// FCtrl ClassB flag, present for uplink data frames only.
    pub class_b: Option<bool>,
    /// FCtrl FPending flag, present for downlink data frames only.
    pub f_pending: Option<bool>,
    /// FCtrl FOptsLen: size of the FOpts field in bytes, present for data frames.
    pub f_opts_len: Option<u8>,
    /// Why the frame cannot be a valid LoRaWAN frame, or `None` when it can.
    ///
    /// A malformed record keeps the type read from the first byte, the radio
    /// data, the size, the airtime, the status and the gateway.  Everything
    /// decoded from the frame body is `None`: `dev_addr`, `dev_eui`,
    /// `join_eui`, `dev_nonce`, `fcnt`, `fport`, the FCtrl flags (`adr`,
    /// `adr_ack_req`, `ack`, `class_b`, `f_pending`) and `f_opts_len`.  Bytes
    /// that are not a LoRaWAN frame would otherwise produce identifiers that
    /// belong to no device.
    pub malformed: Option<Malformed>,
}

impl MessageRecord {
    pub fn new_id() -> String {
        Uuid::new_v4().to_string()
    }
}

/// Assert that every field decoded from the frame body is `None`, which is how
/// a malformed record must look.
#[cfg(test)]
pub(crate) fn assert_body_cleared(rec: &MessageRecord) {
    assert_eq!(rec.dev_addr, None, "dev_addr");
    assert_eq!(rec.dev_eui, None, "dev_eui");
    assert_eq!(rec.join_eui, None, "join_eui");
    assert_eq!(rec.dev_nonce, None, "dev_nonce");
    assert_eq!(rec.fcnt, None, "fcnt");
    assert_eq!(rec.fport, None, "fport");
    assert_eq!(rec.adr, None, "adr");
    assert_eq!(rec.adr_ack_req, None, "adr_ack_req");
    assert_eq!(rec.ack, None, "ack");
    assert_eq!(rec.class_b, None, "class_b");
    assert_eq!(rec.f_pending, None, "f_pending");
    assert_eq!(rec.f_opts_len, None, "f_opts_len");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn record() -> MessageRecord {
        MessageRecord {
            id: MessageRecord::new_id(),
            timestamp: Utc::now(),
            direction: Direction::Up,
            status: Status::Forwarded,
            blocked_by: None,
            source_protocol: Protocol::Udp,
            freq_mhz: 868.1,
            datarate: "SF7BW125".to_string(),
            rssi: None,
            snr: None,
            crc_status: None,
            mtype: MType::JoinRequest,
            dev_addr: None,
            dev_eui: None,
            fcnt: None,
            fport: None,
            payload_size: 23,
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
            malformed: None,
        }
    }

    /// The keys every record has carried since the first release.
    const EXISTING_KEYS: [&str; 18] = [
        "id",
        "timestamp",
        "direction",
        "status",
        "blocked_by",
        "source_protocol",
        "freq_mhz",
        "datarate",
        "rssi",
        "snr",
        "crc_status",
        "mtype",
        "dev_addr",
        "dev_eui",
        "fcnt",
        "fport",
        "payload_size",
        "toa_ms",
    ];

    const NEW_KEYS: [&str; 10] = [
        "gateway_eui",
        "join_eui",
        "dev_nonce",
        "adr",
        "adr_ack_req",
        "ack",
        "class_b",
        "f_pending",
        "f_opts_len",
        "malformed",
    ];

    fn json_keys(value: &serde_json::Value) -> BTreeSet<String> {
        value.as_object().unwrap().keys().cloned().collect()
    }

    #[test]
    fn record_json_keys() {
        let expected: BTreeSet<String> = EXISTING_KEYS
            .iter()
            .chain(NEW_KEYS.iter())
            .map(|k| k.to_string())
            .collect();
        assert_eq!(expected.len(), 28);

        // Optional fields are serialized as null, never omitted, so clients
        // can rely on every key being present.
        let empty = serde_json::to_value(record()).unwrap();
        assert_eq!(json_keys(&empty), expected);
        for key in NEW_KEYS {
            assert!(empty[key].is_null(), "{key} should be null when unset");
        }

        let mut full = record();
        full.gateway_eui = Some("AA:BB:CC:DD:00:11:22:33".to_string());
        full.join_eui = Some("00:00:00:00:00:00:00:01".to_string());
        full.dev_nonce = Some(0x1234);
        full.adr = Some(true);
        full.adr_ack_req = Some(false);
        full.ack = Some(true);
        full.class_b = Some(false);
        full.f_pending = Some(true);
        full.f_opts_len = Some(5);
        full.malformed = Some(Malformed::Length);
        let full = serde_json::to_value(full).unwrap();
        assert_eq!(json_keys(&full), expected);
        assert_eq!(full["gateway_eui"], "AA:BB:CC:DD:00:11:22:33");
        assert_eq!(full["join_eui"], "00:00:00:00:00:00:00:01");
        assert_eq!(full["dev_nonce"], 0x1234);
        assert_eq!(full["adr"], true);
        assert_eq!(full["adr_ack_req"], false);
        assert_eq!(full["ack"], true);
        assert_eq!(full["class_b"], false);
        assert_eq!(full["f_pending"], true);
        assert_eq!(full["f_opts_len"], 5);
        assert_eq!(full["malformed"], "length");
    }

    #[test]
    fn malformed_serializes_as_its_lowercase_name() {
        let cases = [
            (Malformed::Crc, "crc"),
            (Malformed::Mhdr, "mhdr"),
            (Malformed::Length, "length"),
        ];
        for (reason, text) in cases {
            assert_eq!(reason.as_str(), text);

            let mut rec = record();
            rec.malformed = Some(reason);
            let json = serde_json::to_value(&rec).unwrap();
            assert_eq!(json["malformed"], text);

            // The text round-trips, so a stored or received record keeps its reason.
            let back: MessageRecord = serde_json::from_value(json).unwrap();
            assert_eq!(back.malformed, Some(reason));
        }

        let json = serde_json::to_value(record()).unwrap();
        assert!(json["malformed"].is_null());
    }

    #[test]
    fn assert_body_cleared_checks_every_body_field() {
        // The helper must fail for each of the twelve fields on its own, or a
        // test using it would silently let one of them through.
        assert_body_cleared(&record());

        type Setter = fn(&mut MessageRecord);
        let setters: [(&str, Setter); 12] = [
            ("dev_addr", |r| r.dev_addr = Some("01020304".to_string())),
            ("dev_eui", |r| {
                r.dev_eui = Some("01:02:03:04:05:06:07:08".to_string())
            }),
            ("join_eui", |r| {
                r.join_eui = Some("01:02:03:04:05:06:07:08".to_string())
            }),
            ("dev_nonce", |r| r.dev_nonce = Some(1)),
            ("fcnt", |r| r.fcnt = Some(1)),
            ("fport", |r| r.fport = Some(1)),
            ("adr", |r| r.adr = Some(false)),
            ("adr_ack_req", |r| r.adr_ack_req = Some(false)),
            ("ack", |r| r.ack = Some(false)),
            ("class_b", |r| r.class_b = Some(false)),
            ("f_pending", |r| r.f_pending = Some(false)),
            ("f_opts_len", |r| r.f_opts_len = Some(0)),
        ];
        for (name, set) in setters {
            let mut rec = record();
            set(&mut rec);
            let result = std::panic::catch_unwind(|| assert_body_cleared(&rec));
            assert!(result.is_err(), "{name} was not checked");
        }
    }

    #[test]
    fn captured_status_serializes_by_name() {
        let mut rec = record();
        rec.status = Status::Captured;
        let json = serde_json::to_value(&rec).unwrap();
        assert_eq!(json["status"], "Captured");
        assert_eq!(Status::Captured.to_string(), "Captured");
    }

    #[test]
    fn records_without_the_new_fields_still_deserialize() {
        // A JSON document from before the new fields existed.
        let mut json = serde_json::to_value(record()).unwrap();
        for key in NEW_KEYS {
            json.as_object_mut().unwrap().remove(key);
        }
        let rec: MessageRecord = serde_json::from_value(json).unwrap();
        assert_eq!(rec.gateway_eui, None);
        assert_eq!(rec.f_opts_len, None);
        assert_eq!(rec.malformed, None);
    }
}
