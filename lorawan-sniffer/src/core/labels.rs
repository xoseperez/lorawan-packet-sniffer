//! Validation and normalisation for device labels.
//!
//! A label is free text that an operator attaches to a device identifier: the
//! DevAddr shown on data frames (`00E8A80B`) or the DevEUI shown on join
//! requests (`36:31:39:54:36:5D:05:39`).  This module only decides what is an
//! acceptable identifier and an acceptable label; it does no I/O, so the API
//! layer can reject bad input before anything touches the database.
//!
//! Identifiers are normalised to the exact text the decoder writes into
//! records.  The label table, the live-device list and the API all use that
//! text as the key, so a label saved as `00e8a80b` still matches the rows
//! that show `00E8A80B`.
//!
//! The error messages are written for the person using the WebUI: the dialog
//! shows them as they are.

use std::str::FromStr;

use thiserror::Error;

use crate::core::eui::Eui64;

/// Longest label, counted in characters rather than bytes so that accented
/// letters, CJK text and emoji are not penalised for their encoded size.
pub const MAX_LABEL_CHARS: usize = 64;

/// Most labels the database may hold at once.
///
/// The retention sweep reads every label that has no stored record, so this
/// bounds the work done while the storage lock is held.
pub const MAX_LABELS: usize = 10_000;

/// Why an identifier or a label was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum LabelError {
    /// The identifier is neither a DevAddr nor a DevEUI.
    #[error("not a DevAddr (8 hex digits) or DevEUI (16 hex digits)")]
    BadDevice,

    /// The label is longer than [`MAX_LABEL_CHARS`].
    #[error("a label can have at most {MAX_LABEL_CHARS} characters")]
    TooLong,

    /// The label contains a newline, tab or other control character.
    #[error("a label cannot contain control characters")]
    ControlChar,
}

/// Normalise a device identifier to the form used in message records.
///
/// - 8 hex digits are a DevAddr and come back upper case (`00e8a80b` becomes
///   `00E8A80B`).
/// - 16 hex digits are a DevEUI, written either contiguously or in pairs
///   separated by `:` or `-`, and come back colon-separated and upper case
///   (`36:31:39:54:36:5D:05:39`).
///
/// Surrounding whitespace is ignored.  Anything else, including a hex string
/// of any other length, is [`LabelError::BadDevice`].
pub fn parse_device_id(raw: &str) -> Result<String, LabelError> {
    let id = raw.trim();
    if id.len() == 8 && id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Ok(id.to_ascii_uppercase());
    }
    Eui64::from_str(id)
        .map(|eui| eui.to_string())
        .map_err(|_| LabelError::BadDevice)
}

