//! Tests of `GET /api/messages` through the router, and of what stands
//! behind it: the permits that bound the queries, the marker that makes a
//! reload wait for the storage writer, the failure count on every answer, and
//! the `/ws` feed a reload after a reconnect depends on.
//!
//! The in-process router cannot upgrade a connection, so the tests that need a
//! WebSocket run against a real listener on loopback.  Tests that hold the
//! storage writer still use a file database, whose history connection is
//! independent of the writer's lock: an in-memory database answers queries on
//! the writer's connection, so it could not answer while the writer is held.

// The tests stall the storage writer by holding its lock while they await the
// requests that wait for it; that is the point, not an oversight.
#![allow(clippy::await_holding_lock)]

use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use chrono::{DateTime, SecondsFormat, TimeZone, Utc};
use log::Level;
use serde_json::Value;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tower::ServiceExt;

use super::label_tests::{serve, WsClient};
use super::tests::{record_with_new_fields, send, Reply, Rig, Setup, DEFAULT_HOST};
use super::*;
use crate::core::error::SnifferError;
use crate::core::labels::MAX_LABELS;
use crate::core::linelog::{start_capture, take_capture};
use crate::core::metadata::{MType, Malformed, Status};
use crate::core::storage::TempDb;
use crate::proxy::Outputs;

/// Failure guard for waits that should end at once.  Nothing here sleeps to
/// let something happen; this only turns a hang into a failure.
const GUARD: Duration = Duration::from_secs(10);

/// Where the records of these tests start.
fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 30, 12, 0, 0).unwrap()
}

fn at(secs: i64) -> DateTime<Utc> {
    t0() + chrono::Duration::seconds(secs)
}

/// `at(secs)` as a query-string value.
fn stamp(secs: i64) -> String {
    at(secs).to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// A record of DevAddr `01ABCDEF`, `secs` seconds after [`t0`].
fn rec(id: &str, secs: i64) -> MessageRecord {
    MessageRecord {
        id: id.to_string(),
        timestamp: at(secs),
        ..record_with_new_fields()
    }
}

fn edit(mut record: MessageRecord, change: impl FnOnce(&mut MessageRecord)) -> MessageRecord {
    change(&mut record);
    record
}

/// The record of a frame that cannot be a LoRaWAN frame: its reason set and
/// everything read from its body gone.
fn malformed(id: &str, secs: i64, reason: Malformed) -> MessageRecord {
    edit(rec(id, secs), |r| {
        r.malformed = Some(reason);
        r.mtype = MType::Unknown;
        r.dev_addr = None;
        r.fcnt = None;
        r.fport = None;
    })
}

/// `count` records `r0000`, `r0001`, ..., one a second from [`t0`].
fn series(count: i64) -> Vec<MessageRecord> {
    (0..count).map(|n| rec(&format!("r{n:04}"), n)).collect()
}

async fn get(app: &Router, uri: &str) -> Reply {
    send(app, "GET", uri, &[], None).await
}

/// The ids in the body of a reply, in order.
fn ids(reply: &Reply) -> Vec<String> {
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    reply
        .json()
        .as_array()
        .unwrap_or_else(|| panic!("not an array: {}", reply.text()))
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_string())
        .collect()
}

fn names(wanted: &[&str]) -> Vec<String> {
    wanted.iter().map(|s| s.to_string()).collect()
}

/// An error answer: status, JSON body with a message that contains `needle`.
fn assert_error(reply: &Reply, status: StatusCode, needle: &str) {
    assert_eq!(reply.status, status, "{}", reply.text());
    assert!(
        reply
            .header("content-type")
            .is_some_and(|t| t.starts_with("application/json")),
        "{:?}",
        reply.header("content-type")
    );
    let error = reply.json()["error"]
        .as_str()
        .unwrap_or_else(|| panic!("no error message in {}", reply.text()))
        .to_string();
    assert!(error.contains(needle), "{error:?} lacks {needle:?}");
}

/// A database file in WAL mode, with the API in front of it.
fn file_rig(label: &str) -> (TempDb, Rig) {
    let tmp = TempDb::new(label);
    let storage = Storage::open(&tmp.path()).unwrap();
    let rig = Setup::new().storage(storage).build();
    (tmp, rig)
}

/// Wait, with a real-time limit, until `condition` holds.  Yields to the
/// runtime and gives other threads (the blocking pool) a moment between looks,
/// so it works on a paused clock too.
async fn until(what: &str, mut condition: impl FnMut() -> bool) {
    let started = Instant::now();
    while !condition() {
        assert!(started.elapsed() < GUARD, "timed out waiting for {what}");
        tokio::task::yield_now().await;
        std::thread::sleep(Duration::from_micros(200));
    }
}

/// Wait for a task that is expected to finish, without depending on the
/// runtime's clock.
async fn finished<T>(what: &str, handle: JoinHandle<T>) -> T {
    until(what, || handle.is_finished()).await;
    handle.await.unwrap()
}

/// Items in the writer queue: the `max_capacity() - capacity()` of its
/// sending end.  The item the writer is working on is not in the queue.
fn queued(rig: &Rig) -> usize {
    rig.record_tx.max_capacity() - rig.record_tx.capacity()
}

/// Spawn a request, to be awaited later.
fn spawn_get(app: &Router, uri: &str) -> JoinHandle<Reply> {
    let app = app.clone();
    let uri = uri.to_string();
    tokio::spawn(async move { send(&app, "GET", &uri, &[], None).await })
}

/// Send a request and keep the response unconsumed: its body, and the permit
/// the body holds, stay alive until it is dropped.
async fn raw_get(app: &Router, uri: &str) -> axum::response::Response {
    app.clone()
        .oneshot(
            Request::builder()
                .uri(uri)
                .header("host", DEFAULT_HOST)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}

// ---------------------------------------------------------------------------
// Calls that existed before: the same answers
// ---------------------------------------------------------------------------

#[tokio::test]
async fn no_parameters_returns_the_newest_500_oldest_first() {
    let rig = Setup::new().build();
    rig.storage.insert_many(series(600)).unwrap();

    let reply = get(&rig.app(), "/api/messages").await;

    let got = ids(&reply);
    assert_eq!(got.len(), 500);
    assert_eq!(got.first().unwrap(), "r0100");
    assert_eq!(got.last().unwrap(), "r0599");
    assert!(got.windows(2).all(|w| w[0] < w[1]), "oldest first");
}

#[tokio::test]
async fn since_is_exclusive_and_returns_the_oldest_records_after_it() {
    let rig = Setup::new().build();
    rig.storage.insert_many(series(30)).unwrap();

    let reply = get(
        &rig.app(),
        &format!("/api/messages?since={}&limit=3", stamp(10)),
    )
    .await;

    assert_eq!(ids(&reply), names(&["r0011", "r0012", "r0013"]));
}

#[tokio::test]
async fn an_unparsable_since_is_ignored_and_the_newest_rows_come_back() {
    let rig = Setup::new().build();
    rig.storage.insert_many(series(600)).unwrap();

    let reply = get(&rig.app(), "/api/messages?since=invalid&limit=10").await;

    let got = ids(&reply);
    assert_eq!(got.first().unwrap(), "r0590");
    assert_eq!(got.last().unwrap(), "r0599");
    assert_eq!(got.len(), 10);
}

#[tokio::test]
async fn a_limit_that_is_not_a_number_keeps_axums_own_rejection() {
    let rig = Setup::new().build();

    let reply = get(&rig.app(), "/api/messages?limit=abc").await;

    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert!(
        reply
            .text()
            .starts_with("Failed to deserialize query string"),
        "{}",
        reply.text()
    );
    assert!(
        !reply
            .header("content-type")
            .is_some_and(|t| t.contains("json")),
        "not a JSON error"
    );
}

#[tokio::test]
async fn limit_is_clamped_to_between_1_and_5000() {
    let rig = Setup::new().build();
    rig.storage.insert_many(series(5001)).unwrap();
    let app = rig.app();

    assert_eq!(ids(&get(&app, "/api/messages?limit=0").await).len(), 1);
    assert_eq!(ids(&get(&app, "/api/messages?limit=-7").await).len(), 1);
    assert_eq!(
        ids(&get(&app, "/api/messages?limit=1").await),
        names(&["r5000"])
    );
    let big = ids(&get(&app, "/api/messages?limit=9999").await);
    assert_eq!(big.len(), 5000);
    assert_eq!(big.first().unwrap(), "r0001", "the newest 5,000");
    assert_eq!(big.last().unwrap(), "r5000");
}

#[tokio::test]
async fn without_the_new_parameters_no_new_header_is_sent() {
    let rig = Setup::new().build();
    rig.storage.insert_many(series(5)).unwrap();

    let reply = get(&rig.app(), "/api/messages?limit=3").await;

    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.header("x-total-count").is_none());
    assert!(reply.header("x-malformed-count").is_none());
    assert!(reply.header("x-history-settled").is_none());
    assert!(reply
        .header("content-type")
        .is_some_and(|t| t.starts_with("application/json")));
}

// ---------------------------------------------------------------------------
// The new parameters
// ---------------------------------------------------------------------------

#[tokio::test]
async fn from_is_inclusive() {
    let rig = Setup::new().build();
    rig.storage.insert_many(series(20)).unwrap();

    let reply = get(
        &rig.app(),
        &format!("/api/messages?from={}&limit=3", stamp(10)),
    )
    .await;

    // The record stamped exactly on the bound is in, and `from` alone asks for
    // the oldest after it, as `since` does.
    assert_eq!(ids(&reply), names(&["r0010", "r0011", "r0012"]));
}

#[tokio::test]
async fn newest_keeps_the_newest_records_after_a_bound() {
    let rig = Setup::new().build();
    rig.storage.insert_many(series(20)).unwrap();
    let app = rig.app();

    let from = get(
        &app,
        &format!("/api/messages?from={}&newest=true&limit=3", stamp(10)),
    )
    .await;
    let since = get(
        &app,
        &format!("/api/messages?since={}&newest=true&limit=3", stamp(10)),
    )
    .await;
    let oldest = get(&app, "/api/messages?newest=false&limit=3").await;

    // Oldest first within the page whichever end is kept.
    assert_eq!(ids(&from), names(&["r0017", "r0018", "r0019"]));
    assert_eq!(ids(&since), names(&["r0017", "r0018", "r0019"]));
    assert_eq!(ids(&oldest), names(&["r0000", "r0001", "r0002"]));
    // Where the bound cuts the range, the count is of what is after it.
    let counted = get(
        &app,
        &format!(
            "/api/messages?from={}&newest=true&count=true&limit=3",
            stamp(10)
        ),
    )
    .await;
    assert_eq!(counted.header("x-total-count"), Some("10"));
    let counted = get(
        &app,
        &format!(
            "/api/messages?since={}&newest=true&count=true&limit=3",
            stamp(10)
        ),
    )
    .await;
    assert_eq!(counted.header("x-total-count"), Some("9"));
}

#[tokio::test]
async fn before_is_exclusive_and_alone_returns_the_newest_records_before_it() {
    let rig = Setup::new().build();
    rig.storage.insert_many(series(20)).unwrap();
    let app = rig.app();

    let reply = get(
        &app,
        &format!("/api/messages?before={}&limit=3&count=true", stamp(10)),
    )
    .await;

    // The record stamped exactly on the bound is out, and with no lower bound
    // the newest records before it are the ones kept.
    assert_eq!(ids(&reply), names(&["r0007", "r0008", "r0009"]));
    assert_eq!(reply.header("x-total-count"), Some("10"));
    let reply = get(&app, &format!("/api/messages?before={}", stamp(0))).await;
    assert!(ids(&reply).is_empty());
}

