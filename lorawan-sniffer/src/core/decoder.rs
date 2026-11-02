use base64::prelude::*;

use crate::core::eui::Eui64;
use crate::core::metadata::{CrcStatus, MType, Malformed};

/// Frame control byte (FCtrl) of a data frame.
///
/// The top bits of FCtrl mean different things in each direction, so the
/// decoder resolves them from the MType and only fills in the flags that exist
/// for that direction.  The others stay `None` rather than `false`, so "not
/// applicable" and "bit clear" remain distinguishable downstream.
///
/// | Bit | Uplink    | Downlink                 |
/// |-----|-----------|--------------------------|
/// | 7   | ADR       | ADR                      |
/// | 6   | ADRACKReq | reserved (always `None`) |
/// | 5   | ACK       | ACK                      |
/// | 4   | ClassB    | FPending                 |
/// | 3:0 | FOptsLen  | FOptsLen                 |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FCtrl {
    /// Adaptive data rate enabled (bit 7, both directions).
    pub adr: bool,
    /// Device asks the network to confirm its ADR state (bit 6, uplink only).
    pub adr_ack_req: Option<bool>,
    /// Acknowledges the previous confirmed message (bit 5, both directions).
    pub ack: bool,
    /// Device is in Class B mode (bit 4, uplink only).
    pub class_b: Option<bool>,
    /// Network has more downlink data queued for the device (bit 4, downlink only).
    pub f_pending: Option<bool>,
    /// Length of the FOpts field in bytes (bits 3..0).
    pub f_opts_len: u8,
}

impl FCtrl {
    fn parse(byte: u8, uplink: bool) -> Self {
        let bit = |n: u8| byte & (1 << n) != 0;
        Self {
            adr: bit(7),
            adr_ack_req: uplink.then(|| bit(6)),
            ack: bit(5),
            class_b: uplink.then(|| bit(4)),
            f_pending: (!uplink).then(|| bit(4)),
            f_opts_len: byte & 0x0F,
        }
    }
}

/// Decoded fields from a LoRaWAN PHY payload.
///
/// Only fields readable without session keys are populated; encrypted
/// portions (FRMPayload, JoinAccept body) are left as `None`.
///
/// A malformed frame (`malformed` is `Some`) carries only `mtype`,
/// `payload_size` and the reason: its bytes are not a LoRaWAN frame, so
/// anything read from them would be an identifier that belongs to no device.
#[derive(Debug, Default)]
pub struct DecodedFrame {
    pub mtype: MType,
    /// DevAddr as 8 uppercase hex chars (data frames).
    pub dev_addr: Option<String>,
    /// DevEUI as colon-separated uppercase hex (JoinRequest).
    pub dev_eui: Option<String>,
    /// JoinEUI / AppEUI as colon-separated uppercase hex (JoinRequest).
    pub join_eui: Option<String>,
    /// DevNonce (JoinRequest).
    pub dev_nonce: Option<u16>,
    /// Frame counter from FHDR (16-bit value, not extended).
    pub fcnt: Option<u32>,
    /// FPort byte.
    pub fport: Option<u8>,
    /// Frame control flags (data frames).
    pub fctrl: Option<FCtrl>,
    /// PHY payload size in bytes.
    pub payload_size: u16,
    /// Why the frame cannot be a valid LoRaWAN frame, if it cannot.
    pub malformed: Option<Malformed>,
}

/// Length of a JoinRequest: MHDR + JoinEUI + DevEUI + DevNonce + MIC.
pub const JOIN_REQUEST_LEN: usize = 23;

/// The two lengths of a JoinAccept: MHDR + JoinNonce + NetID + DevAddr +
/// DLSettings + RxDelay + MIC, with or without a 16-byte CFList.
pub const JOIN_ACCEPT_LENS: [usize; 2] = [17, 33];

/// The two lengths of a RejoinRequest: type 0 or 2 (19 bytes) and type 1
/// (24 bytes).  The RJType byte is not checked.
pub const REJOIN_REQUEST_LENS: [usize; 2] = [19, 24];

/// Shortest data frame: MHDR + DevAddr + FCtrl + FCnt + MIC, with no FOpts.
pub const DATA_FRAME_MIN_LEN: usize = 12;

/// The MHDR bits that are not the message type: RFU (4:2) and Major (1:0).
/// A LoRaWAN 1.0 or 1.1 frame has all of them at zero.
const MHDR_RFU_AND_MAJOR: u8 = 0b0001_1111;

/// Whether the gateway reported a failed CRC.
///
/// Only an explicit failure counts.  A source that does not report CRC
/// (`NoCrc`), an unknown status and a downlink (`None`) are not failures, so
/// that a source without CRC does not have all its traffic marked.
pub fn crc_failed(crc: Option<&CrcStatus>) -> bool {
    matches!(crc, Some(CrcStatus::Fail))
}

/// Whether a frame of `size` bytes has a length that fits its type.
///
/// `f_opts_len` is the FOptsLen announced in FCtrl; only the data frame types
/// use it.  Proprietary frames have no length rule, and neither does
/// `Unknown`.
pub fn length_fits(mtype: &MType, size: usize, f_opts_len: u8) -> bool {
    match mtype {
        MType::JoinRequest => size == JOIN_REQUEST_LEN,
        MType::JoinAccept => JOIN_ACCEPT_LENS.contains(&size),
        MType::RejoinRequest => REJOIN_REQUEST_LENS.contains(&size),
        MType::UnconfirmedDataUp
        | MType::UnconfirmedDataDown
        | MType::ConfirmedDataUp
        | MType::ConfirmedDataDown => size >= DATA_FRAME_MIN_LEN + f_opts_len as usize,
        MType::Proprietary | MType::Unknown => true,
    }
}