/// Validate and normalise label text.
///
/// The text is trimmed.  An empty result means the operator wants the label
/// removed and is returned as `Ok(None)`, so the caller can treat "save an
/// empty label" and "delete" the same way.
///
/// Control characters (newline, tab, DEL, the C1 range, ...) are refused
/// rather than stripped: a label is shown on one line in a table cell and
/// written into a CSV export, and silently altering what the operator typed
/// would be more surprising than telling them.
pub fn normalize_label(raw: &str) -> Result<Option<String>, LabelError> {
    let label = raw.trim();
    if label.is_empty() {
        return Ok(None);
    }
    if label.chars().any(char::is_control) {
        return Err(LabelError::ControlChar);
    }
    if label.chars().count() > MAX_LABEL_CHARS {
        return Err(LabelError::TooLong);
    }
    Ok(Some(label.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // parse_device_id
    // -----------------------------------------------------------------------

    #[test]
    fn devaddr_is_upper_cased() {
        assert_eq!(parse_device_id("00e8a80b").unwrap(), "00E8A80B");
        assert_eq!(parse_device_id("00E8A80B").unwrap(), "00E8A80B");
        assert_eq!(parse_device_id("0aBc1D2e").unwrap(), "0ABC1D2E");
    }

    #[test]
    fn devaddr_keeps_leading_zeros() {
        assert_eq!(parse_device_id("00000001").unwrap(), "00000001");
    }

    #[test]
    fn deveui_with_colons_dashes_or_none_gives_the_record_form() {
        let want = "36:31:39:54:36:5D:05:39";
        assert_eq!(parse_device_id("36:31:39:54:36:5D:05:39").unwrap(), want);
        assert_eq!(parse_device_id("36:31:39:54:36:5d:05:39").unwrap(), want);
        assert_eq!(parse_device_id("36-31-39-54-36-5D-05-39").unwrap(), want);
        assert_eq!(parse_device_id("36313954365D0539").unwrap(), want);
        assert_eq!(parse_device_id("36313954365d0539").unwrap(), want);
    }

    #[test]
    fn surrounding_whitespace_is_ignored() {
        assert_eq!(parse_device_id("  00e8a80b\n").unwrap(), "00E8A80B");
        assert_eq!(
            parse_device_id("\t36313954365D0539 ").unwrap(),
            "36:31:39:54:36:5D:05:39"
        );
    }

    #[test]
    fn output_matches_what_the_decoder_writes() {
        // The decoder formats a DevAddr with {:08X} and a DevEUI through
        // Eui64's Display; the normalised key must be byte-identical so that
        // it matches the rows stored in the messages table.
        assert_eq!(
            parse_device_id("0000ab12").unwrap(),
            format!("{:08X}", 0xAB12u32)
        );
        let eui = Eui64([0xAA, 0xBB, 0xCC, 0xDD, 0x00, 0x11, 0x22, 0x33]);
        assert_eq!(
            parse_device_id("aabbccdd00112233").unwrap(),
            eui.to_string()
        );
    }

    #[test]
    fn nine_or_fifteen_hex_digits_are_refused() {
        for bad in [
            "000000001",         // 9
            "0000001",           // 7
            "AABBCCDD0011223",   // 15
            "AABBCCDD001122334", // 17
            "A",
            "",
        ] {
            assert_eq!(parse_device_id(bad), Err(LabelError::BadDevice), "{bad:?}");
        }
    }

    #[test]
    fn non_hex_is_refused() {
        for bad in [
            "00E8A80G",
            "0x00E8A8",
            "ZZZZZZZZ",
            "AABBCCDD0011223G",
            "not a device",
            "00E8 A80B",
            "00E8A80B,",
        ] {
            assert_eq!(parse_device_id(bad), Err(LabelError::BadDevice), "{bad:?}");
        }
    }

    #[test]
    fn non_ascii_lookalikes_are_refused() {
        // Eight characters but not eight ASCII hex digits: the byte length
        // check alone would let these through.
        assert_eq!(
            parse_device_id("００Ｅ８Ａ８０Ｂ"),
            Err(LabelError::BadDevice)
        );
        // Two bytes per character, four characters: 8 bytes long.
        assert_eq!(parse_device_id("éééé"), Err(LabelError::BadDevice));
    }

    #[test]
    fn mixed_or_misplaced_separators_are_refused() {
        for bad in [
            "36:31-39:54:36:5D:05:39",
            "3631:3954:365D:0539",
            "36:31:39:54:36:5D:05",
            "36:31:39:54:36:5D:05:39:",
            ":36:31:39:54:36:5D:05:39",
        ] {
            assert_eq!(parse_device_id(bad), Err(LabelError::BadDevice), "{bad:?}");
        }
    }

    // -----------------------------------------------------------------------
    // normalize_label
    // -----------------------------------------------------------------------

    #[test]
    fn label_is_trimmed() {
        assert_eq!(
            normalize_label("  Qingping T&H Havana \n").unwrap(),
            Some("Qingping T&H Havana".to_string())
        );
    }

    #[test]
    fn inner_spaces_are_kept() {
        assert_eq!(normalize_label("a  b").unwrap(), Some("a  b".to_string()));
    }

    #[test]
    fn empty_or_blank_label_means_removal() {
        assert_eq!(normalize_label(""), Ok(None));
        assert_eq!(normalize_label("   "), Ok(None));
        assert_eq!(normalize_label(" \t\r\n "), Ok(None));
        // Unicode whitespace is trimmed too.
        assert_eq!(normalize_label("\u{00A0}\u{2003}"), Ok(None));
    }

    #[test]
    fn sixty_four_characters_pass_and_sixty_five_do_not() {
        let ok = "a".repeat(MAX_LABEL_CHARS);
        assert_eq!(normalize_label(&ok).unwrap(), Some(ok.clone()));
        let long = "a".repeat(MAX_LABEL_CHARS + 1);
        assert_eq!(normalize_label(&long), Err(LabelError::TooLong));
    }

    #[test]
    fn length_counts_characters_not_bytes() {
        // 64 two-byte letters are 128 bytes but still within the limit.
        let ok = "é".repeat(MAX_LABEL_CHARS);
        assert!(ok.len() > MAX_LABEL_CHARS);
        assert_eq!(normalize_label(&ok).unwrap(), Some(ok));
        let long = "é".repeat(MAX_LABEL_CHARS + 1);
        assert_eq!(normalize_label(&long), Err(LabelError::TooLong));

        // A four-byte emoji and a CJK character are one character each.
        let mixed = format!("{}{}", "温".repeat(32), "🌡".repeat(32));
        assert_eq!(mixed.chars().count(), MAX_LABEL_CHARS);
        assert_eq!(normalize_label(&mixed).unwrap(), Some(mixed));
    }

    #[test]
    fn length_is_checked_after_trimming() {
        let padded = format!("  {}  ", "a".repeat(MAX_LABEL_CHARS));
        assert_eq!(
            normalize_label(&padded).unwrap(),
            Some("a".repeat(MAX_LABEL_CHARS))
        );
    }

    #[test]
    fn control_characters_are_refused() {
        for bad in [
            "line\nbreak",
            "tab\there",
            "carriage\rreturn",
            "del\u{7f}",
            "nul\0byte",
            "escape\u{1b}[31m",
            "c1\u{85}control",
        ] {
            assert_eq!(
                normalize_label(bad),
                Err(LabelError::ControlChar),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn control_characters_at_the_edges_are_trimmed_not_refused() {
        // Whitespace controls at either end are removed by the trim, so a
        // trailing newline from a pasted value is harmless.
        assert_eq!(
            normalize_label("\nHavana\n").unwrap(),
            Some("Havana".to_string())
        );
        // Non-whitespace controls at the edge are still refused.
        assert_eq!(
            normalize_label("Havana\u{7f}"),
            Err(LabelError::ControlChar)
        );
    }

    #[test]
    fn text_that_looks_like_markup_or_a_formula_is_accepted_verbatim() {
        // Labels are shown as text and the CSV export guards formulas, so the
        // server stores exactly what was typed.
        for text in ["<b>x</b>", "=1+1", "+1", "-1", "@SUM(A1)", "a,\"b\""] {
            assert_eq!(normalize_label(text).unwrap(), Some(text.to_string()));
        }
    }

    #[test]
    fn content_is_checked_before_length() {
        // A text with both problems reports the control character, which is
        // the one the operator has to find in the text.
        let both = format!("{}\u{7f}", "a".repeat(MAX_LABEL_CHARS + 10));
        assert_eq!(normalize_label(&both), Err(LabelError::ControlChar));
        // A trailing newline is trimmed first, leaving only the length problem.
        let long = format!("{}\n", "a".repeat(MAX_LABEL_CHARS + 10));
        assert_eq!(normalize_label(&long), Err(LabelError::TooLong));
    }

    // -----------------------------------------------------------------------
    // LabelError
    // -----------------------------------------------------------------------

    #[test]
    fn error_messages_are_meant_for_the_user() {
        assert_eq!(
            LabelError::BadDevice.to_string(),
            "not a DevAddr (8 hex digits) or DevEUI (16 hex digits)"
        );
        assert_eq!(
            LabelError::TooLong.to_string(),
            "a label can have at most 64 characters"
        );
        assert_eq!(
            LabelError::ControlChar.to_string(),
            "a label cannot contain control characters"
        );
    }

    #[test]
    fn limits_match_the_documented_values() {
        assert_eq!(MAX_LABEL_CHARS, 64);
        assert_eq!(MAX_LABELS, 10_000);
    }
}
