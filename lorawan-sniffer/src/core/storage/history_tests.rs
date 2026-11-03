//! Tests of [`Storage::query_history`]: what it returns, which connection
//! answers, how it stops, and the plans of the statements it runs.
//!
//! Most tests run twice, on an in-memory database (one connection, the path
//! the tests of other modules use) and on a file database in WAL mode (the
//! reader connection), so both read paths must give the same answers.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, TimeZone, Utc};
use log::Level;
use rusqlite::{params, Connection};

use super::tests::{sample_record, TempDb};
use super::*;
use crate::core::history::HistoryFilter;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

const G1: &str = "AA:BB:CC:DD:00:11:22:33";
const G2: &str = "AA:BB:CC:DD:00:11:22:44";

/// The time the fixtures count from.
fn t0() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-03-01T10:00:00+00:00")
        .unwrap()
        .with_timezone(&Utc)
}

fn seconds(n: i64) -> DateTime<Utc> {
    t0() + chrono::Duration::seconds(n)
}

/// A forwarded uplink of DevAddr `01ABCDEF`, `n` seconds after [`t0`].
fn rec(id: &str, n: i64) -> MessageRecord {
    let mut r = sample_record();
    r.id = id.to_string();
    r.timestamp = seconds(n);
    r
}

fn edit(mut r: MessageRecord, change: impl FnOnce(&mut MessageRecord)) -> MessageRecord {
    change(&mut r);
    r
}

/// A malformed record: the reason set and the fields read from its bytes gone.
fn malformed_rec(id: &str, n: i64, reason: Malformed) -> MessageRecord {
    edit(rec(id, n), |r| {
        r.malformed = Some(reason);
        r.mtype = MType::Unknown;
        r.dev_addr = None;
        r.fcnt = None;
        r.fport = None;
    })
}

fn join_rec(id: &str, n: i64, dev_eui: &str) -> MessageRecord {
    edit(rec(id, n), |r| {
        r.mtype = MType::JoinRequest;
        r.dev_addr = None;
        r.dev_eui = Some(dev_eui.to_string());
        r.fcnt = None;
        r.fport = None;
    })
}

fn data_rec(id: &str, n: i64, dev_addr: &str) -> MessageRecord {
    edit(rec(id, n), |r| r.dev_addr = Some(dev_addr.to_string()))
}

fn store(s: &Storage, records: impl IntoIterator<Item = MessageRecord>) {
    let mut conn = s.conn.lock().unwrap();
    let tx = conn.transaction().unwrap();
    for r in records {
        insert_record(&tx, &r).unwrap();
    }
    tx.commit().unwrap();
}

/// `count` plain uplinks of 5,000 devices, one a second from `first` on, written
/// by SQL because that is much faster than building the records.
fn bulk(s: &Storage, first: i64, count: i64) {
    let conn = s.conn.lock().unwrap();
    conn.execute(
        "WITH RECURSIVE n(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM n WHERE i < ?3 - 1)
         INSERT INTO messages (id, timestamp, direction, status, source_protocol,
                               freq_mhz, datarate, mtype, dev_addr, payload_size, classified)
         SELECT 'bulk-' || (?2 + i),
                strftime('%Y-%m-%dT%H:%M:%S', ?1 + ?2 + i, 'unixepoch') || '+00:00',
                'up', 'forwarded', 'udp', 868.1, 'SF7BW125', 'UnconfirmedDataUp',
                printf('%08X', 0x26000000 + (i % 5000)), 23, 1
           FROM n",
        params![t0().timestamp(), first, count],
    )
    .unwrap();
}

fn label(s: &Storage, device: &str, text: &str) {
    s.set_label(device, text, true, 100).unwrap();
}

/// Run `check` on an in-memory database and on a file database in WAL mode.
fn on_both_backends(check: impl Fn(&Storage)) {
    println!("in memory");
    check(&Storage::open_in_memory().unwrap());

    println!("on a file");
    let tmp = TempDb::new("history");
    let s = Storage::open(&tmp.path()).unwrap();
    assert!(
        s.reader.is_some(),
        "a file database in WAL mode has a reader"
    );
    check(&s);
}

pub(super) fn flag() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

/// Every record, newest first, with the counts.
fn all() -> HistoryQuery {
    HistoryQuery {
        lower: None,
        upper: None,
        filter: HistoryFilter::default(),
        limit: MAX_HISTORY_ROWS,
        newest: true,
        count: true,
    }
}

fn with_filter(change: impl FnOnce(&mut HistoryFilter)) -> HistoryQuery {
    let mut q = all();
    change(&mut q.filter);
    q
}

fn run(s: &Storage, q: &HistoryQuery) -> HistoryPage {
    s.query_history(q, flag()).unwrap()
}

fn ids(page: &HistoryPage) -> Vec<&str> {
    page.records.iter().map(|r| r.id.as_str()).collect()
}

fn device(text: &str) -> HistoryQuery {
    with_filter(|f| f.device = Some(text.to_string()))
}

// ---------------------------------------------------------------------------
// Ranges, order and counts
// ---------------------------------------------------------------------------

fn ten_records(s: &Storage) {
    store(s, (0..10).map(|n| rec(&format!("r{n}"), n)));
}

#[test]
fn the_newest_rows_come_back_oldest_first() {
    on_both_backends(|s| {
        ten_records(s);
        let q = HistoryQuery { limit: 3, ..all() };
        let page = run(s, &q);
        assert_eq!(ids(&page), ["r7", "r8", "r9"]);
        assert_eq!(page.total, Some(10));
    });
}

#[test]
fn the_oldest_rows_after_a_bound_come_back_oldest_first() {
    on_both_backends(|s| {
        ten_records(s);
        let from = HistoryQuery {
            lower: Some(LowerBound::From(seconds(2))),
            limit: 3,
            newest: false,
            ..all()
        };
        let page = run(s, &from);
        assert_eq!(ids(&page), ["r2", "r3", "r4"]);
        assert_eq!(page.total, Some(8));

        let after = HistoryQuery {
            lower: Some(LowerBound::After(seconds(2))),
            ..from
        };
        assert_eq!(ids(&run(s, &after)), ["r3", "r4", "r5"]);
    });
}

#[test]
fn the_newest_rows_of_a_range_leave_out_the_older_ones() {
    on_both_backends(|s| {
        ten_records(s);
        let q = HistoryQuery {
            lower: Some(LowerBound::From(seconds(6))),
            limit: 2,
            ..all()
        };
        let page = run(s, &q);
        assert_eq!(ids(&page), ["r8", "r9"]);
        assert_eq!(page.total, Some(4));
    });
}

#[test]
fn a_record_exactly_on_the_bound_is_in_from_and_not_in_after() {
    on_both_backends(|s| {
        ten_records(s);
        let from = HistoryQuery {
            lower: Some(LowerBound::From(seconds(5))),
            newest: false,
            ..all()
        };
        assert_eq!(run(s, &from).records[0].id, "r5");
        let after = HistoryQuery {
            lower: Some(LowerBound::After(seconds(5))),
            ..from
        };
        assert_eq!(run(s, &after).records[0].id, "r6");
    });
}

/// A moment `seconds` after [`t0`] plus `nanos` nanoseconds.
fn moment(secs: i64, nanos: u32) -> DateTime<Utc> {
    Utc.timestamp_opt(t0().timestamp() + secs, nanos).unwrap()
}

