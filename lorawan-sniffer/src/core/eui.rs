//! 64-bit extended unique identifiers (gateway EUI, DevEUI, JoinEUI).
//!
//! An [`Eui64`] always holds its bytes in display order (most significant
//! byte first), regardless of where they came from.  The two sources use
//! opposite byte orders, so the constructors say which one they expect:
//!
//! - the Semtech UDP header carries the gateway EUI big-endian
//!   ([`Eui64::from_be_slice`]);
//! - LoRaWAN PHY payloads carry JoinEUI and DevEUI little-endian
//!   ([`Eui64::from_le_slice`]).

use std::fmt;
use std::str::FromStr;

use thiserror::Error;

/// A 64-bit EUI, stored most significant byte first.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Eui64(pub [u8; 8]);

impl Eui64 {
    /// Build an EUI from 8 bytes already in display order, as found in the
    /// Semtech UDP packet header.
    ///
    /// Returns `None` unless the slice is exactly 8 bytes long.
    pub fn from_be_slice(b: &[u8]) -> Option<Self> {
        let bytes: [u8; 8] = b.try_into().ok()?;
        Some(Self(bytes))
    }

    /// Build an EUI from 8 bytes in little-endian order, as found in LoRaWAN
    /// frame fields; the bytes are reversed so the result is in display order.
    ///
    /// Returns `None` unless the slice is exactly 8 bytes long.
    pub fn from_le_slice(b: &[u8]) -> Option<Self> {
        let mut bytes: [u8; 8] = b.try_into().ok()?;
        bytes.reverse();
        Some(Self(bytes))
    }
}

/// Formats as 8 colon-separated uppercase hex bytes, e.g. `AA:BB:CC:DD:00:11:22:33`.
impl fmt::Display for Eui64 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, b) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(":")?;
            }
            write!(f, "{:02X}", b)?;
        }
        Ok(())
    }
}

/// Returned when a string is not a valid EUI-64.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("invalid EUI-64 {0:?}: expected 16 hex digits, optionally separated by ':' or '-' after every second digit")]
pub struct ParseEuiError(String);

impl FromStr for Eui64 {
    type Err = ParseEuiError;

    /// Accepts exactly 16 hex digits in any case, either contiguous
    /// (`AABBCCDD00112233`) or split into pairs by a single kind of separator
    /// (`aa:bb:cc:dd:00:11:22:33`, `AA-BB-CC-DD-00-11-22-33`).
    ///
    /// Anything else is rejected: other lengths, mixed separators, other
    /// groupings, surrounding whitespace and prefixes such as `0x`.  Leniency
    /// here would let a typo in a configuration file silently match nothing.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || ParseEuiError(s.to_string());
        let raw = s.as_bytes();

        // Contiguous form: 16 digits.  Separated form: 8 pairs plus 7
        // separators.  Collect the digits for either, then decode them.
        let mut digits = [0u8; 16];
        match raw.len() {
            16 => {
                for (slot, &c) in digits.iter_mut().zip(raw) {
                    *slot = hex_value(c).ok_or_else(err)?;
                }
            }
            23 => {
                let sep = raw[2];
                if sep != b':' && sep != b'-' {
                    return Err(err());
                }
                for (i, &c) in raw.iter().enumerate() {
                    if i % 3 == 2 {
                        if c != sep {
                            return Err(err());
                        }
                    } else {
                        // Position i holds digit number i - i/3.
                        digits[i - i / 3] = hex_value(c).ok_or_else(err)?;
                    }
                }
            }
            _ => return Err(err()),
        }

        let mut bytes = [0u8; 8];
        for (byte, pair) in bytes.iter_mut().zip(digits.chunks_exact(2)) {
            *byte = (pair[0] << 4) | pair[1];
        }
        Ok(Self(bytes))
    }
}