#[tokio::test]
async fn before_closes_the_window_that_from_or_since_opens() {
    let rig = Setup::new().build();
    rig.storage.insert_many(series(20)).unwrap();
    let app = rig.app();

    let from = get(
        &app,
        &format!("/api/messages?from={}&before={}", stamp(5), stamp(9)),
    )
    .await;
    let since = get(
        &app,
        &format!("/api/messages?since={}&before={}", stamp(5), stamp(9)),
    )
    .await;
    assert_eq!(
        ids(&from),
        names(&["r0005", "r0006", "r0007", "r0008"]),
        "from is in, before is out"
    );
    assert_eq!(ids(&since), names(&["r0006", "r0007", "r0008"]));

    // A window of ten rows with room for three: the lower bound alone would
    // keep the oldest, `newest` keeps the newest of the window, not of the
    // database.
    let window = format!("from={}&before={}&limit=3&count=true", stamp(5), stamp(15));
    let oldest = get(&app, &format!("/api/messages?{window}")).await;
    let newest = get(&app, &format!("/api/messages?{window}&newest=true")).await;
    assert_eq!(ids(&oldest), names(&["r0005", "r0006", "r0007"]));
    assert_eq!(ids(&newest), names(&["r0012", "r0013", "r0014"]));
    assert_eq!(oldest.header("x-total-count"), Some("10"));
    assert_eq!(newest.header("x-total-count"), Some("10"));
}

#[tokio::test]
async fn two_windows_that_meet_share_no_record() {
    let rig = Setup::new().build();
    rig.storage.insert_many(series(30)).unwrap();
    let app = rig.app();

    let first = ids(&get(
        &app,
        &format!("/api/messages?from={}&before={}", stamp(0), stamp(12)),
    )
    .await);
    let second = ids(&get(
        &app,
        &format!("/api/messages?from={}&before={}", stamp(12), stamp(30)),
    )
    .await);

    assert_eq!(first.len() + second.len(), 30);
    assert_eq!(first.last().map(String::as_str), Some("r0011"));
    assert_eq!(second.first().map(String::as_str), Some("r0012"));
}

#[tokio::test]
async fn the_headers_of_a_window_count_only_the_window() {
    // The same on both read paths: the history connection of a file, and the
    // one connection of an in-memory database.
    for on_file in [false, true] {
        let tmp = TempDb::new("api-window-count");
        let storage = if on_file {
            Storage::open(&tmp.path()).unwrap()
        } else {
            Storage::open_in_memory().unwrap()
        };
        let records = assorted(48);
        storage.insert_many(records.clone()).unwrap();
        let rig = Setup::new().storage(storage).build();
        let app = rig.app();

        let in_window = |r: &MessageRecord| r.timestamp >= at(8) && r.timestamp < at(40);
        type Keep = Box<dyn Fn(&MessageRecord) -> bool>;
        let cases: Vec<(&str, Keep)> = vec![
            ("", Box::new(|_| true)),
            ("&status=blocked", Box::new(|r| r.status == Status::Blocked)),
            (
                "&gateway=AA%3ABB%3ACC%3ADD%3A00%3A11%3A22%3A33",
                Box::new(|r| r.gateway_eui.as_deref() == Some("AA:BB:CC:DD:00:11:22:33")),
            ),
            (
                "&device=0a1b2c",
                Box::new(|r| r.dev_addr.as_deref() == Some("0A1B2C3D")),
            ),
            (
                "&mtype=ConfirmedDataUp",
                Box::new(|r| r.mtype == MType::ConfirmedDataUp && r.malformed.is_none()),
            ),
            (
                "&device=0a1b2c&status=forwarded&mtype=UnconfirmedDataUp",
                Box::new(|r| {
                    r.dev_addr.as_deref() == Some("0A1B2C3D")
                        && r.status == Status::Forwarded
                        && r.mtype == MType::UnconfirmedDataUp
                }),
            ),
        ];
        let window = format!("from={}&before={}", stamp(8), stamp(40));
        for (extra, keep) in &cases {
            let label = format!("{extra:?}, on_file={on_file}");
            let matching: Vec<&MessageRecord> =
                records.iter().filter(|r| in_window(r) && keep(r)).collect();
            let malformed = matching.iter().filter(|r| r.malformed.is_some()).count();
            let wider = records.iter().filter(|r| keep(r)).count();
            assert!(
                matching.len() < wider,
                "the window cuts the fixture: {label}"
            );

            for hide in [false, true] {
                let mode = if hide { "exclude" } else { "include" };
                let reply = get(
                    &app,
                    &format!(
                        "/api/messages?{window}&count=true&malformed={mode}&newest=true&limit=5000{extra}"
                    ),
                )
                .await;
                let shown: Vec<String> = matching
                    .iter()
                    .filter(|r| !hide || r.malformed.is_none())
                    .map(|r| r.id.clone())
                    .collect();
                assert_eq!(ids(&reply), shown, "{label}, malformed={mode}");
                assert_eq!(
                    reply.header("x-total-count"),
                    Some(shown.len().to_string().as_str()),
                    "{label}, malformed={mode}"
                );
                assert_eq!(
                    reply.header("x-malformed-count"),
                    Some(malformed.to_string().as_str()),
                    "{label}, malformed={mode}"
                );
            }
        }
    }
}

#[tokio::test]
async fn count_true_gives_the_number_that_match_not_the_number_returned() {
    let rig = Setup::new().build();
    rig.storage.insert_many(series(40)).unwrap();

    let reply = get(&rig.app(), "/api/messages?count=true&limit=5").await;

    assert_eq!(ids(&reply).len(), 5);
    assert_eq!(reply.header("x-total-count"), Some("40"));
    assert_eq!(reply.header("x-malformed-count"), Some("0"));
    let reply = get(&rig.app(), "/api/messages?count=false&limit=5").await;
    assert!(reply.header("x-total-count").is_none());
}

/// Records for the filter tests: every fourth is malformed, statuses and
/// gateways and devices alternate.
fn assorted(count: i64) -> Vec<MessageRecord> {
    (0..count)
        .map(|n| {
            let id = format!("a{n:03}");
            let base = if n % 4 == 3 {
                malformed(
                    &id,
                    n,
                    [Malformed::Crc, Malformed::Mhdr, Malformed::Length][(n as usize / 4) % 3],
                )
            } else {
                edit(rec(&id, n), |r| {
                    r.dev_addr = Some(if n % 2 == 0 { "0A1B2C3D" } else { "26011BDA" }.to_string());
                    r.mtype = if n % 5 == 0 {
                        MType::ConfirmedDataUp
                    } else {
                        MType::UnconfirmedDataUp
                    };
                })
            };
            edit(base, |r| {
                r.status = if n % 3 == 0 {
                    Status::Blocked
                } else {
                    Status::Forwarded
                };
                r.gateway_eui = Some(
                    if n % 2 == 0 {
                        "AA:BB:CC:DD:00:11:22:33"
                    } else {
                        "AA:BB:CC:DD:00:11:22:34"
                    }
                    .to_string(),
                );
            })
        })
        .collect()
}

/// The malformed records that satisfy `keep` (the other conditions).
fn malformed_among(records: &[MessageRecord], keep: impl Fn(&MessageRecord) -> bool) -> usize {
    records
        .iter()
        .filter(|r| r.malformed.is_some() && keep(r))
        .count()
}

#[tokio::test]
async fn the_malformed_count_is_the_same_whether_malformed_records_are_hidden_or_shown() {
    // The same on both read paths: the history connection of a file, and the
    // one connection of an in-memory database.
    for on_file in [false, true] {
        let tmp = TempDb::new("api-malformed-count");
        let storage = if on_file {
            Storage::open(&tmp.path()).unwrap()
        } else {
            Storage::open_in_memory().unwrap()
        };
        let records = assorted(48);
        storage.insert_many(records.clone()).unwrap();
        let rig = Setup::new().storage(storage).build();
        let app = rig.app();

        let status_blocked = |r: &MessageRecord| r.status == Status::Blocked;
        type Keep = Box<dyn Fn(&MessageRecord) -> bool>;
        let cases: Vec<(String, Keep)> = vec![
            (String::new(), Box::new(|_| true)),
            ("&status=blocked".to_string(), Box::new(status_blocked)),
            (
                "&gateway=AA%3ABB%3ACC%3ADD%3A00%3A11%3A22%3A33".to_string(),
                Box::new(|r| r.gateway_eui.as_deref() == Some("AA:BB:CC:DD:00:11:22:33")),
            ),
            (
                format!("&from={}", stamp(20)),
                Box::new(|r| r.timestamp >= at(20)),
            ),
        ];
        for (extra, keep) in &cases {
            let want = malformed_among(&records, keep);
            assert!(want > 0 || !extra.is_empty(), "the fixture has some");
            let shown = get(
                &app,
                &format!("/api/messages?count=true&malformed=include&newest=true{extra}"),
            )
            .await;
            let hidden = get(
                &app,
                &format!("/api/messages?count=true&malformed=exclude&newest=true{extra}"),
            )
            .await;

            let label = format!("{extra:?}, on_file={on_file}");
            assert_eq!(
                shown.header("x-malformed-count"),
                Some(want.to_string().as_str()),
                "{label}"
            );
            assert_eq!(
                hidden.header("x-malformed-count"),
                shown.header("x-malformed-count"),
                "{label}"
            );
            let matching = records.iter().filter(|r| keep(r)).count();
            assert_eq!(
                shown.header("x-total-count"),
                Some(matching.to_string().as_str()),
                "{label}"
            );
            assert_eq!(
                hidden.header("x-total-count"),
                Some((matching - want).to_string().as_str()),
                "{label}"
            );
            assert!(ids(&hidden).iter().all(|id| {
                let r = records.iter().find(|r| &r.id == id).unwrap();
                r.malformed.is_none()
            }));
        }
    }
}

#[tokio::test]
async fn mtype_takes_a_comma_separated_list() {
    let rig = Setup::new().build();
    rig.storage
        .insert_many(vec![
            edit(rec("up", 1), |r| r.mtype = MType::UnconfirmedDataUp),
            edit(rec("confirmed", 2), |r| r.mtype = MType::ConfirmedDataUp),
            edit(rec("join", 3), |r| r.mtype = MType::JoinRequest),
            edit(rec("down", 4), |r| r.mtype = MType::UnconfirmedDataDown),
        ])
        .unwrap();
    let app = rig.app();

    let one = get(&app, "/api/messages?mtype=JoinRequest").await;
    let two = get(
        &app,
        "/api/messages?mtype=ConfirmedDataUp,UnconfirmedDataDown",
    )
    .await;

    assert_eq!(ids(&one), names(&["join"]));
    assert_eq!(ids(&two), names(&["confirmed", "down"]));
}