#[test]
fn bounds_compare_correctly_with_every_stored_fraction_length() {
    on_both_backends(|s| {
        // Whole second, milliseconds, microseconds and nanoseconds: the four
        // forms `to_rfc3339` writes, in time order.
        let stamps = [
            ("whole", moment(0, 0)),
            ("milli", moment(0, 500_000_000)),
            ("micro", moment(0, 500_001_000)),
            ("nano", moment(0, 500_000_001)),
            ("next", moment(1, 0)),
        ];
        store(
            s,
            stamps
                .iter()
                .map(|(id, at)| edit(rec(id, 0), |r| r.timestamp = *at)),
        );
        let stored: Vec<String> = s
            .conn
            .lock()
            .unwrap()
            .prepare("SELECT timestamp FROM messages ORDER BY timestamp")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        // 500_000_001 ns sorts between .500000 and .500001 in time, but it is
        // the longest text; the order below is the order of the instants.
        assert_eq!(
            stored,
            [
                "2026-03-01T10:00:00+00:00",
                "2026-03-01T10:00:00.500+00:00",
                "2026-03-01T10:00:00.500000001+00:00",
                "2026-03-01T10:00:00.500001+00:00",
                "2026-03-01T10:00:01+00:00",
            ]
        );

        let oldest_after = |bound: LowerBound| {
            let q = HistoryQuery {
                lower: Some(bound),
                newest: false,
                ..all()
            };
            let page = run(s, &q);
            page.records.into_iter().map(|r| r.id).collect::<Vec<_>>()
        };
        // The stored order is the order of the instants, so a bound on any
        // of the five splits the list at its own position.
        let order = ["whole", "milli", "nano", "micro", "next"];
        for (k, (id, at)) in order
            .iter()
            .map(|id| (*id, stamps.iter().find(|(i, _)| i == id).unwrap().1))
            .enumerate()
        {
            assert_eq!(oldest_after(LowerBound::From(at)), &order[k..], "from {id}");
            assert_eq!(
                oldest_after(LowerBound::After(at)),
                &order[k + 1..],
                "after {id}"
            );
        }
    });
}

// ---------------------------------------------------------------------------
// The upper bound
// ---------------------------------------------------------------------------

/// The records from `first` seconds after [`t0`] up to, not including, `end`
/// seconds after it.
fn window(first: i64, end: i64) -> HistoryQuery {
    HistoryQuery {
        lower: Some(LowerBound::From(seconds(first))),
        upper: Some(seconds(end)),
        ..all()
    }
}

#[test]
fn the_upper_bound_leaves_out_the_records_stamped_on_it_and_after_it() {
    on_both_backends(|s| {
        ten_records(s);

        let before_6 = HistoryQuery {
            upper: Some(seconds(6)),
            ..all()
        };
        let page = run(s, &before_6);
        assert_eq!(ids(&page), ["r0", "r1", "r2", "r3", "r4", "r5"]);
        assert_eq!(page.total, Some(6));
        assert_eq!(page.malformed, Some(0));

        // With a lower bound it closes a window, either kind of lower bound.
        assert_eq!(ids(&run(s, &window(2, 6))), ["r2", "r3", "r4", "r5"]);
        let after = HistoryQuery {
            lower: Some(LowerBound::After(seconds(2))),
            ..before_6
        };
        let page = run(s, &after);
        assert_eq!(ids(&page), ["r3", "r4", "r5"]);
        assert_eq!(page.total, Some(3));

        // A bound before everything, on the first record, and after everything.
        for (end, total) in [(-1, 0), (0, 0), (1, 1), (10, 10), (11, 10), (1_000, 10)] {
            let q = HistoryQuery {
                upper: Some(seconds(end)),
                ..all()
            };
            let page = run(s, &q);
            assert_eq!(page.total, Some(total), "before {end}");
            assert_eq!(page.records.len() as u64, total, "before {end}");
        }
    });
}

#[test]
fn an_empty_window_has_no_rows_and_counts_zero() {
    on_both_backends(|s| {
        ten_records(s);
        for filter in [
            HistoryFilter::default(),
            HistoryFilter {
                hide_malformed: true,
                ..Default::default()
            },
        ] {
            // Between two records, and where the bounds meet.
            for (first, end) in [(3, 3), (20, 30)] {
                let q = HistoryQuery {
                    filter: filter.clone(),
                    ..window(first, end)
                };
                let page = run(s, &q);
                assert!(page.records.is_empty(), "{first}..{end}");
                assert_eq!(page.total, Some(0), "{first}..{end}");
                assert_eq!(page.malformed, Some(0), "{first}..{end}");
            }
        }
    });
}

#[test]
fn the_upper_bound_applies_to_the_total_and_to_the_malformed_count() {
    on_both_backends(|s| {
        // v1@1 m1@2 m2@3 m3@4 v2@5; m1 and m2 carry gateway 1, m3 gateway 2.
        records_with_malformed(s);

        // Time bounds only: the counts come from the index and a range scan.
        let shown = run(s, &window(0, 4));
        assert_eq!(ids(&shown), ["v1", "m1", "m2"]);
        assert_eq!(shown.total, Some(3));
        assert_eq!(shown.malformed, Some(2));

        // Row filters: one scan counts both.  The malformed number is the
        // same whether the box hides them or not.
        for hide in [false, true] {
            let q = HistoryQuery {
                filter: HistoryFilter {
                    hide_malformed: hide,
                    status: Some(Status::Forwarded),
                    ..Default::default()
                },
                ..window(0, 4)
            };
            let page = run(s, &q);
            assert_eq!(
                ids(&page),
                if hide {
                    vec!["v1"]
                } else {
                    vec!["v1", "m1", "m2"]
                },
                "hide={hide}"
            );
            assert_eq!(page.total, Some(if hide { 1 } else { 3 }), "hide={hide}");
            assert_eq!(page.malformed, Some(2), "hide={hide}");
        }

        // m3 is the only record of gateway 2 below 5, and it sits at 4.
        for (end, total) in [(4, 0), (5, 1)] {
            let q = HistoryQuery {
                filter: HistoryFilter {
                    gateway: Some(G2.to_string()),
                    ..Default::default()
                },
                ..window(0, end)
            };
            let page = run(s, &q);
            assert_eq!(page.total, Some(total), "before {end}");
            assert_eq!(page.malformed, Some(total), "before {end}");
        }
    });
}

#[test]
fn the_upper_bound_applies_to_the_device_text_and_to_labels() {
    on_both_backends(|s| {
        mixed_records(s);
        label(s, "0A000001", "kitchen");
        // 0A000001 has r1 and r4; the window keeps only r1.
        let q = HistoryQuery {
            filter: HistoryFilter {
                device: Some("KITCHEN".to_string()),
                ..Default::default()
            },
            ..window(0, 4)
        };
        let page = run(s, &q);
        assert_eq!(ids(&page), ["r1"]);
        assert_eq!(page.total, Some(1));
        let q = HistoryQuery {
            filter: HistoryFilter {
                device: Some("a00000".to_string()),
                mtypes: vec![MType::UnconfirmedDataUp],
                ..Default::default()
            },
            ..window(2, 6)
        };
        assert_eq!(ids(&run(s, &q)), ["r4"]);
    });
}

#[test]
fn the_newest_rows_of_a_window_are_the_newest_of_the_window() {
    on_both_backends(|s| {
        store(s, (0..30).map(|n| rec(&format!("r{n:02}"), n)));
        // The window holds 15 rows, r05 to r19, more than the limit, and 10
        // newer rows lie outside it.
        for filter in [
            HistoryFilter::default(),
            HistoryFilter {
                status: Some(Status::Forwarded),
                ..Default::default()
            },
        ] {
            let newest = HistoryQuery {
                filter: filter.clone(),
                limit: 4,
                ..window(5, 20)
            };
            let page = run(s, &newest);
            assert_eq!(ids(&page), ["r16", "r17", "r18", "r19"], "{filter:?}");
            assert_eq!(page.total, Some(15), "{filter:?}");

            let oldest = HistoryQuery {
                newest: false,
                ..newest
            };
            let page = run(s, &oldest);
            assert_eq!(ids(&page), ["r05", "r06", "r07", "r08"], "{filter:?}");
            assert_eq!(page.total, Some(15), "{filter:?}");
        }

        // With only the upper bound, the newest rows before it.
        let q = HistoryQuery {
            upper: Some(seconds(20)),
            limit: 3,
            ..all()
        };
        let page = run(s, &q);
        assert_eq!(ids(&page), ["r17", "r18", "r19"]);
        assert_eq!(page.total, Some(20));
    });
}

