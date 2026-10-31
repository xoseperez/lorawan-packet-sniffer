//! Semtech UDP packet forwarder wire format.
//!
//! Protocol reference: <https://github.com/Lora-net/packet_forwarder/blob/master/PROTOCOL.TXT>
//!
//! Every datagram starts with a 4-byte header: protocol version, a 2-byte
//! token and a 1-byte identifier.
//!
//! | ID   | Name       | Direction       | After the header               |
//! |------|------------|-----------------|--------------------------------|
//! | 0x00 | PUSH_DATA  | Gateway → LNS   | gateway EUI (8), JSON          |
//! | 0x01 | PUSH_ACK   | LNS → Gateway   | nothing                        |
//! | 0x02 | PULL_DATA  | Gateway → LNS   | gateway EUI (8)                |
//! | 0x03 | PULL_RESP  | LNS → Gateway   | JSON                           |
//! | 0x04 | PULL_ACK   | LNS → Gateway   | nothing                        |
//! | 0x05 | TX_ACK     | Gateway → LNS   | gateway EUI (8), optional JSON |
//!
//! The token of an ACK repeats the token of the datagram it answers, but a
//! PULL_RESP carries a fresh token of the server's choosing, which the
//! gateway echoes in its TX_ACK.

use serde::Deserialize;

use crate::core::eui::Eui64;

// ---------------------------------------------------------------------------
// Semtech protocol constants
// ---------------------------------------------------------------------------

pub const PUSH_DATA: u8 = 0x00;
pub const PUSH_ACK: u8 = 0x01;
pub const PULL_DATA: u8 = 0x02;
pub const PULL_RESP: u8 = 0x03;
pub const PULL_ACK: u8 = 0x04;
pub const TX_ACK: u8 = 0x05;
pub const PROTOCOL_VERSION: u8 = 2;

/// Minimum header size: version(1) + token(2) + identifier(1).
pub const HDR_LEN: usize = 4;
/// Gateway EUI field length in PUSH_DATA / PULL_DATA / TX_ACK.
pub const EUI_LEN: usize = 8;

// ---------------------------------------------------------------------------
// Header parsing
// ---------------------------------------------------------------------------

/// The fixed part of a datagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub token: [u8; 2],
    /// The identifier byte; compare with the constants above.
    pub kind: u8,
}

impl Header {
    /// Read the header, or `None` if the datagram is shorter than one.
    pub fn parse(buf: &[u8]) -> Option<Self> {
        if buf.len() < HDR_LEN {
            return None;
        }
        Some(Self {
            token: [buf[1], buf[2]],
            kind: buf[3],
        })
    }
}

/// The gateway EUI that follows the header of PUSH_DATA, PULL_DATA and
/// TX_ACK, or `None` if the datagram is too short to hold one.
pub fn gateway_eui(buf: &[u8]) -> Option<Eui64> {
    Eui64::from_be_slice(buf.get(HDR_LEN..HDR_LEN + EUI_LEN)?)
}

// ---------------------------------------------------------------------------
// Semtech JSON structs (uplink)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct PushDataPayload {
    #[serde(default)]
    pub rxpk: Vec<RxPk>,
    pub stat: Option<GwStat>,
}

#[derive(Deserialize)]
pub struct GwStat {
    /// Total number of radio packets received.
    #[serde(default)]
    pub rxnb: u32,
    /// Number of radio packets received with valid CRC.
    #[serde(default)]
    pub rxok: u32,
    /// Number of radio packets forwarded.
    #[serde(default)]
    pub rxfw: u32,
    /// Number of downlink datagrams sent.
    #[serde(default)]
    pub txnb: u32,
}

#[derive(Deserialize)]
pub struct RxPk {
    /// Centre frequency in MHz.
    pub freq: f64,
    /// Data rate string, e.g. `"SF7BW125"`.
    pub datr: String,
    /// Coding rate string, e.g. `"4/5"`.
    #[serde(default = "default_codr")]
    pub codr: String,
    /// RSSI in dBm.
    pub rssi: Option<i16>,
    /// Signal-to-noise ratio in dB.
    pub lsnr: Option<f32>,
    /// CRC status: 1=OK, −1=fail, 0=no CRC.
    #[serde(default)]
    pub stat: i8,
    /// PHY payload size in bytes.
    pub size: u16,
    /// Base64-encoded PHY payload.
    pub data: String,
}