#[tokio::test]
async fn status_is_accepted_in_any_case_and_all_means_any() {
    let rig = Setup::new().build();
    rig.storage
        .insert_many(vec![
            edit(rec("fwd", 1), |r| r.status = Status::Forwarded),
            edit(rec("blk", 2), |r| r.status = Status::Blocked),
            edit(rec("cap", 3), |r| r.status = Status::Captured),
        ])
        .unwrap();
    let app = rig.app();

    for spelling in ["blocked", "Blocked", "BLOCKED"] {
        let reply = get(&app, &format!("/api/messages?status={spelling}")).await;
        assert_eq!(ids(&reply), names(&["blk"]), "{spelling}");
    }
    assert_eq!(
        ids(&get(&app, "/api/messages?status=Forwarded").await),
        names(&["fwd"])
    );
    assert_eq!(
        ids(&get(&app, "/api/messages?status=captured").await),
        names(&["cap"])
    );
    for any in ["all", "ALL"] {
        let reply = get(&app, &format!("/api/messages?status={any}")).await;
        assert_eq!(ids(&reply), names(&["fwd", "blk", "cap"]), "{any}");
    }
}

#[tokio::test]
async fn empty_values_mean_any() {
    let rig = Setup::new().build();
    rig.storage.insert_many(assorted(12)).unwrap();
    let app = rig.app();

    let plain = ids(&get(&app, "/api/messages?newest=true").await);
    let empty = ids(&get(
        &app,
        "/api/messages?newest=true&device=&mtype=&status=&gateway=",
    )
    .await);

    assert_eq!(plain.len(), 12);
    assert_eq!(empty, plain);
}

#[tokio::test]
async fn gateway_is_compared_exactly() {
    let rig = Setup::new().build();
    rig.storage.insert_many(assorted(12)).unwrap();
    let app = rig.app();

    let exact = get(
        &app,
        "/api/messages?gateway=AA%3ABB%3ACC%3ADD%3A00%3A11%3A22%3A34",
    )
    .await;
    let lower = get(
        &app,
        "/api/messages?gateway=aa%3Abb%3Acc%3Add%3A00%3A11%3A22%3A34",
    )
    .await;
    let part = get(&app, "/api/messages?gateway=AA%3ABB").await;

    assert_eq!(ids(&exact).len(), 6);
    assert!(ids(&lower).is_empty());
    assert!(ids(&part).is_empty());
}

// ---------------------------------------------------------------------------
// Refused parameters
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_bad_from_is_a_400_with_a_json_error() {
    let rig = Setup::new().build();
    let app = rig.app();

    for bad in ["yesterday", "2026-13-40T00%3A00%3A00Z", "", "1730000000"] {
        let reply = get(&app, &format!("/api/messages?from={bad}")).await;
        assert_error(&reply, StatusCode::BAD_REQUEST, "from");
    }
}

#[tokio::test]
async fn from_together_with_since_is_a_400() {
    let rig = Setup::new().build();

    let reply = get(
        &rig.app(),
        &format!("/api/messages?from={}&since={}", stamp(1), stamp(2)),
    )
    .await;

    assert_error(&reply, StatusCode::BAD_REQUEST, "from or since");
}

/// An error answer whose message is exactly `message`.
fn assert_error_message(reply: &Reply, message: &str) {
    assert_error(reply, StatusCode::BAD_REQUEST, message);
    assert_eq!(reply.json()["error"].as_str(), Some(message));
}

#[tokio::test]
async fn a_bad_before_is_a_400_with_a_json_error() {
    let rig = Setup::new().build();
    let app = rig.app();

    for bad in [
        "yesterday",
        "2026-13-40T00%3A00%3A00Z",
        "",
        "1730000000",
        "2026-10-30",
    ] {
        let reply = get(&app, &format!("/api/messages?before={bad}")).await;
        assert_error_message(
            &reply,
            "before must be an RFC 3339 timestamp, such as 2026-10-30T12:00:00.000Z",
        );
    }
}

#[tokio::test]
async fn a_before_not_later_than_from_or_since_is_a_400() {
    let rig = Setup::new().build();
    rig.storage.insert_many(series(20)).unwrap();
    let app = rig.app();

    // The same instant is an empty window, and an earlier one an inverted one.
    for before in [stamp(10), stamp(3)] {
        let reply = get(
            &app,
            &format!("/api/messages?from={}&before={before}", stamp(10)),
        )
        .await;
        assert_error_message(&reply, "before must be later than from");
        let reply = get(
            &app,
            &format!("/api/messages?since={}&before={before}", stamp(10)),
        )
        .await;
        assert_error_message(&reply, "before must be later than since");
    }

    // One second later is a window; an unparsable `since` is ignored, so it
    // leaves `before` nothing to be later than.
    let reply = get(
        &app,
        &format!("/api/messages?from={}&before={}", stamp(10), stamp(11)),
    )
    .await;
    assert_eq!(ids(&reply), names(&["r0010"]));
    let reply = get(
        &app,
        &format!("/api/messages?since=garbage&before={}&limit=2", stamp(3)),
    )
    .await;
    assert_eq!(ids(&reply), names(&["r0001", "r0002"]));
}

#[tokio::test]
async fn an_unknown_mtype_status_or_malformed_is_a_400() {
    let rig = Setup::new().build();
    let app = rig.app();

    assert_error(
        &get(&app, "/api/messages?mtype=JoinRequest,Nonsense").await,
        StatusCode::BAD_REQUEST,
        "mtype",
    );
    assert_error(
        &get(&app, "/api/messages?mtype=joinrequest").await,
        StatusCode::BAD_REQUEST,
        "mtype",
    );
    assert_error(
        &get(&app, "/api/messages?status=lost").await,
        StatusCode::BAD_REQUEST,
        "status",
    );
    assert_error(
        &get(&app, "/api/messages?malformed=only").await,
        StatusCode::BAD_REQUEST,
        "malformed",
    );
    assert_error(
        &get(&app, "/api/messages?malformed=").await,
        StatusCode::BAD_REQUEST,
        "malformed",
    );
}

#[tokio::test]
async fn newest_count_and_settle_take_only_true_or_false() {
    let rig = Setup::new().build();
    let app = rig.app();

    for name in ["newest", "count", "settle"] {
        for bad in ["yes", "1", "True", ""] {
            let reply = get(&app, &format!("/api/messages?{name}={bad}")).await;
            assert_error(&reply, StatusCode::BAD_REQUEST, name);
        }
    }
}

// ---------------------------------------------------------------------------
// A database that is not in WAL mode, and one that is busy
// ---------------------------------------------------------------------------

fn no_wal_rig(label: &str) -> (TempDb, Rig) {
    let tmp = TempDb::new(label);
    let storage = Storage::open_without_wal(&tmp.path()).unwrap();
    let rig = Setup::new().storage(storage).build();
    (tmp, rig)
}

#[tokio::test]
async fn without_wal_a_filter_is_refused_with_503_and_a_time_only_request_works() {
    let (_tmp, rig) = no_wal_rig("api-no-wal");
    rig.storage.insert_many(assorted(12)).unwrap();
    let app = rig.app();

    // A device filter, and hiding malformed records alone, each need a scan.
    for filter in ["device=0a1b", "malformed=exclude"] {
        let reply = get(&app, &format!("/api/messages?{filter}&count=true")).await;
        assert_error(&reply, StatusCode::SERVICE_UNAVAILABLE, "WAL");
        assert_eq!(reply.json()["code"], "no_wal", "{filter}");
        assert_eq!(
            reply.json().as_object().unwrap().len(),
            2,
            "exactly an error and a code"
        );
    }

    // A time bound alone is an index range.  Counting the malformed records
    // would be a scan, so that header is left out.
    let reply = get(&app, "/api/messages?count=true&newest=true").await;
    assert_eq!(ids(&reply).len(), 12);
    assert_eq!(reply.header("x-total-count"), Some("12"));
    assert!(reply.header("x-malformed-count").is_none());
    let reply = get(
        &app,
        &format!(
            "/api/messages?from={}&count=true&malformed=include",
            stamp(6)
        ),
    )
    .await;
    assert_eq!(reply.header("x-total-count"), Some("6"));
    assert!(reply.header("x-malformed-count").is_none());
}

#[tokio::test]
async fn without_wal_a_capped_range_full_of_malformed_records_returns_the_newest() {
    let (_tmp, rig) = no_wal_rig("api-no-wal-heavy");
    // 10,000 records: of the newest 5,000, 4,000 are malformed; of the older
    // 5,000, 1,000 are.
    let records = (0..10_000).map(|n| {
        let id = format!("h{n:05}");
        let bad = if n >= 5_000 { n % 5 != 0 } else { n % 5 == 1 };
        if bad {
            malformed(&id, n, Malformed::Crc)
        } else {
            rec(&id, n)
        }
    });
    rig.storage.insert_many(records).unwrap();

    let reply = get(
        &rig.app(),
        "/api/messages?malformed=include&limit=5000&count=true",
    )
    .await;

    let rows = reply.json();
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 5_000);
    assert_eq!(rows.first().unwrap()["id"], "h05000");
    assert_eq!(rows.last().unwrap()["id"], "h09999");
    assert_eq!(
        rows.iter().filter(|r| !r["malformed"].is_null()).count(),
        4_000
    );
    assert_eq!(reply.header("x-total-count"), Some("10000"));
    assert!(reply.header("x-malformed-count").is_none());
}

#[tokio::test]
async fn a_locked_database_gets_503_busy_with_a_message_to_try_again() {
    let tmp = TempDb::new("api-busy");
    drop(Storage::open(&tmp.path()).unwrap());
    let (storage, holder) = Storage::locked_by_another_connection(&tmp.path());
    let rig = Setup::new().storage(storage).build();

    let started = Instant::now();
    let reply = get(&rig.app(), "/api/messages?count=true").await;

    assert_error(&reply, StatusCode::SERVICE_UNAVAILABLE, "try again");
    assert_eq!(reply.json()["code"], "busy");
    assert!(
        started.elapsed() > Duration::from_millis(800),
        "the history connection waits about a second before giving up"
    );
    // The turn for queries came back: with the lock gone, the next one works.
    holder.execute_batch("COMMIT").unwrap();
    drop(holder);
    assert_eq!(rig.state.history.available_permits(), 1);
    let again = get(&rig.app(), "/api/messages?count=true").await;
    assert_eq!(again.status, StatusCode::OK);
}

// ---------------------------------------------------------------------------
// Headers, CORS, the Host check
// ---------------------------------------------------------------------------

const EXPOSED: [&str; 4] = [
    "x-total-count",
    "x-malformed-count",
    "x-history-settled",
    "x-storage-write-failures",
];

#[tokio::test]
async fn a_listed_origin_may_read_the_four_headers_and_another_gets_no_cors_headers() {
    let rig = Setup::new().cors(&["https://ui.example"]).build();
    rig.storage.insert_many(series(3)).unwrap();
    let app = rig.app();

    let listed = send(
        &app,
        "GET",
        "/api/messages?count=true&settle=true",
        &[("origin", "https://ui.example")],
        None,
    )
    .await;
    let other = send(
        &app,
        "GET",
        "/api/messages?count=true&settle=true",
        &[("origin", "http://evil.example")],
        None,
    )
    .await;

    assert_eq!(listed.status, StatusCode::OK);
    let exposed: Vec<String> = listed
        .header("access-control-expose-headers")
        .expect("the listed origin gets the list of readable headers")
        .split(',')
        .map(|h| h.trim().to_ascii_lowercase())
        .collect();
    for name in EXPOSED {
        assert!(exposed.iter().any(|h| h == name), "{name} in {exposed:?}");
        assert!(listed.header(name).is_some(), "the reply carries {name}");
    }
    assert_eq!(other.status, StatusCode::OK, "the request itself is served");
    // What lets a page on another origin read the answer is the allow-origin
    // header, and an unlisted origin does not get one.  The layer names the
    // readable headers on every answer; without allow-origin a browser
    // ignores that list.
    assert!(
        other.header("access-control-allow-origin").is_none(),
        "{:?}",
        other.headers
    );
    assert!(other.header("access-control-allow-credentials").is_none());
}