#[test]
fn the_counts_of_a_window_equal_direct_counts() {
    on_both_backends(|s| {
        records_with_malformed(s);
        bulk(s, 100, 500);
        store(
            s,
            (0..40).map(|n| malformed_rec(&format!("late{n}"), 700 + n, Malformed::Length)),
        );
        let (first, end) = (seconds(3), seconds(650));
        let q = HistoryQuery {
            limit: 10,
            ..window(3, 650)
        };
        let page = run(s, &q);

        let conn = s.conn.lock().unwrap();
        let direct = |extra: &str| -> u64 {
            conn.query_row(
                &format!(
                    "SELECT COUNT(*) FROM messages WHERE timestamp >= ?1 AND timestamp < ?2{extra}"
                ),
                params![first.to_rfc3339(), end.to_rfc3339()],
                |r| r.get::<_, i64>(0),
            )
            .unwrap() as u64
        };
        assert_eq!(page.total, Some(direct("")));
        assert_eq!(page.total, Some(503));
        assert_eq!(page.malformed, Some(direct(" AND malformed IS NOT NULL")));
        assert_eq!(page.malformed, Some(2));
        assert_eq!(page.records.len(), 10);
        assert_eq!(page.records.last().unwrap().id, "bulk-599");
    });
}

/// The same instant spelled with 0, 3, 6 and 9 fraction digits: the forms
/// `to_rfc3339` writes.  `base` is whole seconds after [`t0`].
fn spellings(base: i64, prefix: &str, nanos: [u32; 4]) -> Vec<(String, DateTime<Utc>)> {
    ["whole", "milli", "micro", "nano"]
        .iter()
        .zip(nanos)
        .map(|(form, nanos)| (format!("{prefix}-{form}"), moment(base, nanos)))
        .collect()
}

#[test]
fn a_whole_second_bound_splits_the_stored_fraction_lengths_at_the_second() {
    on_both_backends(|s| {
        // A bound on 10:00:00 against records of the second before it and of
        // the second that starts at it, each spelled with 0, 3, 6 and 9
        // fraction digits.  The first second is wholly before the bound, the
        // second wholly at or after it.
        let before_the_second = spellings(-1, "prev", [0, 999_000_000, 999_999_000, 999_999_999]);
        let in_the_second = spellings(0, "at", [0, 500_000_000, 500_001_000, 500_000_001]);
        let after_the_second = spellings(1, "next", [0, 1_000_000, 1_000, 1]);
        let stamps: Vec<_> = before_the_second
            .iter()
            .chain(&in_the_second)
            .chain(&after_the_second)
            .cloned()
            .collect();
        store(
            s,
            stamps
                .iter()
                .map(|(id, at)| edit(rec(id, 0), |r| r.timestamp = *at)),
        );
        let stored: Vec<(String, String)> = s
            .conn
            .lock()
            .unwrap()
            .prepare("SELECT id, timestamp FROM messages ORDER BY timestamp")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let text = |id: &str| {
            stored
                .iter()
                .find(|(i, _)| i == id)
                .map(|(_, t)| t.as_str())
                .unwrap()
        };
        assert_eq!(text("prev-whole"), "2026-03-01T09:59:59+00:00");
        assert_eq!(text("prev-milli"), "2026-03-01T09:59:59.999+00:00");
        assert_eq!(text("prev-micro"), "2026-03-01T09:59:59.999999+00:00");
        assert_eq!(text("prev-nano"), "2026-03-01T09:59:59.999999999+00:00");
        assert_eq!(text("at-whole"), "2026-03-01T10:00:00+00:00");
        assert_eq!(text("at-milli"), "2026-03-01T10:00:00.500+00:00");
        assert_eq!(text("at-micro"), "2026-03-01T10:00:00.500001+00:00");
        assert_eq!(text("at-nano"), "2026-03-01T10:00:00.500000001+00:00");

        let names = |list: &[(String, DateTime<Utc>)]| -> Vec<String> {
            list.iter().map(|(id, _)| id.clone()).collect()
        };
        let sorted_ids = |q: &HistoryQuery| -> Vec<String> {
            let mut got: Vec<String> = run(s, q).records.into_iter().map(|r| r.id).collect();
            got.sort();
            got
        };
        let sorted = |mut list: Vec<String>| {
            list.sort();
            list
        };

        // Everything before the bound: the second before it, every spelling,
        // and nothing from the second that starts at it.
        let before = HistoryQuery {
            upper: Some(moment(0, 0)),
            ..all()
        };
        let page = run(s, &before);
        assert_eq!(sorted_ids(&before), sorted(names(&before_the_second)));
        assert_eq!(page.total, Some(4));

        // The two windows that meet at the bound share no record and lose none.
        let earlier = HistoryQuery {
            lower: Some(LowerBound::From(moment(-1, 0))),
            upper: Some(moment(0, 0)),
            ..all()
        };
        let later = HistoryQuery {
            lower: Some(LowerBound::From(moment(0, 0))),
            upper: Some(moment(1, 0)),
            ..all()
        };
        assert_eq!(sorted_ids(&earlier), sorted(names(&before_the_second)));
        assert_eq!(sorted_ids(&later), sorted(names(&in_the_second)));
        for (what, q) in [("earlier", &earlier), ("later", &later)] {
            assert_eq!(run(s, q).total, Some(4), "{what}");
            let filtered = HistoryQuery {
                filter: HistoryFilter {
                    status: Some(Status::Forwarded),
                    ..Default::default()
                },
                ..q.clone()
            };
            assert_eq!(
                sorted_ids(&filtered),
                sorted_ids(q),
                "{what}, with a filter"
            );
            assert_eq!(run(s, &filtered).total, Some(4), "{what}, with a filter");
        }
    });
}

#[test]
fn an_upper_bound_splits_the_list_at_its_own_position_for_every_fraction_length() {
    on_both_backends(|s| {
        // The same five instants as the lower bound test: whole second,
        // milliseconds, microseconds and nanoseconds, in time order.
        let stamps = [
            ("whole", moment(0, 0)),
            ("milli", moment(0, 500_000_000)),
            ("micro", moment(0, 500_001_000)),
            ("nano", moment(0, 500_000_001)),
            ("next", moment(1, 0)),
        ];
        store(
            s,
            stamps
                .iter()
                .map(|(id, at)| edit(rec(id, 0), |r| r.timestamp = *at)),
        );
        let before = |bound: DateTime<Utc>| {
            let q = HistoryQuery {
                upper: Some(bound),
                newest: false,
                ..all()
            };
            run(s, &q)
                .records
                .into_iter()
                .map(|r| r.id)
                .collect::<Vec<_>>()
        };
        let order = ["whole", "milli", "nano", "micro", "next"];
        for (k, id) in order.iter().enumerate() {
            let at = stamps.iter().find(|(i, _)| i == id).unwrap().1;
            assert_eq!(before(at), &order[..k], "before {id}");
        }
    });
}

#[test]
fn without_wal_a_window_is_answered_like_any_time_bound() {
    let tmp = TempDb::new("history-nowal-window");
    let s = Storage::open_without_wal(&tmp.path()).unwrap();
    records_with_malformed(&s);

    let q = HistoryQuery {
        newest: false,
        ..window(2, 5)
    };
    let page = run(&s, &q);
    assert_eq!(ids(&page), ["m1", "m2", "m3"]);
    assert_eq!(page.total, Some(3));
    assert_eq!(page.malformed, None);
}

#[test]
fn an_empty_range_has_no_rows_and_counts_zero() {
    on_both_backends(|s| {
        ten_records(s);
        for filter in [
            HistoryFilter::default(),
            HistoryFilter {
                hide_malformed: true,
                ..Default::default()
            },
        ] {
            let q = HistoryQuery {
                lower: Some(LowerBound::From(seconds(100))),
                filter,
                ..all()
            };
            let page = run(s, &q);
            assert!(page.records.is_empty());
            assert_eq!(page.total, Some(0));
            assert_eq!(page.malformed, Some(0));
        }
    });
}