// ---------------------------------------------------------------------------
// Semtech JSON structs (downlink)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct PullRespPayload {
    pub txpk: TxPk,
}

#[derive(Deserialize)]
pub struct TxPk {
    pub freq: f64,
    pub datr: String,
    #[serde(default = "default_codr")]
    pub codr: String,
    pub size: u16,
    pub data: String,
}

fn default_codr() -> String {
    "4/5".to_string()
}

// ---------------------------------------------------------------------------
// Synthetic ACK builders
// ---------------------------------------------------------------------------

/// Build a 4-byte PUSH_ACK packet: `[version, token[0], token[1], 0x01]`.
pub fn build_push_ack(token: [u8; 2]) -> [u8; 4] {
    [PROTOCOL_VERSION, token[0], token[1], PUSH_ACK]
}

/// Build a 4-byte PULL_ACK packet: `[version, token[0], token[1], 0x04]`.
pub fn build_pull_ack(token: [u8; 2]) -> [u8; 4] {
    [PROTOCOL_VERSION, token[0], token[1], PULL_ACK]
}

/// Build a TX_ACK packet pretending to be the gateway.
///
/// Format: `[version, token[0], token[1], 0x05, gw_eui(8), JSON]`
/// JSON payload: `{"txpk_ack":{"error":"NONE"}}` (success).
pub fn build_tx_ack(token: [u8; 2], gw_eui: &Eui64) -> Vec<u8> {
    const JSON: &[u8] = b"{\"txpk_ack\":{\"error\":\"NONE\"}}";
    let mut pkt = Vec::with_capacity(HDR_LEN + EUI_LEN + JSON.len());
    pkt.push(PROTOCOL_VERSION);
    pkt.extend_from_slice(&token);
    pkt.push(TX_ACK);
    pkt.extend_from_slice(&gw_eui.0);
    pkt.extend_from_slice(JSON);
    pkt
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_ack_format() {
        let ack = build_push_ack([0xAB, 0xCD]);
        assert_eq!(ack, [PROTOCOL_VERSION, 0xAB, 0xCD, PUSH_ACK]);
    }

    #[test]
    fn pull_ack_format() {
        let ack = build_pull_ack([0xAB, 0xCD]);
        assert_eq!(ack, [PROTOCOL_VERSION, 0xAB, 0xCD, PULL_ACK]);
    }

    #[test]
    fn tx_ack_format() {
        let eui = Eui64([0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08]);
        let pkt = build_tx_ack([0x11, 0x22], &eui);
        assert_eq!(pkt[0], PROTOCOL_VERSION);
        assert_eq!(&pkt[1..3], &[0x11, 0x22]);
        assert_eq!(pkt[3], TX_ACK);
        assert_eq!(&pkt[4..12], &eui.0);
        let json = std::str::from_utf8(&pkt[12..]).unwrap();
        assert!(json.contains("NONE"));
    }

    #[test]
    fn header_needs_four_bytes() {
        assert_eq!(Header::parse(&[2, 0xAA]), None);
        assert_eq!(Header::parse(&[2, 0xAA, 0xBB]), None);
        assert_eq!(
            Header::parse(&[2, 0xAA, 0xBB, PULL_DATA]),
            Some(Header {
                token: [0xAA, 0xBB],
                kind: PULL_DATA
            })
        );
    }

    #[test]
    fn gateway_eui_is_read_big_endian_after_the_header() {
        let mut pkt = vec![PROTOCOL_VERSION, 0x01, 0x02, PUSH_DATA];
        pkt.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD, 0x00, 0x11, 0x22, 0x33]);
        pkt.extend_from_slice(b"{}");
        assert_eq!(
            gateway_eui(&pkt).unwrap().to_string(),
            "AA:BB:CC:DD:00:11:22:33"
        );

        // One byte short of a full EUI.
        assert_eq!(gateway_eui(&pkt[..HDR_LEN + EUI_LEN - 1]), None);
        // Header only.
        assert_eq!(gateway_eui(&pkt[..HDR_LEN]), None);
    }
}