#[tokio::test]
async fn a_foreign_host_still_gets_the_plain_text_403_without_the_failure_count() {
    let rig = Setup::new().build();
    rig.write_failures.store(3, Ordering::Relaxed);

    let reply = send(
        &rig.app(),
        "GET",
        &format!(
            "/api/messages?from={}&device=ab&count=true&newest=true&settle=true",
            stamp(0)
        ),
        &[("host", "evil.example")],
        None,
    )
    .await;

    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert!(reply
        .header("content-type")
        .is_some_and(|t| t.starts_with("text/plain")));
    assert!(reply.header("x-storage-write-failures").is_none());
    assert!(reply.header("x-total-count").is_none());
}

// ---------------------------------------------------------------------------
// Bounded work
// ---------------------------------------------------------------------------

#[tokio::test]
async fn four_concurrent_requests_all_complete() {
    let rig = Setup::new().build();
    rig.storage.insert_many(series(100)).unwrap();
    let app = rig.app();

    let requests: Vec<_> = (0..4)
        .map(|n| spawn_get(&app, &format!("/api/messages?count=true&limit={}", 10 + n)))
        .collect();

    for (n, request) in requests.into_iter().enumerate() {
        let reply = timeout(GUARD, request).await.unwrap().unwrap();
        assert_eq!(ids(&reply).len(), 10 + n);
        assert_eq!(reply.header("x-total-count"), Some("100"));
    }
    assert_eq!(rig.state.history.available_permits(), 1);
    assert_eq!(rig.state.history_bodies.available_permits(), 3);
}

#[test]
fn a_cancel_flag_is_set_when_its_owner_is_dropped() {
    let guard = messages::CancelOnDrop::new();
    let flag = guard.flag();
    assert!(!flag.load(Ordering::Relaxed));

    drop(guard);

    assert!(flag.load(Ordering::Relaxed));
}

#[tokio::test]
async fn a_request_dropped_while_waiting_for_its_turn_never_runs_its_query() {
    let (_tmp, rig) = file_rig("api-drop-waiting");
    rig.storage.insert_many(series(20)).unwrap();
    let probe = rig.storage.probe();
    let app = rig.app();
    // Another query is running, as far as the turn is concerned.
    let turn = rig.state.history.clone().acquire_owned().await.unwrap();

    let waiting = spawn_get(&app, "/api/messages?count=true");
    until(
        "the request to take its place and wait for the turn",
        || rig.state.history_bodies.available_permits() == 2,
    )
    .await;
    waiting.abort();
    assert!(waiting.await.unwrap_err().is_cancelled());
    drop(turn);

    // It left nothing behind: no query, the turn free, the place in memory free.
    assert_eq!(rig.state.history.available_permits(), 1);
    assert_eq!(rig.state.history_bodies.available_permits(), 3);
    assert_eq!(probe.started(), 0);
    let next = timeout(
        Duration::from_secs(2),
        get(&app, "/api/messages?count=true"),
    )
    .await
    .expect("the next request is answered at once");
    assert_eq!(next.header("x-total-count"), Some("20"));
    assert_eq!(probe.started(), 1);
}

#[tokio::test]
async fn a_request_aborted_while_its_query_runs_stops_it_before_the_next_one_starts() {
    let (_tmp, rig) = file_rig("api-abort-running");
    // Enough rows for a scan that matches nothing to take a while.
    rig.storage.insert_generated(300_000);
    let probe = rig.storage.probe();
    let app = rig.app();

    let slow = spawn_get(&app, "/api/messages?device=zzzz&count=true&newest=true");
    until("the query to start", || probe.running() == 1).await;
    slow.abort();
    let _ = slow.await;
    // The next request has to wait for the turn, which the stopping query holds.
    let next = spawn_get(&app, "/api/messages?count=true&limit=1");
    let reply = finished("the next request", next).await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.header("x-total-count"), Some("300000"));
    assert_eq!(probe.cancelled(), 1, "the abandoned scan was cancelled");
    assert_eq!(probe.started(), 2);
    assert_eq!(
        probe.most_running(),
        1,
        "the second query did not start until the first had returned"
    );
    assert_eq!(rig.state.history.available_permits(), 1);
}

#[tokio::test]
async fn a_fourth_response_that_nobody_reads_waits_for_one_of_the_others_to_be_dropped() {
    let rig = Setup::new().build();
    rig.storage.insert_many(series(20)).unwrap();
    let app = rig.app();

    let mut held = Vec::new();
    for _ in 0..3 {
        held.push(raw_get(&app, "/api/messages?limit=5").await);
    }
    assert_eq!(rig.state.history_bodies.available_permits(), 0);

    let fourth = {
        let app = app.clone();
        tokio::spawn(async move { raw_get(&app, "/api/messages?limit=5").await })
    };
    assert!(
        timeout(Duration::from_millis(300), async {
            until("never", || fourth.is_finished()).await
        })
        .await
        .is_err(),
        "the fourth is answered while three bodies are held"
    );

    drop(held.pop());
    let response = timeout(GUARD, fourth).await.unwrap().unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    drop(response);
    drop(held);
    assert_eq!(rig.state.history_bodies.available_permits(), 3);
}

#[tokio::test]
async fn a_file_database_serves_filtered_counts_and_sees_a_record_written_in_between() {
    let (_tmp, rig) = file_rig("api-file");
    rig.storage.insert_many(assorted(40)).unwrap();
    let app = rig.app();
    let uri = "/api/messages?device=0a1b&count=true&newest=true&limit=5&malformed=exclude";

    let first = get(&app, uri).await;

    // Even n are 0A1B2C3D; n % 4 == 3 are malformed and have no device, so all
    // 20 even ones match and none is malformed.
    assert_eq!(
        ids(&first),
        names(&["a030", "a032", "a034", "a036", "a038"])
    );
    assert_eq!(first.header("x-total-count"), Some("20"));
    // A malformed record has no identifiers, so no device text matches it.
    assert_eq!(first.header("x-malformed-count"), Some("0"));

    // The writer saves another record; the history connection sees it.
    rig.storage
        .insert(&edit(rec("late", 100), |r| {
            r.dev_addr = Some("0A1B0000".to_string())
        }))
        .unwrap();
    let second = get(&app, uri).await;

    assert_eq!(ids(&second).last().unwrap(), "late");
    assert_eq!(second.header("x-total-count"), Some("21"));
}

// ---------------------------------------------------------------------------
// The shared test vectors
// ---------------------------------------------------------------------------

/// The records, labels and cases that the WebUI's own tests also run: the
/// browser filter for live records and this server filter for loaded ones must
/// agree on every record.
const VECTORS: &str = include_str!("../../../webui/test/history-vectors.json");

/// Store the vectors' labels and records.  A record is built from the usual
/// test record with the fields the vectors name.
fn load_vectors(storage: &Storage) -> Value {
    let vectors: Value = serde_json::from_str(VECTORS).expect("the vectors are valid JSON");
    for (device, label) in vectors["labels"].as_object().unwrap() {
        storage
            .set_label(device, label.as_str().unwrap(), true, MAX_LABELS)
            .unwrap();
    }
    let text = |value: &Value| value.as_str().map(str::to_string);
    let records: Vec<MessageRecord> = vectors["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| MessageRecord {
            id: text(&v["id"]).unwrap(),
            timestamp: DateTime::parse_from_rfc3339(v["timestamp"].as_str().unwrap())
                .unwrap()
                .with_timezone(&Utc),
            mtype: serde_json::from_value(v["mtype"].clone()).unwrap(),
            status: serde_json::from_value(v["status"].clone()).unwrap(),
            gateway_eui: text(&v["gateway_eui"]),
            dev_addr: text(&v["dev_addr"]),
            dev_eui: text(&v["dev_eui"]),
            malformed: serde_json::from_value(v["malformed"].clone()).unwrap(),
            ..record_with_new_fields()
        })
        .collect();
    storage.insert_many(records).unwrap();
    vectors
}

/// Send every case of the vectors through the router and compare the ids (in
/// order), the count and the malformed count with what the case expects.
async fn run_vectors(rig: &Rig, vectors: &Value) {
    let app = rig.app();
    let cases = vectors["cases"].as_array().unwrap();
    assert!(cases.len() > 40, "{} cases", cases.len());
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let query = case["query"].as_str().unwrap();
        let joined = if query.is_empty() { "" } else { "&" };
        let uri = format!("/api/messages?{query}{joined}newest=true&count=true&limit=5000");
        let reply = get(&app, &uri).await;

        let want: Vec<String> = case["expect"]
            .as_array()
            .unwrap()
            .iter()
            .map(|id| id.as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            reply.status,
            StatusCode::OK,
            "{name}: {uri}: {}",
            reply.text()
        );
        assert_eq!(ids(&reply), want, "{name}: {uri}");
        assert_eq!(
            reply.header("x-total-count"),
            Some(want.len().to_string().as_str()),
            "{name}: the count"
        );
        assert_eq!(
            reply.header("x-malformed-count"),
            Some(
                case["expectMalformed"]
                    .as_u64()
                    .unwrap()
                    .to_string()
                    .as_str()
            ),
            "{name}: the malformed count"
        );
    }
}

#[tokio::test]
async fn the_shared_vectors_give_the_browsers_answers_on_an_in_memory_database() {
    let rig = Setup::new().build();
    let vectors = load_vectors(&rig.storage);

    run_vectors(&rig, &vectors).await;
}

#[tokio::test]
async fn the_shared_vectors_give_the_browsers_answers_on_a_file_database() {
    let (_tmp, rig) = file_rig("api-vectors");
    let vectors = load_vectors(&rig.storage);

    run_vectors(&rig, &vectors).await;
}

// ---------------------------------------------------------------------------
// Waiting for the storage writer
// ---------------------------------------------------------------------------

/// A reload as the WebUI makes it: the newest records, counted, after the
/// writer has caught up.
const SETTLE: &str = "/api/messages?newest=true&count=true&limit=5000&settle=true";

/// An insert for a test writer: ids starting `bad` fail, ids starting `panic`
/// panic, everything else is saved.
fn flaky_insert(
    storage: Storage,
) -> impl Fn(MessageRecord) -> Result<(), SnifferError> + Clone + Send + Sync + 'static {
    move |record| {
        if record.id.starts_with("bad") {
            Err(SnifferError::Io(std::io::Error::other("disk full")))
        } else if record.id.starts_with("panic") {
            panic!("the insert panics");
        } else {
            storage.insert(&record)
        }
    }
}