#[test]
fn the_cap_limits_the_rows_but_not_the_count() {
    on_both_backends(|s| {
        store(s, (0..30).map(|n| rec(&format!("r{n:02}"), n)));
        for filter in [
            HistoryFilter::default(),
            HistoryFilter {
                status: Some(Status::Forwarded),
                ..Default::default()
            },
        ] {
            let q = HistoryQuery {
                filter,
                limit: 10,
                ..all()
            };
            let page = run(s, &q);
            assert_eq!(page.records.len(), 10);
            assert_eq!(page.records[0].id, "r20");
            assert_eq!(page.records[9].id, "r29");
            assert_eq!(page.total, Some(30), "{:?}", q.filter);
        }
    });
}

#[test]
fn a_limit_above_the_maximum_is_held_to_it() {
    on_both_backends(|s| {
        bulk(s, 0, MAX_HISTORY_ROWS as i64 + 20);
        let q = HistoryQuery {
            limit: MAX_HISTORY_ROWS + 1_000,
            ..all()
        };
        let page = run(s, &q);
        assert_eq!(page.records.len(), MAX_HISTORY_ROWS);
        assert_eq!(page.total, Some(MAX_HISTORY_ROWS as u64 + 20));
    });
}

#[test]
fn nothing_is_counted_unless_the_request_asks() {
    on_both_backends(|s| {
        ten_records(s);
        for filter in [
            HistoryFilter::default(),
            HistoryFilter {
                status: Some(Status::Forwarded),
                ..Default::default()
            },
        ] {
            let q = HistoryQuery {
                filter,
                count: false,
                limit: 4,
                ..all()
            };
            let page = run(s, &q);
            assert_eq!(ids(&page), ["r6", "r7", "r8", "r9"]);
            assert_eq!(page.total, None);
            assert_eq!(page.malformed, None);
        }
    });
}

#[test]
fn records_with_the_same_timestamp_come_back_in_the_order_they_were_stored() {
    on_both_backends(|s| {
        store(s, ["a", "b", "c", "d"].map(|id| rec(id, 7)));
        for newest in [true, false] {
            let plain = HistoryQuery { newest, ..all() };
            let filtered = HistoryQuery {
                newest,
                ..with_filter(|f| f.status = Some(Status::Forwarded))
            };
            assert_eq!(
                ids(&run(s, &plain)),
                ["a", "b", "c", "d"],
                "newest={newest}"
            );
            assert_eq!(
                ids(&run(s, &filtered)),
                ["a", "b", "c", "d"],
                "newest={newest}"
            );
        }
    });
}

// ---------------------------------------------------------------------------
// Filters
// ---------------------------------------------------------------------------

fn mixed_records(s: &Storage) {
    store(
        s,
        [
            edit(data_rec("r1", 1, "0A000001"), |r| {
                r.gateway_eui = Some(G1.to_string())
            }),
            edit(data_rec("r2", 2, "0A000002"), |r| {
                r.mtype = MType::ConfirmedDataUp;
                r.gateway_eui = Some(G2.to_string());
            }),
            edit(join_rec("r3", 3, "70:B3:D5:7E:D0:00:00:03"), |r| {
                r.gateway_eui = Some(G1.to_string())
            }),
            edit(data_rec("r4", 4, "0A000001"), |r| {
                r.status = Status::Blocked;
                r.gateway_eui = Some(G1.to_string());
            }),
            edit(data_rec("r5", 5, "0A000002"), |r| {
                r.mtype = MType::UnconfirmedDataDown;
                r.direction = Direction::Down;
                r.status = Status::Captured;
                r.gateway_eui = Some(G2.to_string());
            }),
            data_rec("r6", 6, "0A000003"),
        ],
    );
}

#[test]
fn each_filter_alone_keeps_exactly_its_records() {
    on_both_backends(|s| {
        mixed_records(s);
        let cases: Vec<(&str, HistoryQuery, Vec<&str>)> = vec![
            (
                "one type",
                with_filter(|f| f.mtypes = vec![MType::UnconfirmedDataUp]),
                vec!["r1", "r4", "r6"],
            ),
            (
                "two types",
                with_filter(|f| f.mtypes = vec![MType::JoinRequest, MType::ConfirmedDataUp]),
                vec!["r2", "r3"],
            ),
            (
                "blocked",
                with_filter(|f| f.status = Some(Status::Blocked)),
                vec!["r4"],
            ),
            (
                "captured",
                with_filter(|f| f.status = Some(Status::Captured)),
                vec!["r5"],
            ),
            (
                "forwarded",
                with_filter(|f| f.status = Some(Status::Forwarded)),
                vec!["r1", "r2", "r3", "r6"],
            ),
            (
                "gateway 1",
                with_filter(|f| f.gateway = Some(G1.to_string())),
                vec!["r1", "r3", "r4"],
            ),
            (
                "gateway 2",
                with_filter(|f| f.gateway = Some(G2.to_string())),
                vec!["r2", "r5"],
            ),
            (
                "device",
                device("a00000"),
                vec!["r1", "r2", "r4", "r5", "r6"],
            ),
            ("device", device("000001"), vec!["r1", "r4"]),
            (
                "empty device and gateway text",
                with_filter(|f| {
                    f.device = Some(String::new());
                    f.gateway = Some(String::new());
                }),
                vec!["r1", "r2", "r3", "r4", "r5", "r6"],
            ),
        ];
        for (what, q, expected) in cases {
            let page = run(s, &q);
            assert_eq!(ids(&page), expected, "{what}");
            assert_eq!(page.total, Some(expected.len() as u64), "{what}");
        }
    });
}

#[test]
fn the_filters_combine_with_and() {
    on_both_backends(|s| {
        mixed_records(s);
        let q = with_filter(|f| {
            f.device = Some("0A".to_string());
            f.mtypes = vec![MType::UnconfirmedDataUp, MType::ConfirmedDataUp];
            f.status = Some(Status::Forwarded);
            f.gateway = Some(G1.to_string());
        });
        let page = run(s, &q);
        assert_eq!(ids(&page), ["r1"]);
        assert_eq!(page.total, Some(1));

        // A time bound narrows it further.
        let q = HistoryQuery {
            lower: Some(LowerBound::From(seconds(2))),
            ..q
        };
        assert!(run(s, &q).records.is_empty());
    });
}

#[test]
fn the_device_text_matches_a_devaddr_or_a_deveui_in_any_case() {
    on_both_backends(|s| {
        store(
            s,
            [
                data_rec("a", 1, "0A1B2C3D"),
                join_rec("b", 2, "70:B3:D5:7E:D0:00:12:34"),
                data_rec("c", 3, "FF00FF00"),
            ],
        );
        let cases = [
            ("0a1b", vec!["a"]),
            ("0A1B2C3D", vec!["a"]),
            ("2c3d", vec!["a"]),
            ("b3:d5", vec!["b"]),
            ("B3:D5:7E", vec!["b"]),
            (":00:12:34", vec!["b"]),
            ("ff00", vec!["c"]),
            // The colons belong to the text: without them nothing matches.
            ("70b3d5", vec![]),
            ("0", vec!["a", "b", "c"]),
        ];
        for (needle, expected) in cases {
            assert_eq!(ids(&run(s, &device(needle))), expected, "{needle}");
        }
    });
}

#[test]
fn a_percent_or_underscore_in_the_text_is_taken_literally() {
    on_both_backends(|s| {
        store(
            s,
            [
                data_rec("a", 1, "0A1B2C3D"),
                join_rec("b", 2, "70:B3:D5:7E:D0:00:12:34"),
                data_rec("c", 3, "FF00FF00"),
            ],
        );
        label(s, "FF00FF00", "100% sure");
        label(s, "0A1B2C3D", "a_b");
        label(s, "70:B3:D5:7E:D0:00:12:34", "axb");
        for (needle, expected) in [
            ("%", vec!["c"]),
            ("0% s", vec!["c"]),
            ("a_b", vec!["a"]),
            ("_", vec!["a"]),
            ("0A_B", vec![]),
            ("%0A", vec![]),
        ] {
            assert_eq!(ids(&run(s, &device(needle))), expected, "{needle}");
        }
    });
}