/// The reason a frame cannot be a valid LoRaWAN frame, applying the rules in
/// order: failed CRC, then MHDR, then length.  `None` means it can be.
///
/// Reads only the MHDR and, for a data frame of 6 bytes or more, the FOptsLen
/// bits of FCtrl.  A data frame shorter than 6 bytes has no FCtrl, but it is
/// under the 12-byte minimum whatever FOptsLen would have been.
fn malformed_reason(bytes: &[u8], mtype: &MType, crc: Option<&CrcStatus>) -> Option<Malformed> {
    if crc_failed(crc) {
        return Some(Malformed::Crc);
    }
    if bytes[0] & MHDR_RFU_AND_MAJOR != 0 {
        return Some(Malformed::Mhdr);
    }
    let is_data = matches!(
        mtype,
        MType::UnconfirmedDataUp
            | MType::UnconfirmedDataDown
            | MType::ConfirmedDataUp
            | MType::ConfirmedDataDown
    );
    let f_opts_len = if is_data && bytes.len() >= 6 {
        bytes[5] & 0x0F
    } else {
        0
    };
    (!length_fits(mtype, bytes.len(), f_opts_len)).then_some(Malformed::Length)
}

/// Decode a base64-encoded LoRaWAN PHY payload.
///
/// `crc` is the gateway's CRC status for the frame: `None` for a downlink or
/// when the status is unknown.
///
/// Returns `None` only if the payload cannot be base64-decoded or is empty.
/// Unknown / encrypted frames still return a `DecodedFrame` with `mtype` set.
/// A malformed frame returns its type, size and reason, and nothing parsed
/// from its body.
pub fn decode_phy(data_b64: &str, crc: Option<&CrcStatus>) -> Option<DecodedFrame> {
    let bytes = BASE64_STANDARD.decode(data_b64).ok()?;
    decode_phy_bytes(&bytes, crc)
}

/// Decode a raw (already-decoded) LoRaWAN PHY payload byte slice.
///
/// Same as [`decode_phy`] but accepts raw bytes instead of base64.
/// Used by the Concentratord proxy where `phy_payload` arrives as `Vec<u8>`.
pub fn decode_phy_bytes(bytes: &[u8], crc: Option<&CrcStatus>) -> Option<DecodedFrame> {
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

    if let Some(reason) = malformed_reason(bytes, &mtype, crc) {
        return Some(DecodedFrame {
            mtype,
            payload_size,
            malformed: Some(reason),
            ..Default::default()
        });
    }

    match &mtype {
        MType::JoinRequest => decode_join_request(bytes, mtype, payload_size),
        MType::UnconfirmedDataUp
        | MType::ConfirmedDataUp
        | MType::UnconfirmedDataDown
        | MType::ConfirmedDataDown => decode_data_frame(bytes, mtype, payload_size),
        _ => Some(DecodedFrame {
            mtype,
            payload_size,
            ..Default::default()
        }),
    }
}

/// JoinRequest layout (23 bytes):
///   MHDR(1) + JoinEUI(8,LE) + DevEUI(8,LE) + DevNonce(2,LE) + MIC(4)
///
/// Safe on a slice of any length: one shorter than a whole JoinRequest yields
/// no fields.
fn decode_join_request(bytes: &[u8], mtype: MType, payload_size: u16) -> Option<DecodedFrame> {
    if bytes.len() < JOIN_REQUEST_LEN {
        return Some(DecodedFrame {
            mtype,
            payload_size,
            ..Default::default()
        });
    }
    let join_eui = Eui64::from_le_slice(&bytes[1..9]).map(|e| e.to_string());
    let dev_eui = Eui64::from_le_slice(&bytes[9..17]).map(|e| e.to_string());
    let dev_nonce = u16::from_le_bytes([bytes[17], bytes[18]]);
    Some(DecodedFrame {
        mtype,
        dev_eui,
        join_eui,
        dev_nonce: Some(dev_nonce),
        payload_size,
        ..Default::default()
    })
}

/// Length of the message integrity code that ends every data frame.
const MIC_LEN: usize = 4;