/// Run the storage writer of a rig built without one, with `insert` in place
/// of the real insert.
fn run_writer_with(
    rig: &mut Rig,
    insert: impl Fn(MessageRecord) -> Result<(), SnifferError> + Clone + Send + Sync + 'static,
) -> JoinHandle<()> {
    let rx = rig.record_rx.take().expect("the rig has no writer");
    tokio::spawn(crate::cmd::root::process_items(
        rx,
        insert,
        rig.write_failures.clone(),
        rig.writer_cancel.clone(),
    ))
}

async fn queue(rig: &Rig, record: MessageRecord) {
    rig.record_tx
        .send(WriterItem::Record(record))
        .await
        .expect("the writer queue is open");
}

fn lines_with(lines: &[(Level, String)], level: Level, needle: &str) -> usize {
    lines
        .iter()
        .filter(|(l, text)| *l == level && text.contains(needle))
        .count()
}

#[tokio::test]
async fn a_settled_request_waits_for_the_backlog_the_writer_has_yet_to_save() {
    let (_tmp, rig) = file_rig("api-settle-backlog");
    let app = rig.app();
    let guard = rig.storage.hold_writer();
    for n in 0..50 {
        queue(&rig, rec(&format!("q{n:02}"), n)).await;
    }
    // The writer holds one record, stuck on the lock; the rest wait.
    until("the writer to take its first record", || queued(&rig) == 49).await;

    let request = spawn_get(&app, SETTLE);
    until("the marker to be queued", || queued(&rig) == 50).await;
    let waited = timeout(Duration::from_millis(300), async {
        until("never", || request.is_finished()).await
    })
    .await;
    assert!(waited.is_err(), "answered while the writer was stalled");
    let saved = get(&app, "/api/messages?count=true").await;
    assert_eq!(
        saved.header("x-total-count"),
        Some("0"),
        "nothing saved yet"
    );

    drop(guard);
    let reply = finished("the answer", request).await;

    assert_eq!(ids(&reply).len(), 50, "all the backlog");
    assert_eq!(reply.header("x-history-settled"), Some("true"));
    assert_eq!(reply.header("x-total-count"), Some("50"));
    assert_eq!(reply.header("x-storage-write-failures"), Some("0"));
    assert_eq!(rig.state.history_settle.available_permits(), 1);
}

#[tokio::test(start_paused = true)]
async fn after_five_seconds_a_settled_request_answers_with_what_is_saved_and_says_so() {
    let (_tmp, rig) = file_rig("api-settle-timeout");
    rig.storage.insert_many(series(10)).unwrap();
    let app = rig.app();
    let guard = rig.storage.hold_writer();
    for n in 100..106 {
        queue(&rig, rec(&format!("late{n}"), n)).await;
    }
    until("the writer to take its first record", || queued(&rig) == 5).await;
    start_capture();

    let request = spawn_get(&app, SETTLE);
    until("the marker to be queued", || queued(&rig) == 6).await;
    tokio::time::advance(Duration::from_secs(5)).await;
    let reply = finished("the answer", request).await;

    let want: Vec<String> = (0..10).map(|n| format!("r{n:04}")).collect();
    assert_eq!(ids(&reply), want, "the rows saved so far");
    assert_eq!(reply.header("x-history-settled"), Some("false"));
    assert_eq!(reply.header("x-total-count"), Some("10"));
    let lines = take_capture();
    assert_eq!(
        lines_with(
            &lines,
            Level::Warn,
            "Storage writer is behind; history served without waiting for it"
        ),
        1,
        "{lines:?}"
    );
    drop(guard);
    until("the writer to catch up", || {
        queued(&rig) == 0 && rig.state.history_settle.available_permits() == 1
    })
    .await;
}

#[tokio::test]
async fn a_request_without_settle_is_not_delayed_by_a_marker_that_waits() {
    let (_tmp, rig) = file_rig("api-settle-others");
    rig.storage.insert_many(series(5)).unwrap();
    let app = rig.app();
    let guard = rig.storage.hold_writer();
    queue(&rig, rec("stuck", 50)).await;
    until("the writer to be stuck", || queued(&rig) == 0).await;
    let waiting = spawn_get(&app, SETTLE);
    until("the marker to be queued", || queued(&rig) == 1).await;

    let reply = timeout(
        Duration::from_secs(2),
        get(&app, "/api/messages?count=true&limit=2"),
    )
    .await
    .expect("answered while a marker waits");

    assert_eq!(ids(&reply), names(&["r0003", "r0004"]));
    assert!(reply.header("x-history-settled").is_none());
    assert_eq!(
        rig.state.history.available_permits(),
        1,
        "the waiting request has not taken the turn for queries"
    );
    assert!(!waiting.is_finished());
    drop(guard);
    let reply = finished("the waiting request", waiting).await;
    assert_eq!(reply.header("x-history-settled"), Some("true"));
}

#[tokio::test]
async fn without_settle_no_marker_is_queued_and_no_settled_header_is_sent() {
    let mut rig = Setup::new().without_record_writer().build();
    rig.storage.insert_many(series(3)).unwrap();
    let app = rig.app();
    let mut queue = rig.record_rx.take().unwrap();

    let plain = get(&app, "/api/messages?count=true").await;
    let explicit = get(&app, "/api/messages?count=true&settle=false").await;

    for reply in [&plain, &explicit] {
        assert_eq!(ids(reply).len(), 3);
        assert!(reply.header("x-history-settled").is_none());
    }
    assert_eq!(plain.body, explicit.body);
    assert!(queue.try_recv().is_err(), "nothing was queued");
    assert_eq!(rig.state.history_settle.available_permits(), 1);
}

