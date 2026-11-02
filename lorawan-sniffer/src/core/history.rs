//! What a history request asks for, and how device text is matched.
//!
//! This module does no I/O and writes no SQL.  [`HistoryQuery`] describes a
//! request, [`crate::core::storage::Storage::query_history`] answers it with a
//! [`HistoryPage`], and the label rule that SQL cannot express lives here, so
//! the WebUI and the server can be checked against the same cases.
//!
//! ## Matching
//!
//! The device text matches a record when it is a substring of the DevAddr, of
//! the DevEUI, or of the record's label.  The label belongs to the record's
//! identifier, which is the DevAddr when the record has one and the DevEUI
//! otherwise; a label on the DevEUI of a record that also carries a DevAddr
//! does not match it.
//!
//! DevAddr and DevEUI are ASCII, so SQLite's ASCII-only `lower()` agrees with
//! JavaScript's `toLowerCase` on them.  Labels are free text, so they are
//! folded here with [`fold_case`] and compared in memory.

use chrono::{DateTime, Utc};

use crate::core::metadata::{MType, MessageRecord, Status};

/// Most records one history request returns.
///
/// A capped response is a few megabytes of JSON; the cap keeps the memory a
/// request needs, and the time to send it, bounded.
pub const MAX_HISTORY_ROWS: usize = 5_000;

/// The earliest time a history request accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LowerBound {
    /// Strictly after this time (`since`): the incremental form existing
    /// clients use.
    After(DateTime<Utc>),
    /// This time or later (`from`): the form the WebUI's time ranges use,
    /// which keeps a record stamped exactly on the cutoff.
    From(DateTime<Utc>),
}

/// The conditions a record must meet besides the time bound.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HistoryFilter {
    /// Text to find in the DevAddr, the DevEUI or the label, as typed (not
    /// trimmed).  `None` and the empty text both mean any record.
    pub device: Option<String>,
    /// Message types to keep; empty means any.
    pub mtypes: Vec<MType>,
    /// Status to keep; `None` means any.
    pub status: Option<Status>,
    /// Gateway EUI, compared exactly as records carry it; `None` and the
    /// empty text both mean any.
    pub gateway: Option<String>,
    /// Leave malformed records out.  They are counted either way.
    pub hide_malformed: bool,
}

impl HistoryFilter {
    /// Does this filter ask for anything beyond the time bound?
    ///
    /// Only a request that does not needs no scan: a time bound alone is an
    /// index range, and counting it reads the index alone.
    pub fn has_row_filter(&self) -> bool {
        self.device_text().is_some()
            || !self.mtypes.is_empty()
            || self.status.is_some()
            || self.gateway_text().is_some()
            || self.hide_malformed
    }

    /// The device text, unless it is absent or empty.
    pub fn device_text(&self) -> Option<&str> {
        self.device.as_deref().filter(|d| !d.is_empty())
    }

    /// The gateway EUI, unless it is absent or empty.
    pub fn gateway_text(&self) -> Option<&str> {
        self.gateway.as_deref().filter(|g| !g.is_empty())
    }
}

/// One history request.
#[derive(Debug, Clone)]
pub struct HistoryQuery {
    /// The time bound, or `None` for the whole database.
    pub lower: Option<LowerBound>,
    pub filter: HistoryFilter,
    /// Most records to return, at most [`MAX_HISTORY_ROWS`].
    pub limit: usize,
    /// Keep the newest `limit` matches (`true`) or the oldest (`false`).  The
    /// records come back oldest first either way.
    pub newest: bool,
    /// Also count the matches, however many there are.
    pub count: bool,
}

/// The answer to a [`HistoryQuery`].
#[derive(Debug, Clone)]
pub struct HistoryPage {
    /// The records, always oldest first.
    pub records: Vec<MessageRecord>,
    /// How many records match, `Some` when the request asked for the count.
    /// Malformed records are left out of it when the filter hides them.
    pub total: Option<u64>,
    /// How many malformed records match every other condition, whether the
    /// filter leaves them out or returns them.  `Some` when the count was
    /// asked for and could be computed; `None` otherwise.  One number serves
    /// both states of the "show malformed" box.
    ///
    /// Counting them takes a scan, so a database that cannot run scans on a
    /// separate connection (one not in WAL mode) does not compute it.
    pub malformed: Option<u64>,
}

/// Lower-case `text` the way the WebUI does.
///
/// `str::to_lowercase` and JavaScript's `String.prototype.toLowerCase` both
/// apply Unicode's full, locale-independent mapping, special cases included,
/// so a label folds to the same text in the browser and here: `ÑANDÚ` becomes
/// `ñandú`; `İSTANBUL` becomes `i`, U+0307 (a combining dot), then `stanbul`,
/// so that a plain `i` does not find it; `ΟΔΟΣ ΟΔΟΣ` becomes `οδος οδος`, with
/// the final sigma at the end of each word; `ẞ` becomes `ß`.  The tests pin
/// these outputs.
///
/// The two sides could differ only for a character added in a Unicode version
/// that one of them does not know yet.
pub fn fold_case(text: &str) -> String {
    text.to_lowercase()
}

