use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// LoRaWAN message type decoded from MHDR.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

/// Forwarding decision for a captured message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Status {
    Forwarded,
    Blocked,
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
}

impl MessageRecord {
    pub fn new_id() -> String {
        Uuid::new_v4().to_string()
    }
}