#[tokio::test]
async fn a_marker_is_answered_once_a_failed_insert_has_been_counted_and_logged() {
    let tmp = TempDb::new("api-settle-failed");
    let storage = Storage::open(&tmp.path()).unwrap();
    let mut rig = Setup::new()
        .storage(storage.clone())
        .without_record_writer()
        .build();
    let _writer = run_writer_with(&mut rig, flaky_insert(storage));
    let app = rig.app();
    start_capture();
    queue(&rig, rec("ok-1", 1)).await;
    queue(&rig, rec("bad-1", 2)).await;
    queue(&rig, rec("ok-2", 3)).await;

    let reply = get(&app, SETTLE).await;

    // "Settled" means processed: the failed record is not in the answer, and
    // the count says one went missing.
    assert_eq!(ids(&reply), names(&["ok-1", "ok-2"]));
    assert_eq!(reply.header("x-history-settled"), Some("true"));
    assert_eq!(reply.header("x-storage-write-failures"), Some("1"));
    assert_eq!(reply.header("x-total-count"), Some("2"));
    let lines = take_capture();
    assert_eq!(
        lines_with(
            &lines,
            Level::Error,
            "Storage write error: I/O error: disk full"
        ),
        1,
        "{lines:?}"
    );

    // An insert that panics is counted too.
    queue(&rig, rec("panic-1", 4)).await;
    let reply = get(&app, SETTLE).await;
    assert_eq!(reply.header("x-history-settled"), Some("true"));
    assert_eq!(reply.header("x-storage-write-failures"), Some("2"));
    assert_eq!(ids(&reply), names(&["ok-1", "ok-2"]));
    let lines = take_capture();
    assert_eq!(
        lines_with(&lines, Level::Error, "Storage writer task panicked"),
        1,
        "{lines:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_settled_answer_counts_the_failure_that_was_queued_before_its_marker() {
    let mut rig = Setup::new().without_record_writer().build();
    let storage = rig.storage.clone();
    let _writer = run_writer_with(&mut rig, flaky_insert(storage));
    let app = rig.app();

    // The writer and the handler run on different threads; the marker's
    // answer is what makes the writer's count visible to the handler.
    for round in 0..200u64 {
        queue(&rig, rec(&format!("bad-{round}"), 1)).await;
        let reply = get(&app, SETTLE).await;

        assert_eq!(reply.header("x-history-settled"), Some("true"), "{round}");
        assert_eq!(
            reply.header("x-storage-write-failures"),
            Some((round + 1).to_string().as_str()),
            "round {round}"
        );
    }
}

#[tokio::test]
async fn a_new_sniffer_starts_counting_from_zero() {
    let rig = Setup::new().build();

    let reply = get(&rig.app(), "/api/messages").await;

    assert_eq!(reply.header("x-storage-write-failures"), Some("0"));
}

// ---------------------------------------------------------------------------
// The failure count on every answer of the route
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn every_answer_of_the_route_carries_the_failure_count() {
    let tmp = TempDb::new("api-failure-count");
    let storage = Storage::open(&tmp.path()).unwrap();
    storage.insert_many(series(30)).unwrap();
    let mut rig = Setup::new()
        .storage(storage.clone())
        .without_record_writer()
        .build();
    let _writer = run_writer_with(&mut rig, flaky_insert(storage.clone()));
    let app = rig.app();
    queue(&rig, rec("bad-1", 100)).await;
    let first = get(&app, SETTLE).await;
    assert_eq!(first.header("x-storage-write-failures"), Some("1"));

    // The answers a client can get, and the status each has without the header.
    let since = format!("/api/messages?since={}&limit=3", stamp(10));
    let answers: [(&str, StatusCode); 8] = [
        ("/api/messages", StatusCode::OK),
        ("/api/messages?limit=5", StatusCode::OK),
        (&since, StatusCode::OK),
        ("/api/messages?count=true&newest=true", StatusCode::OK),
        (SETTLE, StatusCode::OK),
        ("/api/messages?from=yesterday", StatusCode::BAD_REQUEST),
        ("/api/messages?mtype=Nope", StatusCode::BAD_REQUEST),
        ("/api/messages?limit=abc", StatusCode::BAD_REQUEST),
    ];
    for (uri, status) in answers {
        let reply = get(&app, uri).await;
        assert_eq!(reply.status, status, "{uri}");
        assert_eq!(reply.header("x-storage-write-failures"), Some("1"), "{uri}");
    }

    // The header is the only addition: axum's own refusal of a query string
    // has the status and body it has without the layer.
    let bare = Router::new()
        .route(
            "/api/messages",
            axum::routing::get(messages::messages_handler),
        )
        .with_state(rig.state.clone());
    let with = get(&app, "/api/messages?limit=abc").await;
    let without = get(&bare, "/api/messages?limit=abc").await;
    assert!(without.header("x-storage-write-failures").is_none());
    assert_eq!(with.status, without.status);
    assert_eq!(with.body, without.body);
    assert_eq!(with.header("content-type"), without.header("content-type"));

    // An answer given without waiting for the writer has it too.
    let guard = storage.hold_writer();
    queue(&rig, rec("stuck", 101)).await;
    until("the writer to be stuck", || queued(&rig) == 0).await;
    let late = spawn_get(&app, SETTLE);
    until("the marker to be queued", || queued(&rig) == 1).await;
    tokio::time::advance(Duration::from_secs(5)).await;
    let reply = finished("the unsettled answer", late).await;
    assert_eq!(reply.header("x-history-settled"), Some("false"));
    assert_eq!(reply.header("x-storage-write-failures"), Some("1"));
    drop(guard);
    until("the writer to catch up", || {
        queued(&rig) == 0 && rig.state.history_settle.available_permits() == 1
    })
    .await;
}

#[tokio::test]
async fn errors_of_the_handler_carry_the_failure_count_with_their_own_status_and_body() {
    // 503: a filter on a database without WAL.
    let (_tmp, rig) = no_wal_rig("api-count-no-wal");
    rig.write_failures.store(1, Ordering::Relaxed);
    let reply = get(&rig.app(), "/api/messages?device=ab").await;
    assert_error(&reply, StatusCode::SERVICE_UNAVAILABLE, "WAL");
    assert_eq!(reply.json()["code"], "no_wal");
    assert_eq!(reply.header("x-storage-write-failures"), Some("1"));

    // 503: a database that stays locked.
    let tmp = TempDb::new("api-count-busy");
    drop(Storage::open(&tmp.path()).unwrap());
    let (storage, _holder) = Storage::locked_by_another_connection(&tmp.path());
    let rig = Setup::new().storage(storage).build();
    rig.write_failures.store(1, Ordering::Relaxed);
    let reply = get(&rig.app(), "/api/messages").await;
    assert_error(&reply, StatusCode::SERVICE_UNAVAILABLE, "try again");
    assert_eq!(reply.json()["code"], "busy");
    assert_eq!(reply.header("x-storage-write-failures"), Some("1"));

    // 500: a database whose lock a panic poisoned.
    let rig = Setup::new().build();
    rig.write_failures.store(1, Ordering::Relaxed);
    let storage = rig.storage.clone();
    let poisoner = std::thread::spawn(move || {
        let _guard = storage.hold_writer();
        panic!("poison the lock");
    });
    assert!(poisoner.join().is_err());
    start_capture();
    let reply = get(&rig.app(), "/api/messages").await;
    assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(reply.body.is_empty());
    assert_eq!(reply.header("x-storage-write-failures"), Some("1"));
    assert_eq!(
        lines_with(
            &take_capture(),
            Level::Error,
            "Messages query error: storage lock poisoned"
        ),
        1
    );
}

// ---------------------------------------------------------------------------
// One marker at a time, and the permit that comes back
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn one_marker_is_in_the_queue_even_after_its_waiters_have_given_up() {
    let (_tmp, rig) = file_rig("api-one-marker");
    let app = rig.app();
    let permits = || rig.state.history_settle.available_permits();
    let guard = rig.storage.hold_writer();
    queue(&rig, rec("stuck", 1)).await;
    until("the writer to be stuck", || queued(&rig) == 0).await;

    // A marker queues and its waiter gives up after the budget.
    let first = spawn_get(&app, SETTLE);
    until("the marker to be queued", || queued(&rig) == 1).await;
    tokio::time::advance(Duration::from_secs(5)).await;
    let reply = finished("the first answer", first).await;
    assert_eq!(reply.header("x-history-settled"), Some("false"));
    assert_eq!((queued(&rig), permits()), (1, 0), "the marker stays");

    // A retry cannot get the permit and is answered unsettled within the
    // budget, without queueing a second marker.
    let retry = spawn_get(&app, SETTLE);
    // Nothing observable says the retry is waiting for the permit, so give it
    // time to get there: it takes one turn of the runtime.
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert!(!retry.is_finished());
    assert_eq!((queued(&rig), permits()), (1, 0));
    tokio::time::advance(Duration::from_secs(5)).await;
    let reply = finished("the retry's answer", retry).await;
    assert_eq!(reply.header("x-history-settled"), Some("false"));
    assert_eq!((queued(&rig), permits()), (1, 0), "still one marker");

    // The writer is released: it consumes the stale marker, whose answer goes
    // to nobody, and the permit is back.
    drop(guard);
    until("the stale marker to be consumed", || {
        queued(&rig) == 0 && permits() == 1
    })
    .await;

    // The next retry queues its own marker and is answered settled.
    let reply = get(&app, SETTLE).await;
    assert_eq!(reply.header("x-history-settled"), Some("true"));
    assert_eq!((queued(&rig), permits()), (0, 1));

    // Ten requests at once against a writer that stays stalled for 30 s: the
    // queue never holds more than one marker, and every request is answered
    // when its own budget runs out, not one budget after another.
    let guard = rig.storage.hold_writer();
    queue(&rig, rec("stuck-again", 2)).await;
    until("the writer to be stuck again", || queued(&rig) == 0).await;
    let started = tokio::time::Instant::now();
    let requests: Vec<_> = (0..10).map(|_| spawn_get(&app, SETTLE)).collect();
    for _ in 0..20 {
        tokio::task::yield_now().await;
        assert!(queued(&rig) <= 1, "{} markers", queued(&rig));
    }
    assert_eq!(queued(&rig), 1);
    tokio::time::advance(Duration::from_secs(5)).await;
    for request in requests {
        let reply = finished("an answer of the ten", request).await;
        assert_eq!(reply.header("x-history-settled"), Some("false"));
        assert!(queued(&rig) <= 1);
    }
    assert!(started.elapsed() <= Duration::from_secs(5) + Duration::from_millis(50));
    tokio::time::advance(Duration::from_secs(25)).await;
    assert_eq!((queued(&rig), permits()), (1, 0));
    drop(guard);
    until("the writer to catch up", || {
        queued(&rig) == 0 && permits() == 1
    })
    .await;
}

#[tokio::test]
async fn the_permit_comes_back_when_the_queue_is_closed() {
    let mut rig = Setup::new().without_record_writer().build();
    let app = rig.app();
    // The writer's end is gone, but the proxy's handle is still there.
    drop(rig.record_rx.take());
    start_capture();

    let reply = get(&app, SETTLE).await;

    assert_eq!(reply.header("x-history-settled"), Some("false"));
    assert_eq!(rig.state.history_settle.available_permits(), 1);
    assert_eq!(
        lines_with(
            &take_capture(),
            Level::Warn,
            "Storage writer stopped; history served without waiting for it"
        ),
        1
    );

    // Once the proxy's handles are gone too, there is no queue to upgrade to.
    drop(rig.record_tx);
    let reply = get(&app, SETTLE).await;
    assert_eq!(reply.header("x-history-settled"), Some("false"));
    assert_eq!(rig.state.history_settle.available_permits(), 1);
    assert_eq!(rig.state.history.available_permits(), 1);
}

#[tokio::test(start_paused = true)]
async fn the_permit_comes_back_when_the_wait_for_room_in_a_full_queue_runs_out() {
    let mut rig = Setup::new().without_record_writer().build();
    let app = rig.app();
    let mut writer_end = rig.record_rx.take().unwrap();
    let capacity = rig.record_tx.max_capacity();
    for n in 0..capacity {
        rig.record_tx
            .try_send(WriterItem::Record(rec(&format!("f{n}"), 1)))
            .unwrap();
    }
    start_capture();

    // The marker waits for room for its whole budget, then is given up.
    let reply = get(&app, SETTLE).await;

    assert_eq!(reply.header("x-history-settled"), Some("false"));
    assert_eq!(rig.state.history_settle.available_permits(), 1);
    assert_eq!(queued(&rig), capacity, "no room was made for a marker");
    for _ in 0..capacity {
        match writer_end.try_recv() {
            Ok(WriterItem::Record(_)) => {}
            Ok(WriterItem::Settle { .. }) => panic!("a marker was queued"),
            Err(e) => panic!("fewer records than were queued: {e}"),
        }
    }
    assert!(writer_end.try_recv().is_err());
    assert_eq!(
        lines_with(
            &take_capture(),
            Level::Warn,
            "Storage writer is behind; history served without waiting for it"
        ),
        1
    );
}

#[tokio::test]
async fn the_permit_comes_back_when_the_receiver_is_dropped_with_a_marker_queued() {
    let mut rig = Setup::new().without_record_writer().build();
    let app = rig.app();
    let writer_end = rig.record_rx.take().unwrap();

    let request = spawn_get(&app, SETTLE);
    until("the marker to be queued", || queued(&rig) == 1).await;
    assert_eq!(rig.state.history_settle.available_permits(), 0);
    drop(writer_end);
    let reply = finished("the answer", request).await;

    assert_eq!(reply.header("x-history-settled"), Some("false"));
    assert_eq!(rig.state.history_settle.available_permits(), 1);
}

#[tokio::test]
async fn the_permit_comes_back_when_the_writer_task_is_aborted_with_a_marker_queued() {
    let (_tmp, mut rig) = {
        let tmp = TempDb::new("api-aborted-writer");
        let storage = Storage::open(&tmp.path()).unwrap();
        (
            tmp,
            Setup::new()
                .storage(storage)
                .without_record_writer()
                .build(),
        )
    };
    let app = rig.app();
    let storage = rig.storage.clone();
    let writer = run_writer_with(&mut rig, flaky_insert(storage.clone()));
    let guard = storage.hold_writer();
    queue(&rig, rec("stuck", 1)).await;
    until("the writer to be stuck", || queued(&rig) == 0).await;
    let request = spawn_get(&app, SETTLE);
    until("the marker to be queued", || queued(&rig) == 1).await;

    writer.abort();
    let reply = finished("the answer", request).await;
    drop(guard);

    assert_eq!(reply.header("x-history-settled"), Some("false"));
    assert_eq!(rig.state.history_settle.available_permits(), 1);
}

#[tokio::test]
async fn the_permit_comes_back_when_the_writer_task_panics_with_a_marker_queued() {
    let mut rig = Setup::new().without_record_writer().build();
    let app = rig.app();
    let mut writer_end = rig.record_rx.take().unwrap();
    let (die, dies) = tokio::sync::oneshot::channel::<()>();
    // A writer that dies, taking its queue with it.
    let writer = tokio::spawn(async move {
        let _ = dies.await;
        let _ = writer_end.try_recv();
        panic!("the writer task panics");
    });
    let request = spawn_get(&app, SETTLE);
    until("the marker to be queued", || queued(&rig) == 1).await;

    die.send(()).unwrap();
    let reply = finished("the answer", request).await;

    assert!(writer.await.unwrap_err().is_panic());
    assert_eq!(reply.header("x-history-settled"), Some("false"));
    assert_eq!(rig.state.history_settle.available_permits(), 1);
}

// ---------------------------------------------------------------------------
// The /ws feed
// ---------------------------------------------------------------------------

/// Wait until `count` sockets are subscribed to the record feed.
async fn subscribed(rig: &Rig, count: usize) {
    until("the sockets to subscribe", || {
        rig.broadcast_tx.receiver_count() == count
    })
    .await;
}

/// The id in a frame of the record feed.
fn frame_id(frame: &Value) -> String {
    assert_eq!(frame["type"], "message");
    frame["data"]["id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn the_feed_is_subscribed_before_the_upgrade_is_answered() {
    let rig = Setup::new().build();
    // Hold the answer back once the handler has produced it: the handshake
    // does not finish until the test lets it.
    let (ready_tx, mut ready) = tokio::sync::mpsc::unbounded_channel::<()>();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let hold = {
        let release = release.clone();
        middleware::from_fn(
            move |request: axum::extract::Request, next: middleware::Next| {
                let ready_tx = ready_tx.clone();
                let release = release.clone();
                async move {
                    let response = next.run(request).await;
                    let _ = ready_tx.send(());
                    let _wait = release.acquire().await.unwrap();
                    response
                }
            },
        )
    };
    let addr = serve(rig.app().layer(hold)).await;

    let connecting = tokio::spawn(async move { WsClient::connect(addr, "/ws", None).await });
    ready.recv().await.unwrap();

    assert_eq!(
        rig.broadcast_tx.receiver_count(),
        1,
        "the socket listens before it is told it is connected"
    );
    assert!(!connecting.is_finished(), "the 101 is still held back");
    release.add_permits(1);
    connecting.await.unwrap().unwrap();
}

#[tokio::test]
async fn a_marker_sent_after_the_socket_opens_is_queued_with_the_subscription_in_place() {
    let mut rig = Setup::new().without_record_writer().build();
    let app = rig.app();
    let addr = serve(app.clone()).await;
    let _client = WsClient::connect(addr, "/ws", None).await.unwrap();
    let mut writer_end = rig.record_rx.take().unwrap();

    // The page's `open` event has fired; now it asks for its reload.
    let request = spawn_get(&app, SETTLE);
    let marker = timeout(GUARD, writer_end.recv()).await.unwrap().unwrap();

    let WriterItem::Settle { done, permit } = marker else {
        panic!("the first item queued by the reload is not a marker");
    };
    assert_eq!(rig.broadcast_tx.receiver_count(), 1);
    done.send(()).unwrap();
    drop(permit);
    let reply = finished("the answer", request).await;
    assert_eq!(reply.header("x-history-settled"), Some("true"));
}

#[tokio::test]
async fn a_client_that_falls_behind_is_closed_with_1013() {
    let rig = Setup::new().broadcast_capacity(4).build();
    let addr = serve(rig.app()).await;
    let mut client = WsClient::connect(addr, "/ws", None).await.unwrap();
    subscribed(&rig, 1).await;
    start_capture();

    // Twenty records before the server's task has had a turn: the client's
    // place in the channel is overwritten.
    for n in 0..20 {
        let json = make_ws_json(&rec(&format!("b{n:02}"), n)).unwrap();
        rig.broadcast_tx.send(json).unwrap();
    }
    while client.text().await.is_some() {}

    let (code, reason) = client.closed_with.clone().expect("a close frame");
    assert_eq!(code, 1013);
    assert_eq!(reason, "fell behind; reconnect and reload");
    until("the socket to be dropped", || {
        rig.broadcast_tx.receiver_count() == 0
    })
    .await;
    assert_eq!(
        lines_with(&take_capture(), Level::Warn, "WebSocket client lagged"),
        1
    );
}

/// A small receive buffer for the client that never reads, so that the
/// kernel's buffers fill after as little as it allows.
const STALLED_RCVBUF: u32 = 4096;

/// Open a connection that a server cannot write to for long: a small receive
/// buffer and no reads.
async fn stalled_connection(addr: SocketAddr) -> tokio::net::TcpStream {
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.set_recv_buffer_size(STALLED_RCVBUF).unwrap();
    socket.connect(addr).await.unwrap()
}

/// How many bytes the kernel takes from a writer on this system before a write
/// would block, when the reader never reads and has a small receive buffer.
///
/// Buffers grow by themselves (autotuning), so a test that sends a fixed
/// amount and hopes it is enough could pass without any backpressure at all.
async fn bytes_accepted_before_a_write_blocks() -> usize {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let reader = stalled_connection(addr).await;
    let (writer, _) = listener.accept().await.unwrap();
    let chunk = vec![0u8; 64 * 1024];
    let mut total = 0;
    // Write while the socket says it can take more; when it stays unwritable
    // for a moment, the buffers are full.
    while timeout(Duration::from_millis(500), writer.writable())
        .await
        .is_ok()
    {
        match writer.try_write(&chunk) {
            Ok(n) => total += n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => panic!("write failed: {e}"),
        }
    }
    drop(reader);
    total
}

#[tokio::test]
async fn a_client_that_stops_accepting_data_is_dropped_and_the_others_carry_on() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const FRAME: usize = 64 * 1024;
    let accepted = bytes_accepted_before_a_write_blocks().await;
    // Enough frames to fill the buffers, and fewer than the channel holds, so
    // that the client cannot fall behind in the channel's sense first.
    let frames = (accepted * 2 / FRAME + 8).min(400);
    println!(
        "the kernel accepts {accepted} bytes before a write blocks; \
         sending {frames} frames of {FRAME} bytes"
    );
    assert!(frames * FRAME > accepted, "not enough to fill the buffers");
    let text = |n: usize| format!("{n:05}{}", "x".repeat(FRAME - 5));

    let rig = Setup::new().build();
    let addr = serve(rig.app()).await;
    let mut good = WsClient::connect(addr, "/ws", None).await.unwrap();
    // A client that completes the handshake and then never reads.
    let mut stalled = stalled_connection(addr).await;
    stalled
        .write_all(
            format!(
                "GET /ws HTTP/1.1\r\nHost: {addr}\r\nUpgrade: websocket\r\n\
                 Connection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
                 Sec-WebSocket-Version: 13\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut head = vec![0u8; 512];
    let n = stalled.read(&mut head).await.unwrap();
    assert!(String::from_utf8_lossy(&head[..n]).contains(" 101 "));
    subscribed(&rig, 2).await;
    start_capture();

    // Records before the stall reach both.
    rig.broadcast_tx.send(Arc::new(text(0))).unwrap();
    assert_eq!(good.text().await.unwrap(), text(0));

    let started = Instant::now();
    for n in 1..=frames {
        rig.broadcast_tx.send(Arc::new(text(n))).unwrap();
    }
    // The client that reads gets every one.
    for n in 1..=frames {
        assert_eq!(good.text().await.unwrap(), text(n), "frame {n}");
    }
    // The one that does not is dropped once a send has stalled for 10 s and
    // its goodbye has had a second.
    let deadline = Duration::from_secs(10 + 1 + 4);
    while rig.broadcast_tx.receiver_count() == 2 {
        assert!(
            started.elapsed() < deadline,
            "the stalled client is still there"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let dropped_after = started.elapsed();
    println!("the stalled client was dropped after {dropped_after:?}");
    assert!(
        dropped_after >= Duration::from_secs(9),
        "dropped after {dropped_after:?}, before the send could have timed out"
    );
    let lines = take_capture();
    assert_eq!(
        lines_with(&lines, Level::Warn, "did not accept a message within 10s"),
        1,
        "{lines:?}"
    );
    assert_eq!(
        lines_with(&lines, Level::Warn, "lagged"),
        0,
        "it did not fall behind in the channel: {lines:?}"
    );

    // Everyone else carries on.
    rig.broadcast_tx.send(Arc::new(text(frames + 1))).unwrap();
    assert_eq!(good.text().await.unwrap(), text(frames + 1));
    assert_eq!(rig.broadcast_tx.receiver_count(), 1);
    drop(stalled);
}

/// A reload after a reconnect, with the newest records of the range.
fn reload_uri() -> String {
    format!(
        "/api/messages?from={}&newest=true&count=true&limit=5000&settle=true",
        stamp(-300)
    )
}

/// A record held by the gateway task for 15 s between being stamped and being
/// emitted, across a reconnect.
async fn run_withheld(emit_after_the_socket_opens: bool) {
    let (_tmp, rig) = file_rig("api-withheld");
    let app = rig.app();
    let addr = serve(app.clone()).await;
    let outputs = Outputs::new(rig.record_tx.clone(), rig.broadcast_tx.clone());
    let held = rec("held", -15);
    let mut first = WsClient::connect(addr, "/ws", None).await.unwrap();
    subscribed(&rig, 1).await;

    // Records stamped later are emitted, and received.
    for n in 0..5 {
        outputs.emit(rec(&format!("live{n}"), n));
    }
    for n in 0..5 {
        assert_eq!(frame_id(&first.json().await), format!("live{n}"));
    }
    drop(first);
    until("the first socket to go", || {
        rig.broadcast_tx.receiver_count() == 0
    })
    .await;

    let mut second;
    if emit_after_the_socket_opens {
        second = WsClient::connect(addr, "/ws", None).await.unwrap();
        subscribed(&rig, 1).await;
        outputs.emit(held);
    } else {
        // Emitted with nobody listening: saved, but broadcast to no one.
        outputs.emit(held);
        second = WsClient::connect(addr, "/ws", None).await.unwrap();
        subscribed(&rig, 1).await;
    }

    // The page's `open` fired, so it reloads.
    let reply = get(&app, &reload_uri()).await;

    let mut want = vec!["held".to_string()];
    want.extend((0..5).map(|n| format!("live{n}")));
    assert_eq!(ids(&reply), want, "no hole and no repeat, in time order");
    assert_eq!(reply.header("x-history-settled"), Some("true"));
    assert_eq!(reply.header("x-total-count"), Some("6"));
    if emit_after_the_socket_opens {
        // It also arrived on the new socket, which is where the page would
        // find it a second time and keep one copy.
        assert_eq!(frame_id(&second.json().await), "held");
    }
}

#[tokio::test]
async fn a_record_withheld_past_any_overlap_is_in_the_reload_once() {
    run_withheld(false).await;
}

#[tokio::test]
async fn a_record_emitted_just_after_the_socket_opens_is_on_the_socket_and_in_the_reload() {
    run_withheld(true).await;
}

#[tokio::test]
async fn a_record_seen_live_whose_insert_fails_is_counted_and_missing_from_the_reload() {
    let tmp = TempDb::new("api-live-failure");
    let storage = Storage::open(&tmp.path()).unwrap();
    let mut rig = Setup::new()
        .storage(storage.clone())
        .without_record_writer()
        .build();
    let _writer = run_writer_with(&mut rig, flaky_insert(storage.clone()));
    let app = rig.app();
    let addr = serve(app.clone()).await;
    let outputs = Outputs::new(rig.record_tx.clone(), rig.broadcast_tx.clone());
    let mut client = WsClient::connect(addr, "/ws", None).await.unwrap();
    subscribed(&rig, 1).await;

    // The writer is stalled; a record arrives whose insert will fail, and a
    // reload is sent after it.
    let guard = storage.hold_writer();
    outputs.emit(rec("stuck", 1));
    until("the writer to be stuck", || queued(&rig) == 0).await;
    outputs.emit(rec("bad-live", 2));
    let reload = spawn_get(&app, SETTLE);
    until("the marker to be queued", || queued(&rig) == 2).await;
    drop(guard);
    let reply = finished("the reload", reload).await;

    // The page saw both records live.  The reload holds the one that was
    // saved, and says that one insert failed.
    assert_eq!(frame_id(&client.json().await), "stuck");
    assert_eq!(frame_id(&client.json().await), "bad-live");
    assert_eq!(ids(&reply), names(&["stuck"]));
    assert_eq!(reply.header("x-history-settled"), Some("true"));
    assert_eq!(reply.header("x-storage-write-failures"), Some("1"));
}

// ---------------------------------------------------------------------------
// Forwarding does not wait
// ---------------------------------------------------------------------------

/// A PUSH_DATA datagram from a gateway, carrying one uplink.
fn push_data(token: u16, uplink: &[u8]) -> Vec<u8> {
    use base64::prelude::*;
    let mut datagram = vec![2, (token >> 8) as u8, token as u8, 0x00];
    datagram.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD, 0, 0, 0, 1]);
    datagram.extend_from_slice(
        format!(
            r#"{{"rxpk":[{{"freq":868.1,"datr":"SF7BW125","codr":"4/5","rssi":-50,"lsnr":7.5,"stat":1,"size":{},"data":"{}"}}]}}"#,
            uplink.len(),
            BASE64_STANDARD.encode(uplink)
        )
        .as_bytes(),
    );
    datagram
}

#[tokio::test]
async fn the_proxy_keeps_forwarding_while_a_marker_waits_for_a_stalled_writer() {
    use tokio::net::UdpSocket;
    use tokio_util::sync::CancellationToken;

    let (_tmp, rig) = file_rig("api-forwarding");
    let app = rig.app();
    let guard = rig.storage.hold_writer();
    queue(&rig, rec("stuck", 1)).await;
    until("the writer to be stuck", || queued(&rig) == 0).await;
    let waiting = spawn_get(&app, SETTLE);
    until("the marker to be queued", || queued(&rig) == 1).await;

    // The proxy between a gateway and a network server, both on loopback.
    let lns = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let lns_addr = lns.local_addr().unwrap();
    let proxy_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_socket.local_addr().unwrap();
    let cancel = CancellationToken::new();
    let proxy = tokio::spawn({
        let outputs = Outputs::new(rig.record_tx.clone(), rig.broadcast_tx.clone());
        let cancel = cancel.clone();
        async move {
            crate::proxy::udp::run_with_socket(
                proxy_socket,
                &crate::config::UdpConfig::default(),
                Some(lns_addr),
                &[],
                outputs,
                cancel,
            )
            .await
        }
    });
    // The network server acknowledges every PUSH_DATA, as one does.
    let reached = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let server = tokio::spawn({
        let reached = reached.clone();
        async move {
            let mut buf = vec![0u8; 65_536];
            loop {
                let (len, from) = lns.recv_from(&mut buf).await.unwrap();
                if len >= 4 && buf[3] == 0x00 {
                    reached.fetch_add(1, Ordering::SeqCst);
                    lns.send_to(&[2, buf[1], buf[2], 0x01], from).await.unwrap();
                }
            }
        }
    });

    // A data frame: MHDR, DevAddr, FCtrl, FCnt, FPort, payload, MIC.
    let uplink = [
        0x40, 0x01, 0x10, 0x00, 0x00, 0x00, 0x05, 0x00, 0x01, 0x11, 0x22, 0xDE, 0xAD, 0xBE, 0xEF,
    ];
    let gateway = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    start_capture();
    // More datagrams than the queue has room for: it fills, and the records
    // that do not fit are dropped, while every datagram is still forwarded
    // and acknowledged at once.
    let capacity = rig.record_tx.max_capacity();
    let sent = capacity + 100;
    let mut slowest = Duration::ZERO;
    for n in 0..sent {
        let started = Instant::now();
        gateway
            .send_to(&push_data(n as u16, &uplink), proxy_addr)
            .await
            .unwrap();
        let mut ack = [0u8; 64];
        let (len, _) = timeout(Duration::from_secs(2), gateway.recv_from(&mut ack))
            .await
            .unwrap_or_else(|_| panic!("no PUSH_ACK for datagram {n}"))
            .unwrap();
        assert_eq!(&ack[..len], &[2, (n >> 8) as u8, n as u8, 0x01]);
        slowest = slowest.max(started.elapsed());
    }

    println!("the slowest of {sent} PUSH_ACKs took {slowest:?}");
    assert!(
        slowest < Duration::from_millis(100),
        "the slowest PUSH_ACK took {slowest:?}"
    );
    assert_eq!(reached.load(Ordering::SeqCst), sent);
    assert_eq!(queued(&rig), capacity, "the queue is full");
    assert_eq!(
        lines_with(
            &take_capture(),
            Level::Warn,
            "Storage queue full, dropping record"
        ),
        1,
        "the usual warning, once"
    );
    assert!(!waiting.is_finished(), "the marker is still waiting");

    cancel.cancel();
    proxy.await.unwrap().unwrap();
    server.abort();
    drop(guard);
    // The marker was queued ahead of the flood, so it is answered once the
    // record before it is saved; the records behind it follow.
    let reply = finished("the waiting request", waiting).await;
    assert_eq!(reply.header("x-history-settled"), Some("true"));
    until("the writer to catch up", || queued(&rig) == 0).await;
    let reply = get(&app, SETTLE).await;
    assert_eq!(reply.header("x-history-settled"), Some("true"));
    assert_eq!(
        reply.header("x-total-count"),
        Some(capacity.to_string().as_str()),
        "the record the writer was stuck on and the queue's worth behind it"
    );
}

// ---------------------------------------------------------------------------
// The storage writer
// ---------------------------------------------------------------------------

use std::sync::atomic::AtomicU64;
use std::sync::Mutex;
use tokio::sync::{oneshot, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::cmd::root::{process_items, run_writer};

/// A marker with its own permit, and the end that is told when it is answered.
async fn marker() -> (WriterItem, oneshot::Receiver<()>, Arc<Semaphore>) {
    let permits = Arc::new(Semaphore::new(1));
    let permit = permits.clone().acquire_owned().await.unwrap();
    let (done, answered) = oneshot::channel();
    (WriterItem::Settle { done, permit }, answered, permits)
}

/// An insert that waits for a release before it saves a record whose id
/// starts with `hold`, and fails or panics like [`flaky_insert`].
fn gated_insert(
    storage: Storage,
    releases: std::sync::mpsc::Receiver<()>,
) -> impl Fn(MessageRecord) -> Result<(), SnifferError> + Clone + Send + Sync + 'static {
    let releases = Arc::new(Mutex::new(releases));
    let flaky = flaky_insert(storage);
    move |record| {
        if record.id.starts_with("hold") {
            releases.lock().unwrap().recv().unwrap();
        }
        flaky(record)
    }
}

#[tokio::test]
async fn the_writer_saves_what_it_is_given_and_answers_a_marker_after_it() {
    let storage = Storage::open_in_memory().unwrap();
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let failures = Arc::new(AtomicU64::new(0));
    let cancel = CancellationToken::new();
    let writer = tokio::spawn(run_writer(
        rx,
        storage.clone(),
        failures.clone(),
        cancel.clone(),
    ));
    for n in 0..3 {
        tx.send(WriterItem::Record(rec(&format!("w{n}"), n)))
            .await
            .unwrap();
    }
    let (item, answered, permits) = marker().await;
    tx.send(item).await.unwrap();

    timeout(GUARD, answered).await.unwrap().unwrap();

    assert_eq!(storage.count().unwrap(), 3);
    assert_eq!(failures.load(Ordering::Relaxed), 0);
    until("the permit to come back", || {
        permits.available_permits() == 1
    })
    .await;
    cancel.cancel();
    writer.await.unwrap();
}

#[tokio::test]
async fn a_marker_is_answered_when_exactly_the_items_before_it_are_saved() {
    let storage = Storage::open_in_memory().unwrap();
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let (release, releases) = std::sync::mpsc::channel();
    let cancel = CancellationToken::new();
    let writer = tokio::spawn(process_items(
        rx,
        gated_insert(storage.clone(), releases),
        Arc::new(AtomicU64::new(0)),
        cancel.clone(),
    ));
    tx.send(WriterItem::Record(rec("before-1", 1)))
        .await
        .unwrap();
    tx.send(WriterItem::Record(rec("before-2", 2)))
        .await
        .unwrap();
    let (item, answered, _permits) = marker().await;
    tx.send(item).await.unwrap();
    // The records behind the marker cannot be saved until they are released.
    tx.send(WriterItem::Record(rec("hold-1", 3))).await.unwrap();
    tx.send(WriterItem::Record(rec("hold-2", 4))).await.unwrap();

    timeout(GUARD, answered).await.unwrap().unwrap();

    assert_eq!(storage.count().unwrap(), 2, "exactly the records before it");
    release.send(()).unwrap();
    release.send(()).unwrap();
    until("the rest to be saved", || storage.count().unwrap() == 4).await;
    cancel.cancel();
    writer.await.unwrap();
}

#[tokio::test]
async fn a_failed_insert_is_counted_before_a_later_marker_is_answered() {
    let storage = Storage::open_in_memory().unwrap();
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let failures = Arc::new(AtomicU64::new(0));
    let cancel = CancellationToken::new();
    let writer = tokio::spawn(process_items(
        rx,
        flaky_insert(storage.clone()),
        failures.clone(),
        cancel.clone(),
    ));
    start_capture();
    tx.send(WriterItem::Record(rec("bad-1", 1))).await.unwrap();
    tx.send(WriterItem::Record(rec("panic-1", 2)))
        .await
        .unwrap();
    tx.send(WriterItem::Record(rec("fine", 3))).await.unwrap();
    let (item, answered, _permits) = marker().await;
    tx.send(item).await.unwrap();

    timeout(GUARD, answered).await.unwrap().unwrap();

    // Both failures were counted, and the record after them saved.
    assert_eq!(failures.load(Ordering::Relaxed), 2);
    assert_eq!(storage.count().unwrap(), 1);
    let lines = take_capture();
    assert_eq!(lines_with(&lines, Level::Error, "Storage write error"), 1);
    assert_eq!(
        lines_with(&lines, Level::Error, "Storage writer task panicked"),
        1
    );
    cancel.cancel();
    writer.await.unwrap();
}

#[tokio::test]
async fn the_shutdown_drain_answers_markers_and_releases_the_permit_of_one_nobody_waits_for() {
    let storage = Storage::open_in_memory().unwrap();
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let (release, releases) = std::sync::mpsc::channel();
    let cancel = CancellationToken::new();
    let writer = tokio::spawn(process_items(
        rx,
        gated_insert(storage.clone(), releases),
        Arc::new(AtomicU64::new(0)),
        cancel.clone(),
    ));
    // A marker whose request gave up, a marker whose request is still
    // waiting, and records around them, all still queued when the sniffer is
    // told to stop.
    tx.send(WriterItem::Record(rec("hold-1", 1))).await.unwrap();
    let (stale, stale_answer, stale_permits) = marker().await;
    drop(stale_answer);
    let (live, live_answer, live_permits) = marker().await;
    tx.send(stale).await.unwrap();
    tx.send(WriterItem::Record(rec("between", 2)))
        .await
        .unwrap();
    tx.send(live).await.unwrap();
    tx.send(WriterItem::Record(rec("after", 3))).await.unwrap();
    assert_eq!(stale_permits.available_permits(), 0);
    cancel.cancel();
    release.send(()).unwrap();

    timeout(GUARD, live_answer).await.unwrap().unwrap();
    writer.await.unwrap();

    assert_eq!(
        stale_permits.available_permits(),
        1,
        "the stale marker's permit"
    );
    assert_eq!(live_permits.available_permits(), 1);
    assert_eq!(storage.count().unwrap(), 3, "nothing queued was lost");
}

#[tokio::test]
async fn the_shutdown_drain_logs_and_counts_the_inserts_that_fail() {
    let storage = Storage::open_in_memory().unwrap();
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let failures = Arc::new(AtomicU64::new(0));
    let cancel = CancellationToken::new();
    for id in ["bad-1", "ok", "panic-1"] {
        tx.send(WriterItem::Record(rec(id, 1))).await.unwrap();
    }
    start_capture();
    cancel.cancel();

    process_items(rx, flaky_insert(storage.clone()), failures.clone(), cancel).await;

    assert_eq!(failures.load(Ordering::Relaxed), 2);
    assert_eq!(storage.count().unwrap(), 1);
    let lines = take_capture();
    assert_eq!(lines_with(&lines, Level::Error, "Storage write error"), 1);
    assert_eq!(
        lines_with(&lines, Level::Error, "Storage writer task panicked"),
        1
    );
}