/// Data frame layout:
///   MHDR(1) + DevAddr(4,LE) + FCtrl(1) + FCnt(2,LE) + FOpts(0-15) + [FPort(1)] + FRMPayload + MIC(4)
///
/// Safe on a slice of any length: one that cannot hold the whole frame (the
/// header, the FOpts its FCtrl announces and the MIC) yields no fields.
fn decode_data_frame(bytes: &[u8], mtype: MType, payload_size: u16) -> Option<DecodedFrame> {
    let none = || {
        Some(DecodedFrame {
            mtype: mtype.clone(),
            payload_size,
            ..Default::default()
        })
    };

    // FCtrl is byte 5; it must be there before FOptsLen can be read.
    if bytes.len() < 6 {
        return none();
    }

    // Bit meanings in FCtrl differ between uplink and downlink.
    let uplink = matches!(mtype, MType::UnconfirmedDataUp | MType::ConfirmedDataUp);
    let fctrl = FCtrl::parse(bytes[5], uplink);
    let fopts_len = fctrl.f_opts_len as usize;

    // MHDR + FHDR (DevAddr, FCtrl, FCnt, FOpts) + MIC.
    if bytes.len() < DATA_FRAME_MIN_LEN + fopts_len {
        return none();
    }

    let dev_addr_bytes: [u8; 4] = bytes[1..5].try_into().ok()?;
    let dev_addr = format!("{:08X}", u32::from_le_bytes(dev_addr_bytes));

    let fcnt = u16::from_le_bytes(bytes[6..8].try_into().ok()?) as u32;

    // FPort sits immediately after FHDR (1+4+1+2+fopts_len = 8+fopts_len), but
    // only when something lies between the FHDR and the MIC.  A frame that
    // ends right after the FHDR (a bare ACK, for instance) has no FPort, and
    // the byte at that position is the first byte of the MIC.
    let fport_idx = 8 + fopts_len;
    let fport = if bytes.len() > fport_idx + MIC_LEN {
        Some(bytes[fport_idx])
    } else {
        None
    };

    Some(DecodedFrame {
        mtype,
        dev_addr: Some(dev_addr),
        fcnt: Some(fcnt),
        fport,
        fctrl: Some(fctrl),
        payload_size,
        ..Default::default()
    })
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

        let frame = decode_phy(&encode(&pkt), None).unwrap();
        assert_eq!(frame.mtype, MType::JoinRequest);
        // JoinEUI LE [01,00,00,00,00,00,00,00] → BE → "00:00:00:00:00:00:00:01"
        assert_eq!(frame.join_eui.as_deref(), Some("00:00:00:00:00:00:00:01"));
        // DevEUI LE [08,07,06,05,04,03,02,01] → BE → "01:02:03:04:05:06:07:08"
        assert_eq!(frame.dev_eui.as_deref(), Some("01:02:03:04:05:06:07:08"));
        assert_eq!(frame.dev_nonce, Some(1));
        assert_eq!(frame.fctrl, None);
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

        let frame = decode_phy(&encode(&pkt), None).unwrap();
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

        let frame = decode_phy(&encode(&pkt), None).unwrap();
        assert_eq!(frame.mtype, MType::UnconfirmedDataDown);
        assert_eq!(frame.dev_addr.as_deref(), Some("04030201"));
        assert_eq!(frame.fcnt, Some(5));
        assert_eq!(frame.fport, Some(0));
    }

    // MHDR = 0x20 → MType bits [7:5] = 001 → JoinAccept (encrypted body)
    #[test]
    fn decode_join_accept_mtype_only() {
        // MHDR + 12 bytes of encrypted body + MIC = 17, and 33 with a CFList.
        for len in JOIN_ACCEPT_LENS {
            let mut pkt = vec![0x20u8];
            pkt.extend((1..len).map(|i| i as u8)); // body is encrypted
            let frame = decode_phy(&encode(&pkt), None).unwrap();
            assert_eq!(frame.mtype, MType::JoinAccept);
            assert_eq!(frame.payload_size as usize, len);
            assert_eq!(frame.malformed, None, "len {len}");
            assert!(frame.dev_addr.is_none());
            assert!(frame.dev_eui.is_none());
        }

        // The same bytes at any other length are not a JoinAccept.
        let short = vec![0x20u8, 0x01, 0x02, 0x03, 0x04, 0x05];
        let frame = decode_phy(&encode(&short), None).unwrap();
        assert_eq!(frame.mtype, MType::JoinAccept);
        assert_eq!(frame.malformed, Some(Malformed::Length));
    }

    #[test]
    fn decode_empty_returns_none() {
        assert!(decode_phy(&encode(b""), None).is_none());
    }

    #[test]
    fn decode_invalid_base64_returns_none() {
        assert!(decode_phy("not!valid~base64", None).is_none());
    }

    /// A well-formed data frame: FCnt 1, as many FOpts bytes as the low nibble
    /// of `fctrl` announces, FPort 1, a 2-byte payload and a MIC.
    fn data_frame(mhdr: u8, fctrl: u8) -> Vec<u8> {
        let mut pkt = vec![mhdr];
        pkt.extend_from_slice(&[0x01, 0x02, 0x03, 0x04]); // DevAddr LE
        pkt.push(fctrl);
        pkt.extend_from_slice(&[0x01, 0x00]); // FCnt
        pkt.extend(std::iter::repeat_n(0xEE, (fctrl & 0x0F) as usize)); // FOpts
        pkt.push(0x01); // FPort
        pkt.extend_from_slice(&[0xDE, 0xAD]); // FRMPayload
        pkt.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]); // MIC
        pkt
    }

    fn join_request(dev_nonce: [u8; 2]) -> Vec<u8> {
        let mut pkt = vec![0x00u8];
        pkt.extend_from_slice(&[0x11; 8]); // JoinEUI LE
        pkt.extend_from_slice(&[0x22; 8]); // DevEUI LE
        pkt.extend_from_slice(&dev_nonce);
        pkt.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]); // MIC
        pkt
    }

    fn uplink_fctrl(
        adr: bool,
        adr_ack_req: bool,
        ack: bool,
        class_b: bool,
        f_opts_len: u8,
    ) -> FCtrl {
        FCtrl {
            adr,
            adr_ack_req: Some(adr_ack_req),
            ack,
            class_b: Some(class_b),
            f_pending: None,
            f_opts_len,
        }
    }

    fn downlink_fctrl(adr: bool, ack: bool, f_pending: bool, f_opts_len: u8) -> FCtrl {
        FCtrl {
            adr,
            adr_ack_req: None,
            ack,
            class_b: None,
            f_pending: Some(f_pending),
            f_opts_len,
        }
    }

    // Uplink: bit7 ADR, bit6 ADRACKReq, bit5 ACK, bit4 ClassB.
    #[test]
    fn fctrl_uplink_bits() {
        for mhdr in [0x40u8, 0x80] {
            // UnconfirmedDataUp, ConfirmedDataUp
            let cases = [
                (0x00, uplink_fctrl(false, false, false, false, 0)),
                (0x80, uplink_fctrl(true, false, false, false, 0)),
                (0x40, uplink_fctrl(false, true, false, false, 0)),
                (0x20, uplink_fctrl(false, false, true, false, 0)),
                (0x10, uplink_fctrl(false, false, false, true, 0)),
                (0xF0, uplink_fctrl(true, true, true, true, 0)),
                (0xA5, uplink_fctrl(true, false, true, false, 5)),
            ];
            for (byte, expected) in cases {
                let frame = decode_phy_bytes(&data_frame(mhdr, byte), None).unwrap();
                assert_eq!(
                    frame.fctrl,
                    Some(expected),
                    "mhdr {mhdr:#04x} fctrl {byte:#04x}"
                );
            }
        }
    }

    // Downlink: bit7 ADR, bit6 reserved, bit5 ACK, bit4 FPending.
    #[test]
    fn fctrl_downlink_bits() {
        for mhdr in [0x60u8, 0xA0] {
            // UnconfirmedDataDown, ConfirmedDataDown
            let cases = [
                (0x00, downlink_fctrl(false, false, false, 0)),
                (0x80, downlink_fctrl(true, false, false, 0)),
                (0x20, downlink_fctrl(false, true, false, 0)),
                (0x10, downlink_fctrl(false, false, true, 0)),
                // Bit 6 is reserved downlink: it must not leak into any flag.
                (0x40, downlink_fctrl(false, false, false, 0)),
                (0xF0, downlink_fctrl(true, true, true, 0)),
                (0x93, downlink_fctrl(true, false, true, 3)),
            ];
            for (byte, expected) in cases {
                let frame = decode_phy_bytes(&data_frame(mhdr, byte), None).unwrap();
                assert_eq!(
                    frame.fctrl,
                    Some(expected),
                    "mhdr {mhdr:#04x} fctrl {byte:#04x}"
                );
            }
        }
    }

    /// `len` bytes starting with `mhdr`, the rest all `0xFF`.
    fn filled(mhdr: u8, len: usize) -> Vec<u8> {
        let mut pkt = vec![0xFF; len];
        pkt[0] = mhdr;
        pkt
    }

    #[test]
    fn non_data_frames_have_no_flags() {
        // JoinRequest, JoinAccept, RejoinRequest (both lengths), Proprietary:
        // all valid, none of them a data frame.
        let frames: [Vec<u8>; 5] = [
            join_request([0x01, 0x00]),
            filled(0x20, 17),
            filled(0xC0, 19),
            filled(0xC0, 24),
            filled(0xE0, 10),
        ];
        for pkt in frames {
            let frame = decode_phy_bytes(&pkt, None).unwrap();
            assert_eq!(frame.malformed, None, "mtype {:?}", frame.mtype);
            assert_eq!(frame.fctrl, None, "mtype {:?}", frame.mtype);
            assert_eq!(frame.fcnt, None, "mtype {:?}", frame.mtype);
            assert_eq!(frame.fport, None, "mtype {:?}", frame.mtype);
        }

        // Only a JoinRequest carries a DevNonce.
        for pkt in [filled(0x20, 17), data_frame(0x40, 0xFF)] {
            assert_eq!(decode_phy_bytes(&pkt, None).unwrap().dev_nonce, None);
        }

        // The same bytes at a length the type does not allow are malformed,
        // and carry no field at all.
        for pkt in [filled(0x20, 10), filled(0xC0, 10)] {
            let frame = decode_phy_bytes(&pkt, None).unwrap();
            assert_eq!(frame.malformed, Some(Malformed::Length));
            assert_eq!(frame.fctrl, None);
            assert_eq!(frame.fcnt, None);
            assert_eq!(frame.fport, None);
            assert_eq!(frame.dev_nonce, None);
        }
    }

    #[test]
    fn join_request_dev_nonce() {
        // DevNonce is little-endian: [0x34, 0x12] is 0x1234.
        let frame = decode_phy_bytes(&join_request([0x34, 0x12]), None).unwrap();
        assert_eq!(frame.mtype, MType::JoinRequest);
        assert_eq!(frame.dev_nonce, Some(0x1234));

        let frame = decode_phy_bytes(&join_request([0xFF, 0x00]), None).unwrap();
        assert_eq!(frame.dev_nonce, Some(0x00FF));

        // Data frames do not have one.
        assert_eq!(
            decode_phy_bytes(&data_frame(0x40, 0), None)
                .unwrap()
                .dev_nonce,
            None
        );
    }

    #[test]
    fn data_frame_fopts_len() {
        for len in 0u8..=15 {
            for (mhdr, uplink) in [(0x40u8, true), (0x60, false)] {
                let frame = decode_phy_bytes(&data_frame(mhdr, 0x80 | len), None).unwrap();
                let fctrl = frame.fctrl.unwrap();
                assert_eq!(fctrl.f_opts_len, len, "mhdr {mhdr:#04x}");
                assert!(fctrl.adr, "FOptsLen must not disturb the flag bits");
                assert_eq!(fctrl.class_b.is_some(), uplink);
                assert_eq!(fctrl.f_pending.is_some(), !uplink);
                // FPort follows the FOpts bytes, wherever they end.
                assert_eq!(frame.fport, Some(1), "mhdr {mhdr:#04x} len {len}");
                assert_eq!(frame.fcnt, Some(1));
            }
        }
    }

    #[test]
    fn frame_without_fport_has_no_fport() {
        // MHDR + DevAddr + FCtrl (ACK) + FCnt + MIC: a bare acknowledgement.
        // The byte right after the FHDR is the first MIC byte, not an FPort.
        let mut ack = vec![0x60u8];
        ack.extend_from_slice(&[0x01, 0x02, 0x03, 0x04]); // DevAddr LE
        ack.push(0x20); // FCtrl: ACK, no FOpts
        ack.extend_from_slice(&[0x07, 0x00]); // FCnt
        ack.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]); // MIC
        let frame = decode_phy_bytes(&ack, None).unwrap();
        assert_eq!(frame.fport, None);
        assert_eq!(frame.fcnt, Some(7));
        assert!(frame.fctrl.unwrap().ack);

        assert_eq!(ack.len(), 12);
        assert_eq!(frame.malformed, None);

        // The same with FOpts: they are skipped, and still no FPort follows.
        let mut with_fopts = vec![0x60u8];
        with_fopts.extend_from_slice(&[0x01, 0x02, 0x03, 0x04]);
        with_fopts.push(0x23); // FCtrl: ACK, 3 bytes of FOpts
        with_fopts.extend_from_slice(&[0x07, 0x00]);
        with_fopts.extend_from_slice(&[0x02, 0x01, 0x03]); // FOpts
        with_fopts.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]);
        assert_eq!(with_fopts.len(), 15);
        let frame = decode_phy_bytes(&with_fopts, None).unwrap();
        assert_eq!(frame.malformed, None);
        assert_eq!(frame.fport, None);
        assert_eq!(frame.fcnt, Some(7));
        assert_eq!(frame.fctrl.unwrap().f_opts_len, 3);

        // One byte more is an FPort (with no FRMPayload) and the MIC.
        let mut with_fport = ack[..8].to_vec();
        with_fport.push(0x05); // FPort
        with_fport.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]);
        assert_eq!(with_fport.len(), 13);
        let frame = decode_phy_bytes(&with_fport, None).unwrap();
        assert_eq!(frame.malformed, None);
        assert_eq!(frame.fport, Some(5));

        // Frames cut short inside or before the MIC are malformed, so no
        // FPort is read from what is left of them.
        for len in 8..ack.len() {
            let frame = decode_phy_bytes(&ack[..len], None).unwrap();
            assert_eq!(frame.malformed, Some(Malformed::Length), "len {len}");
            assert_eq!(frame.fport, None, "len {len}");
        }
    }

    #[test]
    fn frame_with_fport_and_empty_payload_reports_its_fport() {
        // MHDR + FHDR + FPort + MIC, with no FRMPayload at all.
        for fport in [0x00u8, 0x01, 0xDF, 0xFF] {
            let mut pkt = vec![0x40u8];
            pkt.extend_from_slice(&[0x01, 0x02, 0x03, 0x04]); // DevAddr LE
            pkt.push(0x00); // FCtrl
            pkt.extend_from_slice(&[0x01, 0x00]); // FCnt
            pkt.push(fport);
            pkt.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]); // MIC
            let frame = decode_phy_bytes(&pkt, None).unwrap();
            assert_eq!(frame.fport, Some(fport));
            assert_eq!(frame.payload_size, 13);
        }
    }

    #[test]
    fn truncated_frames_have_no_header_fields() {
        // A data frame needs 12 bytes plus the FOpts its FCtrl announces: with
        // FOptsLen 15 that is 27.
        for mhdr in [0x40u8, 0x60, 0x80, 0xA0] {
            let full = data_frame(mhdr, 0xFF);
            for len in 1..27 {
                let frame = decode_phy_bytes(&full[..len], None).unwrap();
                assert_eq!(frame.payload_size, len as u16);
                assert_eq!(frame.malformed, Some(Malformed::Length), "len {len}");
                assert_eq!(frame.dev_addr, None, "len {len}");
                assert_eq!(frame.fcnt, None, "len {len}");
                assert_eq!(frame.fport, None, "len {len}");
                assert_eq!(frame.fctrl, None, "len {len}");
                assert_eq!(frame.dev_nonce, None, "len {len}");
            }
            // 27 bytes is the whole frame header and the MIC.
            let frame = decode_phy_bytes(&full[..27], None).unwrap();
            assert_eq!(frame.malformed, None);
            assert!(frame.dev_addr.is_some());
            assert_eq!(frame.fcnt, Some(1));
            assert!(frame.fctrl.is_some());
            assert_eq!(frame.fport, None);
        }

        // JoinRequests are 23 bytes, no fewer and no more.
        let full = join_request([0x34, 0x12]);
        for len in 1..23 {
            let frame = decode_phy_bytes(&full[..len], None).unwrap();
            assert_eq!(frame.mtype, MType::JoinRequest);
            assert_eq!(frame.malformed, Some(Malformed::Length), "len {len}");
            assert_eq!(frame.dev_eui, None, "len {len}");
            assert_eq!(frame.join_eui, None, "len {len}");
            assert_eq!(frame.dev_nonce, None, "len {len}");
            assert_eq!(frame.fctrl, None, "len {len}");
        }
        let frame = decode_phy_bytes(&full, None).unwrap();
        assert_eq!(frame.malformed, None);
        assert!(frame.dev_eui.is_some());
        assert!(frame.join_eui.is_some());
        assert_eq!(frame.dev_nonce, Some(0x1234));
    }

    // --- malformed frames ---

    const DATA_MHDRS: [u8; 4] = [0x40, 0x60, 0x80, 0xA0];

    /// `len` zero bytes with `mhdr` first and, when the frame is long enough
    /// to have an FCtrl, `f_opts_len` in its low nibble.
    fn shaped(mhdr: u8, len: usize, f_opts_len: u8) -> Vec<u8> {
        let mut pkt = vec![0x00; len];
        pkt[0] = mhdr;
        if len > 5 {
            pkt[5] = f_opts_len;
        }
        pkt
    }

    /// One valid frame of each type, at lengths the type allows.
    fn valid_frames() -> Vec<(MType, Vec<u8>)> {
        vec![
            (MType::JoinRequest, join_request([0x01, 0x00])),
            (MType::JoinAccept, filled(0x20, 17)),
            (MType::JoinAccept, filled(0x20, 33)),
            (MType::UnconfirmedDataUp, data_frame(0x40, 0x00)),
            (MType::UnconfirmedDataDown, data_frame(0x60, 0x00)),
            (MType::ConfirmedDataUp, data_frame(0x80, 0x00)),
            (MType::ConfirmedDataDown, data_frame(0xA0, 0x00)),
            (MType::RejoinRequest, filled(0xC0, 19)),
            (MType::RejoinRequest, filled(0xC0, 24)),
            (MType::Proprietary, filled(0xE0, 5)),
        ]
    }

    /// Assert that `frame` is malformed for `reason`, keeps its type and
    /// size, and carries nothing read from the body.
    fn assert_malformed(frame: &DecodedFrame, reason: Malformed, mtype: &MType, size: usize) {
        assert_eq!(frame.malformed, Some(reason));
        assert_eq!(&frame.mtype, mtype);
        assert_eq!(frame.payload_size as usize, size);
        assert_eq!(frame.dev_addr, None);
        assert_eq!(frame.dev_eui, None);
        assert_eq!(frame.join_eui, None);
        assert_eq!(frame.dev_nonce, None);
        assert_eq!(frame.fcnt, None, "fcnt");
        assert_eq!(frame.fport, None, "fport");
        assert_eq!(frame.fctrl, None, "fctrl");
    }

    #[test]
    fn valid_frames_of_every_type_are_not_malformed() {
        for (mtype, pkt) in valid_frames() {
            let frame = decode_phy_bytes(&pkt, Some(&CrcStatus::Ok)).unwrap();
            assert_eq!(frame.malformed, None, "{mtype:?} len {}", pkt.len());
            assert_eq!(frame.mtype, mtype);
            assert_eq!(frame.payload_size as usize, pkt.len());
        }
    }

    #[test]
    fn valid_frames_keep_their_fields() {
        let frame = decode_phy_bytes(&join_request([0x34, 0x12]), Some(&CrcStatus::Ok)).unwrap();
        assert_eq!(frame.dev_eui.as_deref(), Some("22:22:22:22:22:22:22:22"));
        assert_eq!(frame.join_eui.as_deref(), Some("11:11:11:11:11:11:11:11"));
        assert_eq!(frame.dev_nonce, Some(0x1234));
        assert_eq!(frame.fctrl, None);

        for mhdr in DATA_MHDRS {
            let frame = decode_phy_bytes(&data_frame(mhdr, 0x02), Some(&CrcStatus::Ok)).unwrap();
            assert_eq!(frame.dev_addr.as_deref(), Some("04030201"), "{mhdr:#04x}");
            assert_eq!(frame.fcnt, Some(1));
            assert_eq!(frame.fport, Some(1));
            assert_eq!(frame.fctrl.unwrap().f_opts_len, 2);
            assert_eq!(frame.dev_eui, None);
            assert_eq!(frame.join_eui, None);
            assert_eq!(frame.dev_nonce, None);
        }

        // JoinAccept, RejoinRequest and Proprietary have an encrypted or
        // opaque body: the type and size are all there is to read.
        for pkt in [filled(0x20, 17), filled(0xC0, 24), filled(0xE0, 5)] {
            let frame = decode_phy_bytes(&pkt, Some(&CrcStatus::Ok)).unwrap();
            assert_eq!(frame.malformed, None);
            assert_eq!(frame.payload_size as usize, pkt.len());
            assert_eq!(frame.dev_addr, None);
            assert_eq!(frame.dev_eui, None);
            assert_eq!(frame.fcnt, None);
        }
    }

    #[test]
    fn crc_status_decides_the_crc_rule() {
        for (mtype, pkt) in valid_frames() {
            // Only an explicit failure marks the frame.  A source that does
            // not report CRC, and a downlink, must not be hidden for it.
            let frame = decode_phy_bytes(&pkt, Some(&CrcStatus::Fail)).unwrap();
            assert_malformed(&frame, Malformed::Crc, &mtype, pkt.len());

            for crc in [Some(CrcStatus::NoCrc), Some(CrcStatus::Ok), None] {
                let frame = decode_phy_bytes(&pkt, crc.as_ref()).unwrap();
                assert_eq!(frame.malformed, None, "{mtype:?} {crc:?}");
            }
        }

        // The base64 entry point passes the status on.
        let pkt = data_frame(0x40, 0x00);
        let frame = decode_phy(&encode(&pkt), Some(&CrcStatus::Fail)).unwrap();
        assert_malformed(&frame, Malformed::Crc, &MType::UnconfirmedDataUp, pkt.len());
        let frame = decode_phy(&encode(&pkt), Some(&CrcStatus::Ok)).unwrap();
        assert_eq!(frame.malformed, None);
        assert!(frame.dev_addr.is_some());

        // Nothing to classify without a first byte, whatever the CRC says.
        assert!(decode_phy(&encode(b""), Some(&CrcStatus::Fail)).is_none());
        assert!(decode_phy("not!valid~base64", Some(&CrcStatus::Fail)).is_none());
    }

    #[test]
    fn crc_failed_only_for_an_explicit_failure() {
        assert!(crc_failed(Some(&CrcStatus::Fail)));
        assert!(!crc_failed(Some(&CrcStatus::Ok)));
        assert!(!crc_failed(Some(&CrcStatus::NoCrc)));
        assert!(!crc_failed(None));
    }

    #[test]
    fn reserved_mhdr_bits_are_malformed() {
        // RFU bits 4:2 and Major 1:0 must be zero, in every type, Proprietary
        // included.
        for (mtype, pkt) in valid_frames() {
            for bits in [0x04u8, 0x08, 0x10, 0x01, 0x02, 0x03, 0x1F, 0x14] {
                let mut bad = pkt.clone();
                bad[0] |= bits;
                let frame = decode_phy_bytes(&bad, Some(&CrcStatus::Ok)).unwrap();
                assert_malformed(&frame, Malformed::Mhdr, &mtype, pkt.len());
            }
        }
    }

    #[test]
    fn rules_apply_in_order() {
        // A bad MHDR (Major 1) and a wrong length (5 bytes) on a JoinRequest.
        let bad = shaped(0x01, 5, 0);
        let frame = decode_phy_bytes(&bad, Some(&CrcStatus::Ok)).unwrap();
        assert_malformed(&frame, Malformed::Mhdr, &MType::JoinRequest, 5);

        // All three at once: the CRC wins.
        let frame = decode_phy_bytes(&bad, Some(&CrcStatus::Fail)).unwrap();
        assert_malformed(&frame, Malformed::Crc, &MType::JoinRequest, 5);

        // A wrong length and a failed CRC: the CRC wins.
        let short = shaped(0x00, 5, 0);
        let frame = decode_phy_bytes(&short, Some(&CrcStatus::Fail)).unwrap();
        assert_malformed(&frame, Malformed::Crc, &MType::JoinRequest, 5);

        // A wrong length alone.
        let frame = decode_phy_bytes(&short, Some(&CrcStatus::Ok)).unwrap();
        assert_malformed(&frame, Malformed::Length, &MType::JoinRequest, 5);
    }

    #[test]
    fn join_request_length_is_exactly_23() {
        for len in [1usize, 19, 22, 23, 24, 30] {
            let frame = decode_phy_bytes(&shaped(0x00, len, 0), Some(&CrcStatus::Ok)).unwrap();
            if len == JOIN_REQUEST_LEN {
                assert_eq!(frame.malformed, None);
                assert!(frame.dev_eui.is_some());
                assert!(frame.join_eui.is_some());
                assert_eq!(frame.dev_nonce, Some(0));
            } else {
                assert_malformed(&frame, Malformed::Length, &MType::JoinRequest, len);
            }
        }
    }

    #[test]
    fn join_accept_length_is_17_or_33() {
        for len in [1usize, 16, 17, 18, 32, 33, 34] {
            let frame = decode_phy_bytes(&shaped(0x20, len, 0), Some(&CrcStatus::Ok)).unwrap();
            if len == 17 || len == 33 {
                assert_eq!(frame.malformed, None, "len {len}");
            } else {
                assert_malformed(&frame, Malformed::Length, &MType::JoinAccept, len);
            }
        }
    }

    #[test]
    fn rejoin_request_length_is_19_or_24() {
        for len in [1usize, 18, 19, 20, 23, 24, 25] {
            let frame = decode_phy_bytes(&shaped(0xC0, len, 0), Some(&CrcStatus::Ok)).unwrap();
            if len == 19 || len == 24 {
                assert_eq!(frame.malformed, None, "len {len}");
            } else {
                assert_malformed(&frame, Malformed::Length, &MType::RejoinRequest, len);
            }
        }
    }

    #[test]
    fn data_frame_length_is_12_plus_fopts_len() {
        let types = [
            (0x40u8, MType::UnconfirmedDataUp),
            (0x60, MType::UnconfirmedDataDown),
            (0x80, MType::ConfirmedDataUp),
            (0xA0, MType::ConfirmedDataDown),
        ];
        for (mhdr, mtype) in types {
            for n in 0u8..=15 {
                // One byte short of the header, the FOpts and the MIC.
                let len = 11 + n as usize;
                let frame = decode_phy_bytes(&shaped(mhdr, len, n), Some(&CrcStatus::Ok)).unwrap();
                assert_malformed(&frame, Malformed::Length, &mtype, len);

                // Exactly enough.
                let len = 12 + n as usize;
                let frame = decode_phy_bytes(&shaped(mhdr, len, n), Some(&CrcStatus::Ok)).unwrap();
                assert_eq!(frame.malformed, None, "{mtype:?} FOptsLen {n}");
                assert_eq!(frame.mtype, mtype);
                assert_eq!(frame.payload_size as usize, len);
                assert!(frame.dev_addr.is_some());
                assert_eq!(frame.fcnt, Some(0));
                assert_eq!(frame.fport, None, "no room for an FPort at {len} bytes");
                assert_eq!(frame.fctrl.unwrap().f_opts_len, n);
            }
        }
    }

    #[test]
    fn data_frame_fopts_len_ignores_the_flag_bits() {
        // FCtrl 0xF3: every flag set and 3 bytes of FOpts, so 15 bytes.
        let frame = decode_phy_bytes(&shaped(0x40, 14, 0xF3), None).unwrap();
        assert_malformed(&frame, Malformed::Length, &MType::UnconfirmedDataUp, 14);
        let frame = decode_phy_bytes(&shaped(0x40, 15, 0xF3), None).unwrap();
        assert_eq!(frame.malformed, None);
        assert_eq!(frame.fctrl.unwrap().f_opts_len, 3);
    }

    #[test]
    fn proprietary_has_no_length_rule() {
        for len in [1usize, 2, 255] {
            let frame = decode_phy_bytes(&shaped(0xE0, len, 0), Some(&CrcStatus::Ok)).unwrap();
            assert_eq!(frame.malformed, None, "len {len}");
            assert_eq!(frame.mtype, MType::Proprietary);
            assert_eq!(frame.payload_size as usize, len);
        }
        // Its MHDR is still checked.
        let frame = decode_phy_bytes(&shaped(0xE4, 2, 0), None).unwrap();
        assert_malformed(&frame, Malformed::Mhdr, &MType::Proprietary, 2);
    }

    #[test]
    fn fopts_with_fport_zero_is_not_malformed() {
        // FCtrl 0x03 (3 bytes of FOpts) and FPort 0: the protocol forbids it,
        // but real devices send it.
        let mut pkt = vec![0x40u8, 0x01, 0x02, 0x03, 0x04, 0x03, 0x09, 0x00];
        pkt.extend_from_slice(&[0x02, 0x07, 0x00]); // FOpts
        pkt.push(0x00); // FPort 0
        pkt.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]); // MIC
        let frame = decode_phy_bytes(&pkt, Some(&CrcStatus::Ok)).unwrap();
        assert_eq!(frame.malformed, None);
        assert_eq!(frame.dev_addr.as_deref(), Some("04030201"));
        assert_eq!(frame.fcnt, Some(9));
        assert_eq!(frame.fport, Some(0));
        assert_eq!(frame.fctrl.unwrap().f_opts_len, 3);
    }

    #[test]
    fn noise_seen_on_a_gateway_with_a_failed_crc_carries_no_fields() {
        // A one-byte "ConfirmedDataDown".
        let frame = decode_phy_bytes(&[0xA0], Some(&CrcStatus::Fail)).unwrap();
        assert_malformed(&frame, Malformed::Crc, &MType::ConfirmedDataDown, 1);

        // A 22-byte "Proprietary".
        let mut prop = vec![0xE0u8];
        prop.extend((1..22).map(|i| (i * 7) as u8));
        let frame = decode_phy_bytes(&prop, Some(&CrcStatus::Fail)).unwrap();
        assert_malformed(&frame, Malformed::Crc, &MType::Proprietary, 22);

        // A 238-byte "UnconfirmedDataUp" whose bytes happen to read as a
        // DevAddr and an FPort of 127.
        let mut noise = vec![0x40u8, 0x22, 0xFA, 0x74, 0x68, 0x00, 0x05, 0x00, 127];
        noise.resize(238, 0x5A);
        let frame = decode_phy_bytes(&noise, Some(&CrcStatus::Fail)).unwrap();
        assert_malformed(&frame, Malformed::Crc, &MType::UnconfirmedDataUp, 238);
    }

    #[test]
    fn short_join_requests_with_a_good_crc_carry_no_eui() {
        for len in 19..=22 {
            let mut pkt = vec![0x00u8];
            pkt.extend((1..len).map(|i| i as u8));
            let frame = decode_phy_bytes(&pkt, Some(&CrcStatus::Ok)).unwrap();
            assert_malformed(&frame, Malformed::Length, &MType::JoinRequest, len);
        }
    }

    #[test]
    fn parsers_return_on_slices_of_any_length() {
        let types = [
            MType::UnconfirmedDataUp,
            MType::UnconfirmedDataDown,
            MType::ConfirmedDataUp,
            MType::ConfirmedDataDown,
        ];
        // Whatever the caller checked, a slice too short to hold the frame
        // yields no fields instead of a panic.  0xFF fills announce 15 bytes
        // of FOpts; 0x00 fills announce none.
        for fill in [0x00u8, 0xFF] {
            for len in 0..=40 {
                let bytes = vec![fill; len];
                for mtype in &types {
                    let frame =
                        super::decode_data_frame(&bytes, mtype.clone(), len as u16).unwrap();
                    let f_opts_len = if len > 5 { (fill & 0x0F) as usize } else { 0 };
                    assert_eq!(
                        frame.dev_addr.is_some(),
                        len >= 12 + f_opts_len,
                        "fill {fill:#04x} len {len}"
                    );
                    assert_eq!(frame.fcnt.is_some(), frame.dev_addr.is_some());
                    assert_eq!(frame.fctrl.is_some(), frame.dev_addr.is_some());
                    assert_eq!(frame.payload_size as usize, len);
                }
            }
        }

        for fill in [0x00u8, 0xFF] {
            for len in 0..=40 {
                let bytes = vec![fill; len];
                let frame =
                    super::decode_join_request(&bytes, MType::JoinRequest, len as u16).unwrap();
                assert_eq!(frame.dev_eui.is_some(), len >= 23, "len {len}");
                assert_eq!(frame.join_eui.is_some(), len >= 23, "len {len}");
                assert_eq!(frame.dev_nonce.is_some(), len >= 23, "len {len}");
            }
        }
    }

    /// Every first byte at every length from 1 to 40, with an all-zero body
    /// (FOptsLen 0) and an all-ones body (FOptsLen 15), under every CRC
    /// status, checked against an independent statement of the rules.
    #[test]
    fn every_first_byte_and_length_is_classified_by_the_rules() {
        // The rules, restated from the frame formats with none of the
        // decoder's helpers: allowed lengths by message type (the top three
        // bits), the reserved bits as the mask 0x1F, FOptsLen as the low
        // nibble of byte 5, and the order CRC, MHDR, length.
        fn expected(bytes: &[u8], crc: Option<&CrcStatus>) -> Option<Malformed> {
            if matches!(crc, Some(CrcStatus::Fail)) {
                return Some(Malformed::Crc);
            }
            if bytes[0] & 0x1F != 0 {
                return Some(Malformed::Mhdr);
            }
            let len = bytes.len();
            let fits = match bytes[0] >> 5 {
                0 => len == 23,
                1 => len == 17 || len == 33,
                2..=5 => {
                    let f_opts_len = if len > 5 {
                        (bytes[5] & 0x0F) as usize
                    } else {
                        0
                    };
                    len >= 12 + f_opts_len
                }
                6 => len == 19 || len == 24,
                _ => true,
            };
            (!fits).then_some(Malformed::Length)
        }

        let names = [
            MType::JoinRequest,
            MType::JoinAccept,
            MType::UnconfirmedDataUp,
            MType::UnconfirmedDataDown,
            MType::ConfirmedDataUp,
            MType::ConfirmedDataDown,
            MType::RejoinRequest,
            MType::Proprietary,
        ];
        let statuses = [
            Some(CrcStatus::Ok),
            Some(CrcStatus::Fail),
            Some(CrcStatus::NoCrc),
            None,
        ];

        for first in 0..=255u8 {
            for len in 1..=40usize {
                for body in [0x00u8, 0xFF] {
                    let mut bytes = vec![body; len];
                    bytes[0] = first;
                    for crc in &statuses {
                        let frame = decode_phy_bytes(&bytes, crc.as_ref()).unwrap();
                        let context =
                            format!("first {first:#04x} len {len} body {body:#04x} {crc:?}");

                        assert_eq!(frame.malformed, expected(&bytes, crc.as_ref()), "{context}");
                        assert_eq!(frame.mtype, names[(first >> 5) as usize], "{context}");
                        assert_eq!(frame.payload_size as usize, len, "{context}");

                        if frame.malformed.is_some() {
                            assert_eq!(frame.dev_addr, None, "{context}");
                            assert_eq!(frame.dev_eui, None, "{context}");
                            assert_eq!(frame.join_eui, None, "{context}");
                            assert_eq!(frame.dev_nonce, None, "{context}");
                            assert_eq!(frame.fcnt, None, "{context}");
                            assert_eq!(frame.fport, None, "{context}");
                            assert_eq!(frame.fctrl, None, "{context}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn eui_le_formatting() {
        // Bytes in LE order → reversed to BE display
        let bytes = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];
        let eui = Eui64::from_le_slice(&bytes).unwrap();
        assert_eq!(eui.to_string(), "08:07:06:05:04:03:02:01");
    }
}