/// The identifiers whose label contains `folded_needle`.
///
/// `labels` pairs each identifier with its label text as stored; the labels
/// are folded here and `folded_needle` must already be (see [`fold_case`]).
/// An empty needle matches every label.  The result keeps the order of
/// `labels`.
pub fn devices_with_matching_label<'a>(
    folded_needle: &str,
    labels: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Vec<&'a str> {
    labels
        .into_iter()
        .filter(|(_, label)| fold_case(label).contains(folded_needle))
        .map(|(device, _)| device)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // The expected texts are what JavaScript's toLowerCase returns for the
    // same input, recorded from a Node run.

    #[test]
    fn folds_an_accented_name_like_javascript() {
        assert_eq!(fold_case("ÑANDÚ"), "ñandú");
    }

    #[test]
    fn folds_a_dotted_capital_i_into_two_code_points() {
        assert_eq!(fold_case("İSTANBUL"), "i\u{307}stanbul");
    }

    #[test]
    fn folds_the_final_sigma_of_every_word() {
        assert_eq!(fold_case("ΟΔΟΣ ΟΔΟΣ"), "οδος οδος");
        assert!(fold_case("ΟΔΟΣ ΟΔΟΣ").matches('ς').count() == 2);
    }

    #[test]
    fn folds_the_capital_sharp_s() {
        assert_eq!(fold_case("ẞ"), "ß");
    }

    #[test]
    fn folding_leaves_ascii_identifiers_to_ascii_lowercase() {
        assert_eq!(
            fold_case("36:31:39:54:36:5D:05:39"),
            "36:31:39:54:36:5d:05:39"
        );
        assert_eq!(fold_case("00E8A80B"), "00e8a80b");
    }

    fn labels() -> Vec<(&'static str, &'static str)> {
        vec![
            ("00E8A80B", "Havana kitchen"),
            ("26011F2A", "ÑANDÚ quiet sensor"),
            ("36:31:39:54:36:5D:05:39", "havana roof"),
            ("26011F2B", "Lisbon"),
        ]
    }

    #[test]
    fn a_needle_matches_labels_that_contain_it() {
        assert_eq!(
            devices_with_matching_label("havana", labels()),
            ["00E8A80B", "36:31:39:54:36:5D:05:39"]
        );
        assert_eq!(devices_with_matching_label("kit", labels()), ["00E8A80B"]);
    }

    #[test]
    fn a_label_matches_whatever_its_case() {
        assert_eq!(
            devices_with_matching_label(&fold_case("HAVANA ROOF"), labels()),
            ["36:31:39:54:36:5D:05:39"]
        );
        assert_eq!(
            devices_with_matching_label(&fold_case("ñandú"), labels()),
            ["26011F2A"]
        );
        assert_eq!(
            devices_with_matching_label(&fold_case("ÑANDÚ"), labels()),
            ["26011F2A"]
        );
    }

    #[test]
    fn a_needle_that_no_label_contains_matches_nothing() {
        assert!(devices_with_matching_label("paris", labels()).is_empty());
        assert!(devices_with_matching_label("havana", Vec::new()).is_empty());
    }

    #[test]
    fn an_empty_needle_matches_every_label() {
        assert_eq!(devices_with_matching_label("", labels()).len(), 4);
    }

    #[test]
    fn label_text_is_matched_literally() {
        let labels = [
            ("00E8A80B", "100% sure"),
            ("26011F2A", "a_b"),
            ("26011F2B", "axb"),
        ];
        assert_eq!(devices_with_matching_label("%", labels), ["00E8A80B"]);
        assert_eq!(devices_with_matching_label("a_b", labels), ["26011F2A"]);
    }

    #[test]
    fn a_time_bound_alone_is_not_a_row_filter() {
        assert!(!HistoryFilter::default().has_row_filter());
    }

    #[test]
    fn each_condition_is_a_row_filter() {
        let filters = [
            HistoryFilter {
                device: Some("havana".to_string()),
                ..Default::default()
            },
            HistoryFilter {
                mtypes: vec![MType::JoinRequest],
                ..Default::default()
            },
            HistoryFilter {
                status: Some(Status::Blocked),
                ..Default::default()
            },
            HistoryFilter {
                gateway: Some("AA:BB:CC:DD:00:11:22:33".to_string()),
                ..Default::default()
            },
            HistoryFilter {
                hide_malformed: true,
                ..Default::default()
            },
        ];
        for filter in filters {
            assert!(filter.has_row_filter(), "{filter:?}");
        }
    }

    #[test]
    fn empty_device_and_gateway_text_ask_for_nothing() {
        let filter = HistoryFilter {
            device: Some(String::new()),
            gateway: Some(String::new()),
            ..Default::default()
        };
        assert!(!filter.has_row_filter());
        assert_eq!(filter.device_text(), None);
        assert_eq!(filter.gateway_text(), None);
    }

    #[test]
    fn whitespace_in_the_device_text_is_kept() {
        let filter = HistoryFilter {
            device: Some(" ".to_string()),
            ..Default::default()
        };
        assert!(filter.has_row_filter());
        assert_eq!(filter.device_text(), Some(" "));
    }
}