#[test]
fn text_that_matches_nothing_gives_an_empty_page_and_a_zero_count() {
    on_both_backends(|s| {
        mixed_records(s);
        label(s, "0A000001", "kitchen");
        let page = run(s, &device("no such device"));
        assert!(page.records.is_empty());
        assert_eq!(page.total, Some(0));
        assert_eq!(page.malformed, Some(0));
    });
}

#[test]
fn the_device_text_matches_the_label_of_the_identifier() {
    on_both_backends(|s| {
        store(
            s,
            [
                data_rec("addr", 1, "0A000001"),
                data_rec("addr-2", 2, "0A000001"),
                data_rec("other", 3, "0A000002"),
                join_rec("eui", 4, "70:B3:D5:7E:D0:00:00:03"),
            ],
        );
        label(s, "0A000001", "Havana Kitchen");
        label(s, "70:B3:D5:7E:D0:00:00:03", "Havana roof");
        label(s, "0A000002", "Lisbon");
        let cases = [
            ("havana", vec!["addr", "addr-2", "eui"]),
            ("HAVANA", vec!["addr", "addr-2", "eui"]),
            ("kitchen", vec!["addr", "addr-2"]),
            ("a ki", vec!["addr", "addr-2"]),
            ("roof", vec!["eui"]),
            ("lisbon", vec!["other"]),
            ("paris", vec![]),
        ];
        for (needle, expected) in cases {
            assert_eq!(ids(&run(s, &device(needle))), expected, "{needle}");
        }
    });
}

#[test]
fn labels_fold_case_the_way_the_browser_does() {
    on_both_backends(|s| {
        store(
            s,
            [
                data_rec("nandu", 1, "0A000001"),
                data_rec("istanbul", 2, "0A000002"),
                data_rec("sigma", 3, "0A000003"),
                data_rec("sharp", 4, "0A000004"),
            ],
        );
        label(s, "0A000001", "ÑANDÚ quiet sensor");
        label(s, "0A000002", "İstanbul office");
        label(s, "0A000003", "ΟΔΟΣ ΟΔΟΣ");
        label(s, "0A000004", "STRAẞE");
        let cases = [
            ("ñandú", vec!["nandu"]),
            ("ÑANDÚ", vec!["nandu"]),
            ("Ñandú QUIET", vec!["nandu"]),
            // Without the accents it is a different word, as in the browser.
            ("nandu", vec![]),
            ("i\u{307}stanbul", vec!["istanbul"]),
            ("İSTANBUL", vec!["istanbul"]),
            // `İ` lower-cases to two code points, so plain `i` does not match.
            ("istanbul", vec![]),
            ("οδος", vec!["sigma"]),
            ("ΟΔΟΣ", vec!["sigma"]),
            ("οδος ο", vec!["sigma"]),
            ("οδοσ", vec![]),
            ("straße", vec!["sharp"]),
            ("STRAẞE", vec!["sharp"]),
        ];
        for (needle, expected) in cases {
            assert_eq!(ids(&run(s, &device(needle))), expected, "{needle}");
        }
    });
}

#[test]
fn a_label_on_the_deveui_does_not_match_a_record_that_has_a_devaddr() {
    on_both_backends(|s| {
        // The record's identifier is its DevAddr, so only a label on the
        // DevAddr belongs to it.
        store(
            s,
            [
                edit(data_rec("both", 1, "0A000001"), |r| {
                    r.dev_eui = Some("70:B3:D5:7E:D0:00:00:03".to_string())
                }),
                join_rec("eui-only", 2, "70:B3:D5:7E:D0:00:00:03"),
            ],
        );
        label(s, "70:B3:D5:7E:D0:00:00:03", "roof");
        assert_eq!(ids(&run(s, &device("roof"))), ["eui-only"]);

        label(s, "0A000001", "pantry");
        assert_eq!(ids(&run(s, &device("pantry"))), ["both"]);
        assert_eq!(ids(&run(s, &device("roof"))), ["eui-only"]);
    });
}

#[test]
fn a_quiet_device_beyond_thousands_of_newer_rows_is_found() {
    on_both_backends(|s| {
        let quiet_eui = "70:B3:D5:7E:D0:00:99:99";
        store(
            s,
            [
                data_rec("q1", 1, "26AABBCC"),
                data_rec("q2", 2, "26AABBCC"),
                data_rec("q3", 3, "26AABBCC"),
                join_rec("e1", 4, quiet_eui),
                join_rec("e2", 5, quiet_eui),
                join_rec("e3", 6, quiet_eui),
            ],
        );
        label(s, "26AABBCC", "Ñandú quiet sensor");
        bulk(s, 100, 6_010);

        // The newest 5,000 of everything do not reach them.
        let newest = run(s, &all());
        assert!(newest.records.iter().all(|r| r.id.starts_with("bulk-")));

        for (needle, expected) in [
            ("26aabbcc", ["q1", "q2", "q3"]),
            ("26AABB", ["q1", "q2", "q3"]),
            ("d0:00:99", ["e1", "e2", "e3"]),
            ("ñandú quiet", ["q1", "q2", "q3"]),
        ] {
            let page = run(s, &device(needle));
            assert_eq!(ids(&page), expected, "{needle}");
            assert_eq!(page.total, Some(3), "{needle}");
        }
    });
}

// ---------------------------------------------------------------------------
// Malformed records
// ---------------------------------------------------------------------------

fn records_with_malformed(s: &Storage) {
    store(
        s,
        [
            edit(data_rec("v1", 1, "0A000001"), |r| {
                r.gateway_eui = Some(G1.to_string())
            }),
            edit(malformed_rec("m1", 2, Malformed::Crc), |r| {
                r.gateway_eui = Some(G1.to_string())
            }),
            edit(malformed_rec("m2", 3, Malformed::Length), |r| {
                r.gateway_eui = Some(G1.to_string())
            }),
            edit(malformed_rec("m3", 4, Malformed::Mhdr), |r| {
                r.gateway_eui = Some(G2.to_string())
            }),
            edit(data_rec("v2", 5, "0A000002"), |r| {
                r.gateway_eui = Some(G2.to_string())
            }),
        ],
    );
}

#[test]
fn the_malformed_count_is_the_same_whether_they_are_hidden_or_shown() {
    on_both_backends(|s| {
        records_with_malformed(s);

        let hidden = run(s, &with_filter(|f| f.hide_malformed = true));
        assert_eq!(ids(&hidden), ["v1", "v2"]);
        assert_eq!(hidden.total, Some(2));
        assert_eq!(hidden.malformed, Some(3));

        // Shown, they are rows and are in the total; the count is unchanged.
        // The state of the box does not make this a row-filtered query, so it
        // takes the time-only path, and the number must still agree.
        let shown = run(s, &all());
        assert_eq!(ids(&shown), ["v1", "m1", "m2", "m3", "v2"]);
        assert_eq!(shown.total, Some(5));
        assert_eq!(shown.malformed, Some(3));

        // The same other filters apply to both.
        for hide in [true, false] {
            let page = run(
                s,
                &with_filter(|f| {
                    f.gateway = Some(G1.to_string());
                    f.hide_malformed = hide;
                }),
            );
            assert_eq!(page.malformed, Some(2), "hide={hide}");
            assert_eq!(page.total, Some(if hide { 1 } else { 3 }), "hide={hide}");
        }
    });
}

