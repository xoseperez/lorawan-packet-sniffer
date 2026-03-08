use base64::prelude::*;

use crate::core::metadata::MType;

/// Decoded fields from a LoRaWAN PHY payload.
///
/// Only fields readable without session keys are populated; encrypted
/// portions (FRMPayload, JoinAccept body) are left as `None`.
#[derive(Debug, Default)]
pub struct DecodedFrame {
    pub mtype: MType,
    /// DevAddr as 8 uppercase hex chars (data frames).
    pub dev_addr: Option<String>,
    /// DevEUI as colon-separated uppercase hex (JoinRequest).
    pub dev_eui: Option<String>,
    /// JoinEUI / AppEUI as colon-separated uppercase hex (JoinRequest).
    pub join_eui: Option<String>,
    /// Frame counter from FHDR (16-bit value, not extended).
    pub fcnt: Option<u32>,
    /// FPort byte.
    pub fport: Option<u8>,
    /// PHY payload size in bytes.
    pub payload_size: u16,
}

impl Default for MType {
    fn default() -> Self {
        MType::Unknown
    }
}

/// Decode a base64-encoded LoRaWAN PHY payload.
///
/// Returns `None` only if the payload cannot be base64-decoded or is empty.
/// Unknown / encrypted frames still return a `DecodedFrame` with `mtype` set.
pub fn decode_phy(data_b64: &str) -> Option<DecodedFrame> {
    let bytes = BASE64_STANDARD.decode(data_b64).ok()?;
    decode_phy_bytes(&bytes)
}

/// Decode a raw (already-decoded) LoRaWAN PHY payload byte slice.
///
/// Same as [`decode_phy`] but accepts raw bytes instead of base64.
/// Used by the Concentratord proxy where `phy_payload` arrives as `Vec<u8>`.
pub fn decode_phy_bytes(bytes: &[u8]) -> Option<DecodedFrame> {
    if bytes.is_empty() {
        return None;
    }

    let mhdr = bytes[0];
    let mtype_bits = (mhdr >> 5) & 0x07;
    let mtype = match mtype_bits {
        0 => MType::JoinRequest,
        1 => MType::JoinAccept,
        2 => MType::UnconfirmedDataUp,
        3 => MType::UnconfirmedDataDown,
        4 => MType::ConfirmedDataUp,
        5 => MType::ConfirmedDataDown,
        6 => MType::RejoinRequest,
        7 => MType::Proprietary,
        _ => MType::Unknown,
    };

    let payload_size = bytes.len() as u16;

    match &mtype {
        MType::JoinRequest => decode_join_request(&bytes, mtype, payload_size),
        MType::UnconfirmedDataUp
        | MType::ConfirmedDataUp
        | MType::UnconfirmedDataDown
        | MType::ConfirmedDataDown => decode_data_frame(&bytes, mtype, payload_size),
        _ => Some(DecodedFrame {
            mtype,
            payload_size,
            ..Default::default()
        }),
    }
}

/// JoinRequest layout (19 bytes):
///   MHDR(1) + JoinEUI(8,LE) + DevEUI(8,LE) + DevNonce(2,LE) + MIC(4)
fn decode_join_request(bytes: &[u8], mtype: MType, payload_size: u16) -> Option<DecodedFrame> {
    if bytes.len() < 19 {
        return Some(DecodedFrame {
            mtype,
            payload_size,
            ..Default::default()
        });
    }
    let join_eui = format_eui_le(&bytes[1..9]);
    let dev_eui = format_eui_le(&bytes[9..17]);
    Some(DecodedFrame {
        mtype,
        dev_eui: Some(dev_eui),
        join_eui: Some(join_eui),
        payload_size,
        ..Default::default()
    })
}

/// Data frame layout:
///   MHDR(1) + DevAddr(4,LE) + FCtrl(1) + FCnt(2,LE) + FOpts(0-15) + [FPort(1)] + FRMPayload + MIC(4)
fn decode_data_frame(bytes: &[u8], mtype: MType, payload_size: u16) -> Option<DecodedFrame> {
    // Need at least MHDR + DevAddr + FCtrl + FCnt = 8 bytes
    if bytes.len() < 8 {
        return Some(DecodedFrame {
            mtype,
            payload_size,
            ..Default::default()
        });
    }

    let dev_addr_bytes: [u8; 4] = bytes[1..5].try_into().ok()?;
    let dev_addr = format!("{:08X}", u32::from_le_bytes(dev_addr_bytes));

    let fcnt = u16::from_le_bytes(bytes[6..8].try_into().ok()?) as u32;

    let fctrl = bytes[5];
    let fopts_len = (fctrl & 0x0F) as usize;

    // FPort sits immediately after FHDR (1+4+1+2+fopts_len = 8+fopts_len)
    let fport_idx = 8 + fopts_len;
    let fport = if bytes.len() > fport_idx {
        Some(bytes[fport_idx])
    } else {
        None
    };

    Some(DecodedFrame {
        mtype,
        dev_addr: Some(dev_addr),
        fcnt: Some(fcnt),
        fport,
        payload_size,
        ..Default::default()
    })
}