/// Value of one ASCII hex digit, or `None` for any other byte.
fn hex_value(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EUI: Eui64 = Eui64([0xAA, 0xBB, 0xCC, 0xDD, 0x00, 0x11, 0x22, 0x33]);

    #[test]
    fn display_is_colon_separated_uppercase() {
        assert_eq!(EUI.to_string(), "AA:BB:CC:DD:00:11:22:33");
        assert_eq!(Eui64([0; 8]).to_string(), "00:00:00:00:00:00:00:00");
        assert_eq!(Eui64([0x0A; 8]).to_string(), "0A:0A:0A:0A:0A:0A:0A:0A");
    }

    #[test]
    fn parses_every_accepted_form() {
        for s in [
            "AABBCCDD00112233",
            "aabbccdd00112233",
            "AaBbCcDd00112233",
            "AA:BB:CC:DD:00:11:22:33",
            "aa:bb:cc:dd:00:11:22:33",
            "AA-BB-CC-DD-00-11-22-33",
            "aa-bb-cc-dd-00-11-22-33",
        ] {
            assert_eq!(s.parse::<Eui64>(), Ok(EUI), "input {s:?}");
        }
    }

    #[test]
    fn display_output_parses_back() {
        let parsed: Eui64 = EUI.to_string().parse().unwrap();
        assert_eq!(parsed, EUI);
    }

    #[test]
    fn rejects_malformed_input() {
        for s in [
            "",
            // Wrong number of digits.
            "AABBCCDD0011223",
            "AABBCCDD001122334",
            // Not hex.
            "AABBCCDD0011223G",
            // Wrong number of groups.
            "AA:BB:CC:DD:00:11:22",
            "AA:BB:CC:DD:00:11:22:33:44",
            // Mixed or unsupported separators.
            "AA:BB-CC:DD:00:11:22:33",
            "AA.BB.CC.DD.00.11.22.33",
            "AA BB CC DD 00 11 22 33",
            // Wrong grouping.
            "AABB:CCDD:0011:2233",
            "A:BB:CC:DD:00:11:22:33",
            "AA:BB:CC:DD:00:11:22:3",
            ":AABBCCDD00112233",
            "AA:BB:CC:DD:00:11:22:33:",
            // Whitespace and prefixes.
            " AABBCCDD00112233",
            "AABBCCDD00112233 ",
            "0xAABBCCDD001122",
            // Non-ASCII text that is 16 bytes long.
            "ÄBBCCDD00112233",
            "AABBCCDD001122é",
        ] {
            assert!(
                s.parse::<Eui64>().is_err(),
                "input {s:?} should be rejected"
            );
        }
    }

    #[test]
    fn parse_error_names_the_input() {
        let err = "not-an-eui".parse::<Eui64>().unwrap_err();
        assert!(err.to_string().contains("not-an-eui"));
    }

    #[test]
    fn from_be_slice_keeps_byte_order() {
        let eui = Eui64::from_be_slice(&[0xAA, 0xBB, 0xCC, 0xDD, 0x00, 0x11, 0x22, 0x33]).unwrap();
        assert_eq!(eui, EUI);
    }

    #[test]
    fn from_le_slice_reverses_bytes() {
        let eui = Eui64::from_le_slice(&[0x33, 0x22, 0x11, 0x00, 0xDD, 0xCC, 0xBB, 0xAA]).unwrap();
        assert_eq!(eui, EUI);
        assert_eq!(eui.to_string(), "AA:BB:CC:DD:00:11:22:33");
    }

    #[test]
    fn slices_of_the_wrong_length_are_rejected() {
        assert_eq!(Eui64::from_be_slice(&[]), None);
        assert_eq!(Eui64::from_be_slice(&[0; 7]), None);
        assert_eq!(Eui64::from_be_slice(&[0; 9]), None);
        assert_eq!(Eui64::from_le_slice(&[0; 7]), None);
        assert_eq!(Eui64::from_le_slice(&[0; 9]), None);
    }

    #[test]
    fn usable_as_a_map_key() {
        let mut seen = std::collections::HashSet::new();
        assert!(seen.insert(EUI));
        assert!(!seen.insert("aabbccdd00112233".parse().unwrap()));
    }
}