#[test]
fn malformed_records_in_a_filtered_scan_are_skipped_only_when_hidden() {
    on_both_backends(|s| {
        records_with_malformed(s);
        let q = with_filter(|f| f.status = Some(Status::Forwarded));
        let shown = run(s, &q);
        assert_eq!(ids(&shown), ["v1", "m1", "m2", "m3", "v2"]);
        assert_eq!(shown.total, Some(5));
        assert_eq!(shown.malformed, Some(3));

        let q = with_filter(|f| {
            f.status = Some(Status::Forwarded);
            f.hide_malformed = true;
        });
        let hidden = run(s, &q);
        assert_eq!(ids(&hidden), ["v1", "v2"]);
        assert_eq!(hidden.total, Some(2));
        assert_eq!(hidden.malformed, Some(3));
    });
}

#[test]
fn the_cap_applies_to_the_rows_that_are_left_after_hiding_malformed() {
    on_both_backends(|s| {
        // Newest first: 4 malformed, then 6 valid.  Hiding them leaves the cap
        // to fill from the valid ones.
        store(
            s,
            (0..6)
                .map(|n| rec(&format!("v{n}"), n))
                .chain((6..10).map(|n| malformed_rec(&format!("m{n}"), n, Malformed::Crc))),
        );
        let q = HistoryQuery {
            limit: 3,
            ..with_filter(|f| f.hide_malformed = true)
        };
        let page = run(s, &q);
        assert_eq!(ids(&page), ["v3", "v4", "v5"]);
        assert_eq!(page.total, Some(6));
        assert_eq!(page.malformed, Some(4));
    });
}

#[test]
fn the_time_only_malformed_count_equals_a_direct_count() {
    on_both_backends(|s| {
        records_with_malformed(s);
        bulk(s, 100, 500);
        store(
            s,
            (0..40).map(|n| malformed_rec(&format!("late{n}"), 700 + n, Malformed::Length)),
        );
        let from = seconds(3);
        let q = HistoryQuery {
            lower: Some(LowerBound::From(from)),
            limit: 10,
            ..all()
        };
        let page = run(s, &q);

        let conn = s.conn.lock().unwrap();
        let direct = |extra: &str| -> u64 {
            conn.query_row(
                &format!("SELECT COUNT(*) FROM messages WHERE timestamp >= ?1{extra}"),
                params![from.to_rfc3339()],
                |r| r.get::<_, i64>(0),
            )
            .unwrap() as u64
        };
        assert_eq!(page.malformed, Some(direct(" AND malformed IS NOT NULL")));
        assert_eq!(page.malformed, Some(42));
        assert_eq!(page.total, Some(direct("")));
    });
}

#[test]
fn rows_an_older_version_stored_are_returned_and_counted_like_checked_rows() {
    on_both_backends(|s| {
        records_with_malformed(s);
        // An older version's INSERT does not list `classified`, so its rows
        // read 0.  Whether that is so for some rows or for all of them, the
        // answers do not change.
        let answers = |s: &Storage| {
            let queries = [
                all(),
                with_filter(|f| f.hide_malformed = true),
                with_filter(|f| f.status = Some(Status::Forwarded)),
                device("0A00"),
            ];
            queries
                .iter()
                .map(|q| {
                    let page = run(s, q);
                    (
                        page.records
                            .iter()
                            .map(|r| r.id.clone())
                            .collect::<Vec<_>>(),
                        page.total,
                        page.malformed,
                    )
                })
                .collect::<Vec<_>>()
        };
        let checked = answers(s);
        s.conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE messages SET classified = 0 WHERE id IN ('v1', 'm2', 'v2')",
                [],
            )
            .unwrap();
        assert_eq!(answers(s), checked);
        s.conn
            .lock()
            .unwrap()
            .execute("UPDATE messages SET classified = 0", [])
            .unwrap();
        assert_eq!(answers(s), checked);
    });
}

// ---------------------------------------------------------------------------
// The connections
// ---------------------------------------------------------------------------

fn file_storage(tmp: &TempDb) -> Storage {
    let s = Storage::open(&tmp.path()).unwrap();
    assert!(s.reader.is_some());
    s
}

#[test]
fn the_reader_sees_a_record_the_writer_commits_after_it_was_opened() {
    let tmp = TempDb::new("history-visible");
    let s = file_storage(&tmp);
    assert_eq!(run(&s, &all()).total, Some(0));

    s.insert(&rec("first", 1)).unwrap();
    assert_eq!(ids(&run(&s, &all())), ["first"]);
    assert_eq!(ids(&run(&s, &device("01ABCDEF"))), ["first"]);

    s.insert(&rec("second", 2)).unwrap();
    assert_eq!(ids(&run(&s, &device("01ABCDEF"))), ["first", "second"]);
}

#[test]
fn the_reader_refuses_to_write() {
    let tmp = TempDb::new("history-readonly");
    let s = file_storage(&tmp);
    let reader = s.reader.as_ref().unwrap().lock().unwrap();
    let err = insert_record(&reader, &rec("nope", 1)).unwrap_err();
    assert!(
        err.to_string().contains("readonly") || err.to_string().contains("read-only"),
        "{err}"
    );
    let err = reader.execute("DELETE FROM messages", []).unwrap_err();
    assert!(!err.to_string().is_empty());
}