/// Format a little-endian EUI byte slice as colon-separated big-endian hex.
///
/// Example: bytes `[0x01, 0x02, ..., 0x08]` → `"08:07:06:05:04:03:02:01"`
fn format_eui_le(bytes: &[u8]) -> String {
    bytes
        .iter()
        .rev()
        .map(|b| format!("{:02X}", b))
        .collect::<Vec<_>>()
        .join(":")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(bytes: &[u8]) -> String {
        base64::prelude::BASE64_STANDARD.encode(bytes)
    }

    // MHDR = 0x00 → MType bits [7:5] = 000 → JoinRequest
    #[test]
    fn decode_join_request() {
        // MHDR(1) + JoinEUI(8,LE) + DevEUI(8,LE) + DevNonce(2) + MIC(4) = 23 bytes
        let mut pkt = vec![0x00u8]; // MHDR: JoinRequest
        pkt.extend_from_slice(&[0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]); // JoinEUI LE
        pkt.extend_from_slice(&[0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]); // DevEUI LE
        pkt.extend_from_slice(&[0x01, 0x00]); // DevNonce
        pkt.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]); // MIC

        let frame = decode_phy(&encode(&pkt)).unwrap();
        assert_eq!(frame.mtype, MType::JoinRequest);
        // JoinEUI LE [01,00,00,00,00,00,00,00] → BE → "00:00:00:00:00:00:00:01"
        assert_eq!(frame.join_eui.as_deref(), Some("00:00:00:00:00:00:00:01"));
        // DevEUI LE [08,07,06,05,04,03,02,01] → BE → "01:02:03:04:05:06:07:08"
        assert_eq!(frame.dev_eui.as_deref(), Some("01:02:03:04:05:06:07:08"));
        assert_eq!(frame.payload_size, 23);
    }

    // MHDR = 0x40 → MType bits [7:5] = 010 → UnconfirmedDataUp
    #[test]
    fn decode_unconfirmed_data_up() {
        // MHDR(1) + DevAddr(4,LE) + FCtrl(1) + FCnt(2,LE) + FPort(1) + FRMPayload(4) + MIC(4) = 17
        let mut pkt = vec![0x40u8]; // MHDR: UnconfirmedDataUp
        pkt.extend_from_slice(&[0xEF, 0xCD, 0xAB, 0x01]); // DevAddr LE = 0x01ABCDEF
        pkt.push(0x00); // FCtrl: no FOpts
        pkt.extend_from_slice(&[0xD2, 0x04]); // FCnt LE = 1234
        pkt.push(0x01); // FPort = 1
        pkt.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]); // FRMPayload
        pkt.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]); // MIC

        let frame = decode_phy(&encode(&pkt)).unwrap();
        assert_eq!(frame.mtype, MType::UnconfirmedDataUp);
        assert_eq!(frame.dev_addr.as_deref(), Some("01ABCDEF"));
        assert_eq!(frame.fcnt, Some(1234));
        assert_eq!(frame.fport, Some(1));
        assert_eq!(frame.payload_size, 17);
    }

    // MHDR = 0x60 → MType bits [7:5] = 011 → UnconfirmedDataDown
    #[test]
    fn decode_unconfirmed_data_down() {
        let mut pkt = vec![0x60u8];
        pkt.extend_from_slice(&[0x01, 0x02, 0x03, 0x04]); // DevAddr LE = 0x04030201
        pkt.push(0x00); // FCtrl
        pkt.extend_from_slice(&[0x05, 0x00]); // FCnt = 5
        pkt.push(0x00); // FPort = 0 (MAC commands)
        pkt.extend_from_slice(&[0x01, 0x02]); // payload
        pkt.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]); // MIC

        let frame = decode_phy(&encode(&pkt)).unwrap();
        assert_eq!(frame.mtype, MType::UnconfirmedDataDown);
        assert_eq!(frame.dev_addr.as_deref(), Some("04030201"));
        assert_eq!(frame.fcnt, Some(5));
        assert_eq!(frame.fport, Some(0));
    }

    // MHDR = 0x20 → MType bits [7:5] = 001 → JoinAccept (encrypted body)
    #[test]
    fn decode_join_accept_mtype_only() {
        let pkt = vec![0x20u8, 0x01, 0x02, 0x03, 0x04, 0x05]; // body is encrypted
        let frame = decode_phy(&encode(&pkt)).unwrap();
        assert_eq!(frame.mtype, MType::JoinAccept);
        assert!(frame.dev_addr.is_none());
        assert!(frame.dev_eui.is_none());
    }

    #[test]
    fn decode_empty_returns_none() {
        assert!(decode_phy(&encode(b"")).is_none());
    }

    #[test]
    fn decode_invalid_base64_returns_none() {
        assert!(decode_phy("not!valid~base64").is_none());
    }

    #[test]
    fn eui_le_formatting() {
        // Bytes in LE order → reversed to BE display
        let bytes = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];
        assert_eq!(format_eui_le(&bytes), "08:07:06:05:04:03:02:01");
    }
}