#[test]
fn a_history_query_does_not_wait_for_the_writer() {
    let tmp = TempDb::new("history-independent");
    let s = file_storage(&tmp);
    store(&s, (0..50).map(|n| rec(&format!("r{n}"), n)));

    // Hold the writer's connection, as an insert or a sweep does.  A query on
    // the reader goes through; one on the writer would wait.
    let _writer = s.conn.lock().unwrap();
    let started = Instant::now();
    let page = run(&s, &device("01AB"));
    assert_eq!(page.total, Some(50));
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn a_clean_close_leaves_no_log_files() {
    let tmp = TempDb::new("history-close");
    let path = tmp.path();
    let s = file_storage(&tmp);
    let clone = s.clone();
    store(&s, (0..20).map(|n| rec(&format!("r{n}"), n)));
    run(&clone, &device("01AB"));
    s.insert(&rec("late", 99)).unwrap();
    assert!(
        Path::new(&format!("{path}-wal")).exists(),
        "the log is in use"
    );

    drop(s);
    drop(clone);
    assert!(Path::new(&path).exists());
    assert!(
        !Path::new(&format!("{path}-wal")).exists(),
        "the log was left behind"
    );
    assert!(
        !Path::new(&format!("{path}-shm")).exists(),
        "the index was left behind"
    );
}

// ---------------------------------------------------------------------------
// Without WAL
// ---------------------------------------------------------------------------

#[test]
fn without_wal_there_is_no_reader_and_the_open_says_why() {
    let tmp = TempDb::new("history-nowal");
    let (s, lines) = log_capture::capture(|| Storage::open_without_wal(&tmp.path()));
    let s = s.unwrap();
    assert!(s.reader.is_none());
    assert!(
        lines.iter().any(|(level, text)| *level == Level::Warn
            && text.contains("not in WAL mode")
            && text.contains("history filters are unavailable")),
        "{lines:?}"
    );

    let wal = TempDb::new("history-wal");
    let (s, lines) = log_capture::capture(|| Storage::open(&wal.path()));
    assert!(s.unwrap().reader.is_some());
    assert!(
        !lines.iter().any(|(level, _)| *level == Level::Warn),
        "{lines:?}"
    );
}

#[test]
fn without_wal_only_a_time_bound_is_answered_and_the_malformed_count_is_absent() {
    let tmp = TempDb::new("history-nowal-queries");
    let s = Storage::open_without_wal(&tmp.path()).unwrap();
    records_with_malformed(&s);

    let page = run(&s, &all());
    assert_eq!(ids(&page), ["v1", "m1", "m2", "m3", "v2"]);
    assert_eq!(page.total, Some(5));
    assert_eq!(page.malformed, None);

    let q = HistoryQuery {
        lower: Some(LowerBound::After(seconds(2))),
        newest: false,
        count: false,
        ..all()
    };
    assert_eq!(ids(&run(&s, &q)), ["m2", "m3", "v2"]);

    let unavailable = |q: HistoryQuery| {
        let err = s.query_history(&q, flag()).unwrap_err();
        assert!(matches!(err, SnifferError::HistoryUnavailable), "{err:?}");
    };
    unavailable(device("0A"));
    unavailable(with_filter(|f| f.mtypes = vec![MType::JoinRequest]));
    unavailable(with_filter(|f| f.status = Some(Status::Blocked)));
    unavailable(with_filter(|f| f.gateway = Some(G1.to_string())));
    unavailable(with_filter(|f| f.hide_malformed = true));
}

#[test]
fn without_wal_a_capped_range_returns_the_newest_rows_malformed_included() {
    let tmp = TempDb::new("history-nowal-capped");
    let s = Storage::open_without_wal(&tmp.path()).unwrap();
    bulk(&s, 0, MAX_HISTORY_ROWS as i64 + 100);
    s.conn
        .lock()
        .unwrap()
        .execute(
            "UPDATE messages SET malformed = 'crc' WHERE rowid % 2 = 0",
            [],
        )
        .unwrap();

    let page = run(&s, &all());
    assert_eq!(page.records.len(), MAX_HISTORY_ROWS);
    assert_eq!(page.total, Some(MAX_HISTORY_ROWS as u64 + 100));
    assert_eq!(page.malformed, None);
    let malformed = page
        .records
        .iter()
        .filter(|r| r.malformed.is_some())
        .count();
    assert_eq!(malformed, MAX_HISTORY_ROWS / 2);
    // The newest rows, whatever they are: hiding the malformed ones is the
    // browser's job here, after the cap.
    assert_eq!(
        page.records.last().unwrap().id,
        format!("bulk-{}", MAX_HISTORY_ROWS + 99)
    );
    assert_eq!(page.records[0].id, "bulk-100");
}

// ---------------------------------------------------------------------------
// Cancellation and busy
// ---------------------------------------------------------------------------

#[test]
fn a_set_flag_gives_cancelled_and_the_next_query_works() {
    on_both_backends(|s| {
        bulk(s, 0, 20_000);
        let cancelled = Arc::new(AtomicBool::new(true));
        for q in [
            all(),
            device("zzzz"),
            with_filter(|f| f.hide_malformed = true),
        ] {
            let err = s.query_history(&q, cancelled.clone()).unwrap_err();
            assert!(matches!(err, SnifferError::Cancelled), "{err:?}");
        }
        let page = run(s, &device("2600"));
        assert_eq!(page.total, Some(20_000));
    });
}

/// A device text no record has: every row is read and none is kept.
fn no_match() -> HistoryQuery {
    device("zzzz")
}

#[test]
fn a_running_scan_stops_soon_after_the_flag_is_set() {
    on_both_backends(|s| {
        bulk(s, 0, 200_000);

        let started = Instant::now();
        let full = run(s, &no_match());
        let full_time = started.elapsed();
        assert_eq!(full.total, Some(0));
        assert!(
            full_time > Duration::from_millis(40),
            "the scan took only {full_time:?}: too fast to cancel in the middle"
        );

        let cancel = flag();
        let setter = {
            let cancel = cancel.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(20));
                cancel.store(true, Ordering::Relaxed);
                Instant::now()
            })
        };
        let started = Instant::now();
        let err = s.query_history(&no_match(), cancel).unwrap_err();
        let stopped = Instant::now();
        let set_at = setter.join().unwrap();
        assert!(matches!(err, SnifferError::Cancelled), "{err:?}");
        let lag = stopped.saturating_duration_since(set_at);
        println!("full scan {full_time:?}, stopped {lag:?} after the flag");
        assert!(
            stopped - started < Duration::from_millis(200),
            "stopped after {:?}",
            stopped - started
        );

        // On the in-memory database the query ran on the connection that
        // saves records.  A handler left behind, with its flag still set,
        // would interrupt the next statement big enough to reach a tick.
        let pruned = s
            .delete_older_than(seconds(100_000), OrphanPolicy::Keep)
            .unwrap();
        assert_eq!(pruned.messages, 100_000);

        // A new flag is not tied to the old one, which is still set.
        let page = s.query_history(&no_match(), flag()).unwrap();
        assert_eq!(page.total, Some(0));
        assert_eq!(run(s, &all()).total, Some(100_000));
    });
}

#[test]
fn a_finished_query_leaves_no_handler_on_the_connection() {
    on_both_backends(|s| {
        bulk(s, 0, 50_000);
        let flag = Arc::new(AtomicBool::new(false));
        let page = s.query_history(&no_match(), flag.clone()).unwrap();
        assert_eq!(page.total, Some(0));
        let page = s.query_history(&all(), flag.clone()).unwrap();
        assert_eq!(page.total, Some(50_000));

        // The requester goes away after the answer.  Nothing may notice.
        flag.store(true, Ordering::Relaxed);
        let pruned = s
            .delete_older_than(Utc::now() + chrono::Duration::days(1), OrphanPolicy::Keep)
            .unwrap();
        assert_eq!(pruned.messages, 50_000);
    });
}

#[test]
fn a_locked_database_gives_busy_after_about_a_second() {
    // Open the three connections before any of them touches the file, so that
    // the third can take the whole database: once a reader and a writer have
    // attached, no connection can switch to exclusive locking.
    let tmp = TempDb::new("history-busy");
    let path = tmp.path();
    drop(Storage::open(&path).unwrap());
    let s = Storage {
        reader: Some(Arc::new(Mutex::new(connect_reader(&path).unwrap()))),
        conn: Arc::new(Mutex::new(Connection::open(&path).unwrap())),
        filters_on_writer: false,
        probe: Arc::default(),
    };
    let holder = Connection::open(&path).unwrap();
    holder
        .pragma_update(None, "locking_mode", "EXCLUSIVE")
        .unwrap();
    holder.execute_batch("BEGIN IMMEDIATE").unwrap();
    insert_record(&holder, &rec("held", 1)).unwrap();

    let started = Instant::now();
    let err = s.query_history(&device("01AB"), flag()).unwrap_err();
    let waited = started.elapsed();
    assert!(matches!(err, SnifferError::Busy), "{err:?}");
    assert!(
        waited > Duration::from_millis(800) && waited < Duration::from_millis(2500),
        "waited {waited:?}"
    );

    // Once the lock is gone the same handle works.
    holder.execute_batch("COMMIT").unwrap();
    drop(holder);
    assert_eq!(run(&s, &device("01AB")).total, Some(1));
}

// ---------------------------------------------------------------------------
// Plans
// ---------------------------------------------------------------------------

/// The `detail` lines of `EXPLAIN QUERY PLAN` for `sql` with `bindings`.
pub(super) fn plan(
    conn: &Connection,
    sql: &str,
    bindings: &[(&str, &dyn rusqlite::ToSql)],
) -> Vec<String> {
    let mut stmt = conn
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .unwrap_or_else(|e| panic!("{e}: {sql}"));
    let rows = stmt
        .query_map(bindings, |r| r.get::<_, String>(3))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    rows
}

pub(super) fn plan_text(lines: &[String]) -> String {
    lines.join(" | ")
}

fn every_shape() -> Vec<(&'static str, HistoryQuery)> {
    let bound = Some(LowerBound::From(seconds(0)));
    let all_filters = HistoryFilter {
        device: Some("0A".to_string()),
        mtypes: vec![MType::JoinRequest, MType::UnconfirmedDataUp],
        status: Some(Status::Forwarded),
        gateway: Some(G1.to_string()),
        hide_malformed: true,
    };
    let mut shapes = vec![
        (
            "time only",
            HistoryQuery {
                lower: bound,
                ..all()
            },
        ),
        (
            "time window",
            HistoryQuery {
                lower: bound,
                upper: Some(seconds(10)),
                ..all()
            },
        ),
        (
            "time window, after",
            HistoryQuery {
                lower: Some(LowerBound::After(seconds(0))),
                upper: Some(seconds(10)),
                newest: false,
                ..all()
            },
        ),
        (
            "before only",
            HistoryQuery {
                upper: Some(seconds(10)),
                ..all()
            },
        ),
        (
            "time only, after",
            HistoryQuery {
                lower: Some(LowerBound::After(seconds(0))),
                newest: false,
                ..all()
            },
        ),
        ("all time", all()),
        ("device", with_filter(|f| f.device = Some("0A".to_string()))),
        ("type", with_filter(|f| f.mtypes = vec![MType::JoinRequest])),
        ("status", with_filter(|f| f.status = Some(Status::Blocked))),
        ("gateway", with_filter(|f| f.gateway = Some(G1.to_string()))),
        ("hide malformed", with_filter(|f| f.hide_malformed = true)),
    ];
    shapes.push((
        "everything",
        HistoryQuery {
            filter: all_filters.clone(),
            ..all()
        },
    ));
    shapes.push((
        "everything in a range",
        HistoryQuery {
            lower: bound,
            filter: all_filters.clone(),
            ..all()
        },
    ));
    shapes.push((
        "everything in a window",
        HistoryQuery {
            lower: bound,
            upper: Some(seconds(10)),
            filter: all_filters,
            ..all()
        },
    ));
    shapes
}

/// A file database with rows, so that the planner has something to plan for.
fn planning_database() -> (TempDb, Storage) {
    let tmp = TempDb::new("history-plans");
    let s = file_storage(&tmp);
    mixed_records(&s);
    records_with_malformed(&s);
    label(&s, "0A000001", "kitchen");
    (tmp, s)
}

#[test]
fn the_time_only_count_reads_an_index_alone() {
    let (_tmp, s) = planning_database();
    let conn = s.reader.as_ref().unwrap().lock().unwrap();
    for (name, q) in every_shape()
        .into_iter()
        .filter(|(_, q)| !q.filter.has_row_filter())
    {
        let c = Conditions::new(&q, None);
        let text = plan_text(&plan(&conn, &c.count_sql(), &c.bindings()));
        assert!(text.contains("COVERING INDEX"), "{name}: {text}");
        assert!(!text.contains("TEMP B-TREE"), "{name}: {text}");
        if q.lower.is_some() || q.upper.is_some() {
            assert!(
                text.contains("COVERING INDEX idx_messages_timestamp"),
                "{name}: {text}"
            );
        }
        // Over the whole table SQLite counts the entries of the narrowest
        // index whatever `INDEXED BY` says.  Every one of them holds every
        // row, so the number is the same.
    }
}

#[test]
fn the_time_only_malformed_count_is_a_range_search_that_reads_the_table() {
    let (_tmp, s) = planning_database();
    let conn = s.reader.as_ref().unwrap().lock().unwrap();
    let q = HistoryQuery {
        lower: Some(LowerBound::From(seconds(0))),
        ..all()
    };
    let c = Conditions::new(&q, None);
    let text = plan_text(&plan(&conn, &c.malformed_count_sql(), &c.bindings()));
    assert!(
        text.contains("SEARCH messages USING INDEX idx_messages_timestamp (timestamp>?)"),
        "{text}"
    );
    assert!(!text.contains("COVERING"), "{text}");
    assert!(!text.contains("TEMP B-TREE"), "{text}");
}

#[test]
fn a_window_is_one_range_search_on_the_timestamp_index() {
    let (_tmp, s) = planning_database();
    let conn = s.reader.as_ref().unwrap().lock().unwrap();
    for (name, q, searched) in [
        ("window", window(0, 10), "(timestamp>? AND timestamp<?)"),
        (
            "before only",
            HistoryQuery {
                upper: Some(seconds(10)),
                ..all()
            },
            "(timestamp<?)",
        ),
    ] {
        let c = Conditions::new(&q, None);
        let text = plan_text(&plan(&conn, &c.malformed_count_sql(), &c.bindings()));
        assert!(
            text.contains(&format!(
                "SEARCH messages USING INDEX idx_messages_timestamp {searched}"
            )),
            "{name}: {text}"
        );
        assert!(!text.contains("TEMP B-TREE"), "{name}: {text}");
        let text = plan_text(&plan(&conn, &c.count_sql(), &c.bindings()));
        assert!(
            text.contains(&format!("COVERING INDEX idx_messages_timestamp {searched}")),
            "{name}: {text}"
        );
    }
}

#[test]
fn page_and_scan_statements_follow_the_timestamp_index_without_sorting() {
    let (_tmp, s) = planning_database();
    let conn = s.reader.as_ref().unwrap().lock().unwrap();
    for (name, q) in every_shape() {
        for newest in [true, false] {
            let ids = q.filter.device_text().map(|_| "[\"0A000001\"]".to_string());
            let c = Conditions::new(&q, ids);
            let limit = 100i64;
            let mut with_limit = c.bindings();
            with_limit.push((":limit", &limit));
            let (what, sql, bindings) = if q.filter.has_row_filter() {
                ("scan", c.scan_sql(newest), c.bindings())
            } else {
                ("page", c.page_sql(newest), with_limit)
            };
            let text = plan_text(&plan(&conn, &sql, &bindings));
            assert!(
                text.contains("idx_messages_timestamp"),
                "{name} {what} newest={newest}: {text}"
            );
            assert!(
                !text.contains("TEMP B-TREE"),
                "{name} {what} newest={newest}: {text}"
            );
        }
    }
}

#[test]
fn the_fetch_by_row_number_searches_the_table_by_rowid() {
    let (_tmp, s) = planning_database();
    let conn = s.reader.as_ref().unwrap().lock().unwrap();
    let text = plan_text(&plan(&conn, &fetch_by_rowid_sql(), &[]));
    assert!(
        text.contains("SEARCH messages USING INTEGER PRIMARY KEY (rowid=?)"),
        "{text}"
    );
    // At most the cap's worth of rows is sorted here.
    assert!(!text.contains("SCAN messages"), "{text}");
}

#[test]
fn no_statement_uses_the_classification_index() {
    let (_tmp, s) = planning_database();
    // The index exists in a database that has been through a classification
    // pass; the plans must not depend on it.
    assert!(s
        .conn
        .lock()
        .unwrap()
        .query_row(UNCLASSIFIED_INDEX_EXISTS_SQL, [], |r| r.get::<_, bool>(0))
        .unwrap());
    let conn = s.reader.as_ref().unwrap().lock().unwrap();
    for (name, q) in every_shape() {
        let ids = q.filter.device_text().map(|_| "[]".to_string());
        let c = Conditions::new(&q, ids);
        let limit = 10i64;
        let mut with_limit = c.bindings();
        with_limit.push((":limit", &limit));
        for (sql, bindings) in [
            (c.count_sql(), c.bindings()),
            (c.malformed_count_sql(), c.bindings()),
            (c.scan_sql(true), c.bindings()),
            (c.page_sql(true), with_limit),
        ] {
            let text = plan_text(&plan(&conn, &sql, &bindings));
            assert!(!text.contains("messages_unclassified"), "{name}: {text}");
        }
    }
    assert!(!plan_text(&plan(&conn, &fetch_by_rowid_sql(), &[])).contains("messages_unclassified"));
}

#[test]
fn the_statements_hold_no_value_in_their_text() {
    let q = HistoryQuery {
        lower: Some(LowerBound::From(seconds(0))),
        filter: HistoryFilter {
            device: Some("'; DROP TABLE messages; --".to_string()),
            gateway: Some("x' OR '1'='1".to_string()),
            ..Default::default()
        },
        ..all()
    };
    let c = Conditions::new(&q, Some("[\"a\"]".to_string()));
    for sql in [
        c.count_sql(),
        c.malformed_count_sql(),
        c.scan_sql(true),
        c.page_sql(false),
    ] {
        assert!(!sql.contains("DROP") && !sql.contains("1'='1"), "{sql}");
        assert!(!sql.contains("2026"), "{sql}");
    }
    // And such text finds nothing, without harm.
    let tmp = TempDb::new("history-injection");
    let s = file_storage(&tmp);
    mixed_records(&s);
    assert_eq!(run(&s, &q).total, Some(0));
    assert_eq!(run(&s, &all()).total, Some(6));
}
