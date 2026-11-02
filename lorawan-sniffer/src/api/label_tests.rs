//! Tests of the label endpoints, the Host check as the router applies it, the
//! label change feed and the writer behind them.
//!
//! The in-process router cannot upgrade a connection, so everything that
//! needs a WebSocket runs against a real listener on loopback, with a small
//! hand-written client at the bottom of this file.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

use axum::http::StatusCode;
use axum::Router;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

use super::tests::{
    frame_of_noise_dev_addr, record_from_frame, record_with_new_fields, send, Reply, Rig, Setup,
    NOISE_DEV_ADDR,
};
use super::*;
use crate::cmd::root::run_seen_listener;
use crate::core::labels::{LabelError, MAX_LABELS};
use crate::core::metadata::{CrcStatus, MType};

const ADDR: &str = "00E8A80B";
const EUI: &str = "36:31:39:54:36:5D:05:39";

/// Failure guard for waits that should end at once.  Nothing here sleeps to
/// let something happen; this only turns a hang into a failure.
const GUARD: Duration = Duration::from_secs(10);

const JSON_BODY: (&str, &str) = ("content-type", "application/json");

fn data_record(dev_addr: &str) -> MessageRecord {
    MessageRecord {
        id: format!("data-{dev_addr}"),
        dev_addr: Some(dev_addr.to_string()),
        ..record_with_new_fields()
    }
}

fn join_record(dev_eui: &str) -> MessageRecord {
    MessageRecord {
        id: format!("join-{dev_eui}"),
        mtype: MType::JoinRequest,
        dev_addr: None,
        dev_eui: Some(dev_eui.to_string()),
        fcnt: None,
        fport: None,
        ..record_with_new_fields()
    }
}

fn label_body(label: &str) -> String {
    json!({ "label": label }).to_string()
}

async fn put_raw(app: &Router, device: &str, body: &str, extra: &[(&str, &str)]) -> Reply {
    let mut headers = vec![JSON_BODY];
    headers.extend_from_slice(extra);
    send(
        app,
        "PUT",
        &format!("/api/labels/{device}"),
        &headers,
        Some(body),
    )
    .await
}

async fn put(app: &Router, device: &str, label: &str) -> Reply {
    put_raw(app, device, &label_body(label), &[]).await
}

async fn delete(app: &Router, device: &str, extra: &[(&str, &str)]) -> Reply {
    send(app, "DELETE", &format!("/api/labels/{device}"), extra, None).await
}

async fn get_labels(app: &Router) -> Value {
    let reply = send(app, "GET", "/api/labels", &[], None).await;
    assert_eq!(reply.status, StatusCode::OK);
    reply.json()
}

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

fn event(device: &str, label: Option<&str>) -> Value {
    json!({"type": "label", "data": {"device": device, "label": label}})
}

/// Take every event published so far from a receiver, in order.
fn drain(rx: &mut broadcast::Receiver<Arc<String>>) -> Vec<Value> {
    let mut out = Vec::new();
    while let Ok(json) = rx.try_recv() {
        out.push(serde_json::from_str(&json).unwrap());
    }
    out
}

// ---------------------------------------------------------------------------
// GET /api/labels
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_labels_is_an_empty_object_when_there_are_none() {
    let rig = Setup::new().build();

    let reply = send(&rig.app(), "GET", "/api/labels", &[], None).await;

    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply
        .header("content-type")
        .is_some_and(|t| t.starts_with("application/json")));
    assert_eq!(reply.json(), json!({}));
}

#[tokio::test]
async fn get_labels_returns_every_label_keyed_by_identifier() {
    let rig = Setup::new().build();
    rig.storage
        .set_label(ADDR, "Qingping T&H Havana", true, MAX_LABELS)
        .unwrap();
    rig.storage
        .set_label(EUI, "Havana join", true, MAX_LABELS)
        .unwrap();

    // Like /api/messages, reading does not look at the Origin.
    let reply = send(
        &rig.app(),
        "GET",
        "/api/labels",
        &[("origin", "http://evil.example")],
        None,
    )
    .await;

    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        reply.json(),
        json!({ADDR: "Qingping T&H Havana", EUI: "Havana join"})
    );
    assert!(reply.header("access-control-allow-origin").is_none());
}

// ---------------------------------------------------------------------------
// PUT /api/labels/{device}
// ---------------------------------------------------------------------------

#[tokio::test]
async fn put_normalises_the_identifier_and_trims_the_label() {
    let rig = Setup::new().build();
    let app = rig.app();
    rig.seen.note(ADDR);
    rig.seen.note(EUI);

    for (raw, want) in [
        ("00e8a80b", ADDR),
        ("00E8A80B", ADDR),
        ("36:31:39:54:36:5d:05:39", EUI),
        ("36-31-39-54-36-5D-05-39", EUI),
        ("36313954365d0539", EUI),
    ] {
        let reply = put(&app, raw, "  Havana sensor \t").await;

        assert_eq!(reply.status, StatusCode::OK, "{raw}: {}", reply.text());
        assert_eq!(
            reply.json(),
            json!({"device": want, "label": "Havana sensor"}),
            "{raw}"
        );
    }
    assert_eq!(
        get_labels(&app).await,
        json!({ADDR: "Havana sensor", EUI: "Havana sensor"})
    );
}

#[tokio::test]
async fn put_replaces_an_existing_label() {
    let rig = Setup::new().build();
    let app = rig.app();
    rig.seen.note(ADDR);

    assert_eq!(put(&app, ADDR, "first").await.status, StatusCode::OK);
    let reply = put(&app, ADDR, "second").await;

    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json(), json!({"device": ADDR, "label": "second"}));
    assert_eq!(get_labels(&app).await, json!({ADDR: "second"}));
}

#[tokio::test]
async fn put_an_empty_label_removes_it_without_any_known_device_check() {
    let rig = Setup::new().build();
    let app = rig.app();
    // A label for a device that is neither stored nor live (it expired from
    // the live list, say): saving a new text would be refused, removing works.

    for blank in ["", "   ", "\t \n"] {
        rig.storage
            .set_label(EUI, "orphan", true, MAX_LABELS)
            .unwrap();
        let reply = put(&app, EUI, blank).await;

        assert_eq!(reply.status, StatusCode::OK, "{blank:?}: {}", reply.text());
        assert_eq!(reply.json(), json!({"device": EUI, "label": null}));
        assert_eq!(get_labels(&app).await, json!({}));
    }

    // Nothing to remove is not an error either.
    let reply = put(&app, ADDR, "").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json(), json!({"device": ADDR, "label": null}));
}

#[tokio::test]
async fn put_rejects_a_bad_identifier() {
    let rig = Setup::new().build();
    let app = rig.app();

    for bad in [
        "nonsense",
        "00E8A80",
        "00E8A80B1",
        "00E8A80G",
        "36:31:39:54:36:5D:05",
        "36:31:39:54:36:5D:05:39:00",
        "3631395436",
    ] {
        let reply = put(&app, bad, "x").await;

        assert_error(
            &reply,
            StatusCode::BAD_REQUEST,
            &LabelError::BadDevice.to_string(),
        );
    }
    assert_eq!(get_labels(&app).await, json!({}));
}

#[tokio::test]
async fn put_accepts_64_characters_and_rejects_65() {
    let rig = Setup::new().build();
    let app = rig.app();
    rig.seen.note(ADDR);

    // Counted in characters, not bytes.
    let at_limit = "é".repeat(64);
    let reply = put(&app, ADDR, &at_limit).await;
    assert_eq!(reply.status, StatusCode::OK);

    let reply = put(&app, ADDR, &"a".repeat(65)).await;
    assert_error(
        &reply,
        StatusCode::BAD_REQUEST,
        &LabelError::TooLong.to_string(),
    );
    // The refused write left the previous label alone.
    assert_eq!(get_labels(&app).await, json!({ADDR: at_limit}));
}

#[tokio::test]
async fn put_rejects_control_characters() {
    let rig = Setup::new().build();
    let app = rig.app();
    rig.seen.note(ADDR);

    for bad in [
        "two\nlines",
        "tab\there",
        "bell\u{7}",
        "del\u{7f}",
        "c\u{85}1",
    ] {
        let reply = put(&app, ADDR, bad).await;

        assert_error(
            &reply,
            StatusCode::BAD_REQUEST,
            &LabelError::ControlChar.to_string(),
        );
    }
    assert_eq!(get_labels(&app).await, json!({}));
}

#[tokio::test]
async fn put_rejects_a_body_that_is_not_a_label_object() {
    let rig = Setup::new().build();
    let app = rig.app();
    rig.seen.note(ADDR);

    let too_big = format!(r#"{{"label": "{}"}}"#, "a".repeat(10_000));
    for body in [
        "",
        "{",
        "not json",
        "{}",
        r#"{"text": "x"}"#,
        r#"{"label": 5}"#,
        r#"{"label": null}"#,
        r#"{"label": ["x"]}"#,
        r#"["x"]"#,
        r#""x""#,
        too_big.as_str(),
    ] {
        let reply = put_raw(&app, ADDR, body, &[]).await;

        assert_error(&reply, StatusCode::BAD_REQUEST, "JSON object");
    }
    assert_eq!(get_labels(&app).await, json!({}));
}

#[tokio::test]
async fn put_without_a_json_content_type_is_415() {
    let rig = Setup::new().build();
    let app = rig.app();
    rig.seen.note(ADDR);
    let body = label_body("x");

    for headers in [
        vec![("content-type", "text/plain")],
        vec![("content-type", "application/x-www-form-urlencoded")],
        vec![("content-type", "multipart/form-data; boundary=x")],
        vec![],
    ] {
        let reply = send(
            &app,
            "PUT",
            &format!("/api/labels/{ADDR}"),
            &headers,
            Some(&body),
        )
        .await;

        assert_error(
            &reply,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "application/json",
        );
    }
    assert_eq!(get_labels(&app).await, json!({}));
}

#[tokio::test]
async fn put_for_a_device_never_seen_is_404() {
    let rig = Setup::new().build();
    let app = rig.app();

    let reply = put(&app, ADDR, "ghost").await;

    assert_error(&reply, StatusCode::NOT_FOUND, "unknown device");
    assert_error(&reply, StatusCode::NOT_FOUND, "retention period");
    assert_eq!(get_labels(&app).await, json!({}));
}

#[tokio::test]
async fn put_for_a_device_seen_only_live_succeeds() {
    let rig = Setup::new().build();
    let app = rig.app();
    rig.seen.note(ADDR);
    rig.seen.note(EUI);

    assert_eq!(put(&app, ADDR, "live addr").await.status, StatusCode::OK);
    assert_eq!(put(&app, EUI, "live eui").await.status, StatusCode::OK);

    assert_eq!(
        get_labels(&app).await,
        json!({ADDR: "live addr", EUI: "live eui"})
    );
    assert_eq!(rig.storage.count().unwrap(), 0, "no record was involved");
}

#[tokio::test]
async fn put_for_a_device_with_only_a_stored_record_succeeds() {
    let rig = Setup::new().build();
    let app = rig.app();
    rig.storage.insert(&data_record(ADDR)).unwrap();
    rig.storage.insert(&join_record(EUI)).unwrap();

    // Spelled differently from the stored text: it is normalised first.
    assert_eq!(put(&app, "00e8a80b", "stored").await.status, StatusCode::OK);
    assert_eq!(
        put(&app, "36313954365d0539", "joined").await.status,
        StatusCode::OK
    );

    assert_eq!(
        get_labels(&app).await,
        json!({ADDR: "stored", EUI: "joined"})
    );
}

#[tokio::test]
async fn put_at_the_label_limit_is_409_but_replacing_and_removing_still_work() {
    let rig = Setup::new().max_labels(2).build();
    let app = rig.app();
    for device in [ADDR, EUI, "0000000A"] {
        rig.seen.note(device);
    }
    assert_eq!(put(&app, ADDR, "one").await.status, StatusCode::OK);
    assert_eq!(put(&app, EUI, "two").await.status, StatusCode::OK);

    let reply = put(&app, "0000000A", "three").await;
    assert_error(
        &reply,
        StatusCode::CONFLICT,
        "label limit reached (2 labels)",
    );
    assert_error(&reply, StatusCode::CONFLICT, "remove one first");

    // Replacing an existing label is not a new label.
    assert_eq!(put(&app, ADDR, "one, again").await.status, StatusCode::OK);
    // Removing frees a slot.
    assert_eq!(put(&app, EUI, "").await.status, StatusCode::OK);
    assert_eq!(put(&app, "0000000A", "three").await.status, StatusCode::OK);
    assert_eq!(
        get_labels(&app).await,
        json!({ADDR: "one, again", "0000000A": "three"})
    );
}

#[tokio::test]
async fn the_production_writer_uses_the_documented_limit() {
    let writer = LabelWriter::spawn(
        Storage::open_in_memory().unwrap(),
        crate::core::seen::SeenDevices::new(),
        Duration::from_secs(60),
        broadcast::channel(4).0,
        tokio_util::sync::CancellationToken::new(),
    );

    assert_eq!(writer.max_labels(), 10_000);
}

#[tokio::test]
async fn put_from_a_foreign_origin_is_403() {
    let rig = Setup::new().build();
    let app = rig.app();
    rig.seen.note(ADDR);

    for origin in ["http://evil.example", "http://127.0.0.1:9999", "null"] {
        let reply = put_raw(&app, ADDR, &label_body("x"), &[("origin", origin)]).await;

        assert_error(
            &reply,
            StatusCode::FORBIDDEN,
            "cross-origin request refused",
        );
        assert_error(&reply, StatusCode::FORBIDDEN, "cors_origins");
    }
    assert_eq!(get_labels(&app).await, json!({}));
}

#[tokio::test]
async fn put_with_no_origin_or_the_same_origin_succeeds() {
    let rig = Setup::new().build();
    let app = rig.app();
    rig.seen.note(ADDR);

    // curl and scripts send no Origin.
    let reply = put_raw(&app, ADDR, &label_body("no origin"), &[]).await;
    assert_eq!(reply.status, StatusCode::OK);
    // The WebUI sends the origin of the page it came from.
    let reply = put_raw(
        &app,
        ADDR,
        &label_body("same origin"),
        &[("origin", "http://127.0.0.1:8888")],
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);

    assert_eq!(get_labels(&app).await, json!({ADDR: "same origin"}));
}

#[tokio::test]
async fn put_from_a_listed_origin_succeeds_and_carries_the_cors_header() {
    let rig = Setup::new().cors(&["https://ui.example"]).build();
    let app = rig.app();
    rig.seen.note(ADDR);

    let reply = put_raw(
        &app,
        ADDR,
        &label_body("from the dashboard"),
        &[("origin", "https://ui.example")],
    )
    .await;

    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        reply.header("access-control-allow-origin"),
        Some("https://ui.example")
    );
}

/// An HTTP/2 request has no `Host` header: its authority is in the URI.
#[tokio::test]
async fn put_works_with_the_authority_in_the_uri_instead_of_a_host_header() {
    let rig = Setup::new().hosts(&["sniffer.lan"]).build();
    let app = rig.app();
    rig.seen.note(ADDR);
    let uri = format!("https://sniffer.lan:8888/api/labels/{ADDR}");
    let body = label_body("h2");

    let same = send(
        &app,
        "PUT",
        &uri,
        &[JSON_BODY, ("origin", "https://sniffer.lan:8888")],
        Some(&body),
    )
    .await;
    let foreign = send(
        &app,
        "PUT",
        &uri,
        &[JSON_BODY, ("origin", "https://evil.example")],
        Some(&body),
    )
    .await;

    assert_eq!(same.status, StatusCode::OK, "{}", same.text());
    assert_error(&foreign, StatusCode::FORBIDDEN, "cors_origins");
    assert_eq!(get_labels(&app).await, json!({ADDR: "h2"}));
}

#[tokio::test]
async fn put_checks_run_in_a_fixed_order() {
    let rig = Setup::new().build();
    let app = rig.app();
    rig.seen.note(ADDR);
    let foreign = ("origin", "http://evil.example");
    let plain = ("content-type", "text/plain");
    let uri = |device: &str| format!("/api/labels/{device}");

    // Origin comes before everything else.
    let reply = send(&app, "PUT", &uri("bad"), &[foreign, plain], Some("{")).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    // Then the identifier, before the body is looked at.
    let reply = send(&app, "PUT", &uri("bad"), &[plain], Some("{")).await;
    assert_error(&reply, StatusCode::BAD_REQUEST, "DevAddr");
    // Then the body's type and shape...
    let too_long = label_body(&"a".repeat(65));
    let reply = send(&app, "PUT", &uri(ADDR), &[plain], Some(&too_long)).await;
    assert_eq!(reply.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    let reply = put_raw(&app, ADDR, "{", &[]).await;
    assert_error(&reply, StatusCode::BAD_REQUEST, "JSON object");
    // ...and only then the label itself.
    let reply = put_raw(&app, ADDR, &too_long, &[]).await;
    assert_error(&reply, StatusCode::BAD_REQUEST, "64 characters");
}

#[tokio::test]
async fn markup_and_quotes_in_a_label_round_trip_as_plain_text() {
    let rig = Setup::new().build();
    let app = rig.app();
    rig.seen.note(ADDR);
    let nasty = r#"<img src=x onerror="alert(1)"> "q" \ é😀"#;

    let reply = put(&app, ADDR, nasty).await;

    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json()["label"], json!(nasty));
    assert_eq!(get_labels(&app).await, json!({ADDR: nasty}));
}

// ---------------------------------------------------------------------------
// DELETE /api/labels/{device}
// ---------------------------------------------------------------------------

#[tokio::test]
async fn delete_removes_a_label_and_answers_204() {
    let rig = Setup::new().build();
    let app = rig.app();
    rig.seen.note(ADDR);
    rig.seen.note(EUI);
    put(&app, ADDR, "a").await;
    put(&app, EUI, "b").await;

    let reply = delete(&app, ADDR, &[]).await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT);
    assert!(reply.body.is_empty());
    // Any spelling of the identifier finds the label.
    let reply = delete(&app, "36-31-39-54-36-5d-05-39", &[]).await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT);

    assert_eq!(get_labels(&app).await, json!({}));
}

#[tokio::test]
async fn delete_of_an_absent_label_is_204() {
    let rig = Setup::new().build();

    let reply = delete(&rig.app(), ADDR, &[]).await;

    assert_eq!(reply.status, StatusCode::NO_CONTENT);
    assert!(reply.body.is_empty());
}

#[tokio::test]
async fn delete_from_a_foreign_origin_is_403_and_keeps_the_label() {
    let rig = Setup::new().build();
    let app = rig.app();
    rig.seen.note(ADDR);
    put(&app, ADDR, "keep me").await;

    let reply = delete(&app, ADDR, &[("origin", "http://evil.example")]).await;

    assert_error(&reply, StatusCode::FORBIDDEN, "cors_origins");
    assert_eq!(get_labels(&app).await, json!({ADDR: "keep me"}));
    // The same request without the foreign Origin goes through.
    let reply = delete(&app, ADDR, &[("origin", "http://127.0.0.1:8888")]).await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn delete_with_a_bad_identifier_is_400() {
    let rig = Setup::new().build();

    let reply = delete(&rig.app(), "nonsense", &[]).await;

    assert_error(
        &reply,
        StatusCode::BAD_REQUEST,
        &LabelError::BadDevice.to_string(),
    );
}

// ---------------------------------------------------------------------------
// CORS
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_preflight_from_a_listed_origin_allows_put_delete_and_content_type() {
    let rig = Setup::new().cors(&["https://ui.example"]).build();
    let uri = format!("/api/labels/{ADDR}");

    let reply = send(
        &rig.app(),
        "OPTIONS",
        &uri,
        &[
            ("origin", "https://ui.example"),
            ("access-control-request-method", "PUT"),
            ("access-control-request-headers", "content-type"),
        ],
        None,
    )
    .await;

    assert!(reply.status.is_success(), "{}", reply.status);
    assert_eq!(
        reply.header("access-control-allow-origin"),
        Some("https://ui.example")
    );
    let methods = reply
        .header("access-control-allow-methods")
        .unwrap()
        .to_ascii_uppercase();
    for method in ["GET", "PUT", "DELETE"] {
        assert!(methods.contains(method), "{methods}");
    }
    assert!(reply
        .header("access-control-allow-headers")
        .unwrap()
        .to_ascii_lowercase()
        .contains("content-type"));
}

#[tokio::test]
async fn a_preflight_from_another_origin_grants_nothing() {
    let rig = Setup::new().cors(&["https://ui.example"]).build();

    let reply = send(
        &rig.app(),
        "OPTIONS",
        &format!("/api/labels/{ADDR}"),
        &[
            ("origin", "https://evil.example"),
            ("access-control-request-method", "PUT"),
            ("access-control-request-headers", "content-type"),
        ],
        None,
    )
    .await;

    assert!(reply.header("access-control-allow-origin").is_none());
}

#[tokio::test]
async fn without_cors_origins_a_preflight_grants_nothing() {
    let rig = Setup::new().build();

    let reply = send(
        &rig.app(),
        "OPTIONS",
        &format!("/api/labels/{ADDR}"),
        &[
            ("origin", "https://ui.example"),
            ("access-control-request-method", "PUT"),
        ],
        None,
    )
    .await;

    assert!(reply.header("access-control-allow-origin").is_none());
    assert!(reply.header("access-control-allow-methods").is_none());
}

// ---------------------------------------------------------------------------
// Host check, through the whole router
// ---------------------------------------------------------------------------

const UPGRADE: [(&str, &str); 4] = [
    ("upgrade", "websocket"),
    ("connection", "upgrade"),
    ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
    ("sec-websocket-version", "13"),
];

fn assert_host_refusal(reply: &Reply, what: &str, host_name: &str) {
    assert_eq!(reply.status, StatusCode::FORBIDDEN, "{what}");
    assert_eq!(
        reply.header("content-type"),
        Some("text/plain; charset=utf-8"),
        "{what}"
    );
    assert_eq!(
        reply.header("x-content-type-options"),
        Some("nosniff"),
        "{what}"
    );
    let text = reply.text();
    assert!(text.contains(host_name), "{what}: {text}");
    assert!(text.contains("allowed_hosts"), "{what}: {text}");
}

#[tokio::test]
async fn a_foreign_host_is_refused_on_every_route() {
    let rig = Setup::new().build();
    let app = rig.app();
    rig.seen.note(ADDR);
    let host = ("host", "evil.example:8888");
    let body = label_body("x");
    let mut upgrade = UPGRADE.to_vec();
    upgrade.push(host);

    let cases = [
        ("/", "GET", vec![host], None),
        ("/index.html", "GET", vec![host], None),
        ("/api/messages", "GET", vec![host], None),
        ("/api/labels", "GET", vec![host], None),
        (
            "/api/labels/00E8A80B",
            "PUT",
            vec![host, JSON_BODY],
            Some(body.as_str()),
        ),
        ("/api/labels/00E8A80B", "DELETE", vec![host], None),
        ("/ws", "GET", upgrade.clone(), None),
        ("/ws/labels", "GET", upgrade, None),
    ];
    for (uri, method, headers, body) in cases {
        let reply = send(&app, method, uri, &headers, body).await;

        assert_host_refusal(&reply, &format!("{method} {uri}"), "evil.example");
    }
    // Nothing was written by the refused PUT, and the DELETE found nothing.
    assert_eq!(rig.storage.labels().unwrap().len(), 0);
}

#[tokio::test]
async fn a_dns_rebinding_request_is_refused() {
    let rig = Setup::new().build();
    let app = rig.app();
    rig.seen.note(ADDR);
    // The rebound page is same-origin as far as the browser can tell: its
    // Origin and the Host it sends name the same attacker-chosen host.
    let rebound = [
        ("host", "evil.example:8888"),
        ("origin", "http://evil.example:8888"),
    ];

    let mut put_headers = rebound.to_vec();
    put_headers.push(JSON_BODY);
    let write = send(
        &app,
        "PUT",
        &format!("/api/labels/{ADDR}"),
        &put_headers,
        Some(&label_body("pwned")),
    )
    .await;
    let read = send(&app, "GET", "/api/labels", &rebound, None).await;
    let mut upgrade = UPGRADE.to_vec();
    upgrade.extend_from_slice(&rebound);
    let feed = send(&app, "GET", "/ws/labels", &upgrade, None).await;

    assert_host_refusal(&write, "PUT", "evil.example");
    assert_host_refusal(&read, "GET labels", "evil.example");
    assert_host_refusal(&feed, "/ws/labels", "evil.example");
    assert_eq!(get_labels(&app).await, json!({}));
}

#[tokio::test]
async fn the_star_turns_the_host_check_off() {
    let rig = Setup::new().hosts(&["*"]).build();
    let app = rig.app();
    rig.seen.note(ADDR);
    let rebound = [
        ("host", "evil.example:8888"),
        ("origin", "http://evil.example:8888"),
    ];

    let read = send(&app, "GET", "/api/labels", &rebound, None).await;
    let mut headers = rebound.to_vec();
    headers.push(JSON_BODY);
    let write = send(
        &app,
        "PUT",
        &format!("/api/labels/{ADDR}"),
        &headers,
        Some(&label_body("allowed")),
    )
    .await;

    // This is the exposure the opt-out accepts: the Host check no longer
    // tells the rebound page from the sniffer's own.
    assert_eq!(read.status, StatusCode::OK);
    assert_eq!(write.status, StatusCode::OK);
}

#[tokio::test]
async fn listed_names_localhost_and_addresses_are_served() {
    let rig = Setup::new().hosts(&["Sniffer.Example.com"]).build();
    let app = rig.app();

    for host in [
        "sniffer.example.com",
        "sniffer.example.com:8888",
        "localhost:8888",
        "LOCALHOST",
        "192.168.1.20:8888",
        "[::1]:8888",
    ] {
        let reply = send(&app, "GET", "/api/labels", &[("host", host)], None).await;

        assert_eq!(reply.status, StatusCode::OK, "{host}");
    }
    let reply = send(
        &app,
        "GET",
        "/api/labels",
        &[("host", "other.example.com")],
        None,
    )
    .await;
    assert_host_refusal(&reply, "unlisted name", "other.example.com");
}

#[tokio::test]
async fn an_ip_host_with_a_listed_public_origin_can_write() {
    // Reverse-proxy set-up where the proxy sends an IP address as Host and the
    // browser's Origin is the public site.
    let rig = Setup::new().cors(&["https://sniffer.example.com"]).build();
    let app = rig.app();
    rig.seen.note(ADDR);

    let reply = put_raw(
        &app,
        ADDR,
        &label_body("via the proxy"),
        &[
            ("host", "10.0.0.5:8888"),
            ("origin", "https://sniffer.example.com"),
        ],
    )
    .await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(get_labels(&app).await, json!({ADDR: "via the proxy"}));
}

#[tokio::test]
async fn an_ip_host_with_an_unlisted_foreign_origin_is_refused() {
    let rig = Setup::new().cors(&["https://sniffer.example.com"]).build();
    let app = rig.app();
    rig.seen.note(ADDR);
    let foreign = [
        ("host", "10.0.0.5:8888"),
        ("origin", "https://evil.example"),
    ];

    let mut headers = foreign.to_vec();
    headers.push(JSON_BODY);
    let write = send(
        &app,
        "PUT",
        &format!("/api/labels/{ADDR}"),
        &headers,
        Some(&label_body("x")),
    )
    .await;
    let mut upgrade = UPGRADE.to_vec();
    upgrade.extend_from_slice(&foreign);
    let feed = send(&app, "GET", "/ws/labels", &upgrade, None).await;

    // The Host check passes; the Origin rule is what refuses these.
    assert_error(&write, StatusCode::FORBIDDEN, "cors_origins");
    assert_eq!(feed.status, StatusCode::FORBIDDEN);
    assert!(feed.body.is_empty());
    assert_eq!(get_labels(&app).await, json!({}));
}

// ---------------------------------------------------------------------------
// Change feed and writer, through the router
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_feed_carries_one_event_per_effective_change_and_none_for_a_no_op() {
    let rig = Setup::new().build();
    let app = rig.app();
    let mut events = rig.labels_tx.subscribe();
    rig.seen.note(ADDR);
    rig.seen.note(EUI);

    put(&app, ADDR, "one").await;
    put(&app, ADDR, "one").await; // same text: still a save
    put(&app, EUI, "two").await;
    delete(&app, ADDR, &[]).await;
    // None of these changes anything.
    delete(&app, ADDR, &[]).await;
    put(&app, ADDR, "").await;
    put(&app, "DEADBEEF", "unknown").await;
    put(&app, ADDR, &"a".repeat(65)).await;
    put_raw(&app, ADDR, "{", &[]).await;
    delete(&app, ADDR, &[("origin", "http://evil.example")]).await;

    assert_eq!(
        drain(&mut events),
        vec![
            event(ADDR, Some("one")),
            event(ADDR, Some("one")),
            event(EUI, Some("two")),
            event(ADDR, None),
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_puts_for_one_device_end_on_the_stored_label_for_every_client() {
    let rig = Setup::new().build();
    let app = rig.app();
    rig.seen.note(ADDR);
    let mut client_a = rig.labels_tx.subscribe();
    let mut client_b = rig.labels_tx.subscribe();

    let tasks: Vec<_> = (0..60)
        .map(|i| {
            let app = app.clone();
            tokio::spawn(async move { put(&app, ADDR, &format!("v{i}")).await.status })
        })
        .collect();
    for task in tasks {
        assert_eq!(task.await.unwrap(), StatusCode::OK);
    }

    let (a, b) = (drain(&mut client_a), drain(&mut client_b));
    assert_eq!(a.len(), 60);
    assert_eq!(a, b, "every client sees the same order");
    let stored = get_labels(&app).await;
    assert_eq!(
        a.last().unwrap()["data"]["label"],
        stored[ADDR],
        "the last event is the label the database kept"
    );
    let mut labels: Vec<String> = a
        .iter()
        .map(|e| e["data"]["label"].as_str().unwrap().to_string())
        .collect();
    labels.sort();
    let mut expected: Vec<String> = (0..60).map(|i| format!("v{i}")).collect();
    expected.sort();
    assert_eq!(labels, expected, "no change was lost or duplicated");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_puts_for_new_devices_with_one_slot_left_admit_exactly_one() {
    let rig = Setup::new().max_labels(3).build();
    let app = rig.app();
    for device in ["0000000A", "0000000B"] {
        rig.seen.note(device);
        assert_eq!(put(&app, device, "existing").await.status, StatusCode::OK);
    }
    let mut events = rig.labels_tx.subscribe();
    let newcomers: Vec<String> = (0..20).map(|i| format!("{:08X}", 0xA000 + i)).collect();
    for device in &newcomers {
        rig.seen.note(device);
    }

    let tasks: Vec<_> = newcomers
        .iter()
        .map(|device| {
            let (app, device) = (app.clone(), device.clone());
            tokio::spawn(async move { put(&app, &device, "newcomer").await.status })
        })
        .collect();
    let mut statuses = Vec::new();
    for task in tasks {
        statuses.push(task.await.unwrap());
    }

    let ok = statuses.iter().filter(|s| **s == StatusCode::OK).count();
    let conflicts = statuses
        .iter()
        .filter(|s| **s == StatusCode::CONFLICT)
        .count();
    assert_eq!((ok, conflicts), (1, 19), "{statuses:?}");
    assert_eq!(rig.storage.labels().unwrap().len(), 3);
    assert_eq!(drain(&mut events).len(), 1);
}

/// Wait until the writer has taken a command and is held at the gate.
async fn writer_is_held(rig: &Rig) {
    timeout(GUARD, rig.gate.as_ref().unwrap().entered.notified())
        .await
        .expect("the writer never reached the gate");
}

#[tokio::test]
async fn a_cancelled_put_is_still_saved_and_published_in_order() {
    let rig = Setup::new().gated().build();
    let app = rig.app();
    rig.seen.note(ADDR);
    rig.seen.note(EUI);
    let mut client_a = rig.labels_tx.subscribe();
    let mut client_b = rig.labels_tx.subscribe();

    let first = tokio::spawn({
        let app = app.clone();
        async move { put(&app, ADDR, "cancelled").await }
    });
    writer_is_held(&rig).await;
    // The browser gives up: the request task is dropped while the writer
    // holds its command.
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    assert!(rig.storage.labels().unwrap().is_empty(), "not saved yet");
    assert!(drain(&mut client_a).is_empty());

    // A later request queues behind the held one.
    let second = tokio::spawn({
        let app = app.clone();
        async move { put(&app, EUI, "second").await }
    });
    let gate = rig.gate.as_ref().unwrap();
    gate.release.add_permits(2);
    let second = second.await.unwrap();

    assert_eq!(second.status, StatusCode::OK);
    let expected = vec![event(ADDR, Some("cancelled")), event(EUI, Some("second"))];
    assert_eq!(drain(&mut client_a), expected);
    assert_eq!(drain(&mut client_b), expected);
    assert_eq!(
        rig.storage.labels().unwrap(),
        BTreeMap::from([
            (ADDR.to_string(), "cancelled".to_string()),
            (EUI.to_string(), "second".to_string()),
        ])
    );
}

#[tokio::test]
async fn a_cancelled_delete_is_still_saved_and_published() {
    let rig = Setup::new().gated().build();
    let app = rig.app();
    rig.storage
        .set_label(ADDR, "doomed", true, MAX_LABELS)
        .unwrap();
    let mut client = rig.labels_tx.subscribe();

    let request = tokio::spawn({
        let app = app.clone();
        async move { delete(&app, ADDR, &[]).await }
    });
    writer_is_held(&rig).await;
    request.abort();
    let _ = request.await;
    rig.gate.as_ref().unwrap().release.add_permits(1);
    // The next request cannot overtake the cancelled one.
    rig.gate.as_ref().unwrap().release.add_permits(1);
    rig.seen.note(EUI);
    assert_eq!(put(&app, EUI, "after").await.status, StatusCode::OK);

    assert_eq!(
        drain(&mut client),
        vec![event(ADDR, None), event(EUI, Some("after"))]
    );
    assert_eq!(
        rig.storage.labels().unwrap(),
        BTreeMap::from([(EUI.to_string(), "after".to_string())])
    );
}

/// Poll `condition` from the test task until it holds.  For state that another
/// task changes and that has no channel to wait on.
async fn eventually(what: &str, condition: impl Fn() -> bool) {
    timeout(GUARD, async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

#[tokio::test]
async fn a_device_seen_only_on_the_record_broadcast_can_be_labelled() {
    // The storage writer has received nothing, as when it drops records under
    // load: the database is empty.
    let rig = Setup::new().build();
    let app = rig.app();
    assert_eq!(
        put(&app, ADDR, "too early").await.status,
        StatusCode::NOT_FOUND
    );
    let listener = tokio::spawn(run_seen_listener(
        rig.broadcast_tx.subscribe(),
        rig.seen.clone(),
        rig.cancel.clone(),
    ));

    // The same envelopes the proxy broadcasts.
    for record in [data_record(ADDR), join_record(EUI)] {
        rig.broadcast_tx
            .send(make_ws_json(&record).unwrap())
            .unwrap();
    }
    eventually("the listener to note both devices", || rig.seen.len() == 2).await;

    assert_eq!(put(&app, ADDR, "heard live").await.status, StatusCode::OK);
    assert_eq!(put(&app, EUI, "joined live").await.status, StatusCode::OK);
    assert_eq!(rig.storage.count().unwrap(), 0, "nothing was stored");
    rig.cancel.cancel();
    timeout(GUARD, listener).await.unwrap().unwrap();
}

/// A valid 23-byte JoinRequest.
fn join_request_frame() -> Vec<u8> {
    let mut frame = vec![0x00];
    frame.extend_from_slice(&[0x01, 0, 0, 0, 0, 0, 0, 0]); // JoinEUI
    frame.extend_from_slice(&[0x08, 7, 6, 5, 4, 3, 2, 1]); // DevEUI
    frame.extend_from_slice(&[0x34, 0x12]); // DevNonce
    frame.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]); // MIC
    frame
}

#[tokio::test]
async fn a_device_seen_only_in_malformed_frames_cannot_be_labelled() {
    let rig = Setup::new().build();
    let app = rig.app();
    let listener = tokio::spawn(run_seen_listener(
        rig.broadcast_tx.subscribe(),
        rig.seen.clone(),
        rig.cancel.clone(),
    ));

    // Frames that read as a data frame from DevAddr 6874FA22 and as a join
    // request from a DevEUI, but cannot be LoRaWAN frames: the first failed
    // its CRC, the second is a join request three bytes short.
    let data = frame_of_noise_dev_addr();
    let join = join_request_frame();
    // The DevEUI the same bytes carry when they are a valid join request.
    let dev_eui = record_from_frame(&join, CrcStatus::Ok)
        .dev_eui
        .expect("a valid join request carries a DevEUI");
    let malformed = [
        record_from_frame(&data, CrcStatus::Fail),
        record_from_frame(&join[..20], CrcStatus::Ok),
    ];
    for record in &malformed {
        assert!(record.malformed.is_some(), "{record:?}");
        assert_eq!((&record.dev_addr, &record.dev_eui), (&None, &None));
    }

    // Each is stored and sent on the record broadcast, as the proxy does. A
    // valid record follows them: the listener handles the broadcast in order,
    // so once it has noted that one it has dealt with the malformed ones too.
    for record in &malformed {
        rig.storage.insert(record).unwrap();
        rig.broadcast_tx
            .send(make_ws_json(record).unwrap())
            .unwrap();
    }
    rig.broadcast_tx
        .send(make_ws_json(&data_record(ADDR)).unwrap())
        .unwrap();
    eventually("the listener to note the valid record", || {
        rig.seen.len() == 1
    })
    .await;

    for device in [NOISE_DEV_ADDR, dev_eui.as_str()] {
        let reply = put(&app, device, "never seen").await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{device}");
    }
    assert_eq!(rig.seen.len(), 1, "only the valid record was noted");
    assert!(rig.storage.labels().unwrap().is_empty());

    // Control: the same bytes received with a passed CRC, and the join request
    // at its full length, are devices that can be labelled.
    let valid = [
        record_from_frame(&data, CrcStatus::Ok),
        record_from_frame(&join, CrcStatus::Ok),
    ];
    for record in &valid {
        assert_eq!(record.malformed, None, "{record:?}");
        rig.storage.insert(record).unwrap();
        rig.broadcast_tx
            .send(make_ws_json(record).unwrap())
            .unwrap();
    }
    eventually("the listener to note both devices", || rig.seen.len() == 3).await;
    for device in [NOISE_DEV_ADDR, dev_eui.as_str()] {
        let reply = put(&app, device, "seen now").await;
        assert_eq!(reply.status, StatusCode::OK, "{device}");
    }

    rig.cancel.cancel();
    timeout(GUARD, listener).await.unwrap().unwrap();
}

// ---------------------------------------------------------------------------
// /ws/labels over a real socket
// ---------------------------------------------------------------------------

async fn serve(app: Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

/// A bare-bones WebSocket client: enough to open a connection, read the text
/// frames the server sends, and close.  Server frames are never masked.
struct WsClient {
    stream: TcpStream,
    buf: Vec<u8>,
}

impl WsClient {
    /// Open `path`; on a refused handshake return the response's status line.
    async fn connect(addr: SocketAddr, path: &str, origin: Option<&str>) -> Result<Self, String> {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let origin = origin
            .map(|o| format!("Origin: {o}\r\n"))
            .unwrap_or_default();
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: {addr}\r\nUpgrade: websocket\r\n\
             Connection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
             Sec-WebSocket-Version: 13\r\n{origin}\r\n"
        );
        stream.write_all(request.as_bytes()).await.unwrap();

        let mut client = WsClient {
            stream,
            buf: Vec::new(),
        };
        let end = loop {
            if let Some(i) = client.buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break i + 4;
            }
            assert!(
                client.fill().await,
                "connection closed during the handshake"
            );
        };
        let head = String::from_utf8_lossy(&client.buf[..end]).into_owned();
        client.buf.drain(..end);
        let status = head.lines().next().unwrap_or_default().to_string();
        if status.contains(" 101 ") {
            Ok(client)
        } else {
            Err(status)
        }
    }

    /// Read more bytes; `false` when the server closed the connection.
    async fn fill(&mut self) -> bool {
        let mut chunk = [0u8; 8192];
        let n = timeout(GUARD, self.stream.read(&mut chunk))
            .await
            .expect("timed out waiting for the server")
            .unwrap_or(0);
        self.buf.extend_from_slice(&chunk[..n]);
        n > 0
    }

    async fn need(&mut self, len: usize) -> bool {
        while self.buf.len() < len {
            if !self.fill().await {
                return false;
            }
        }
        true
    }

    /// The next text frame, or `None` once the server closed the socket.
    async fn text(&mut self) -> Option<String> {
        loop {
            if !self.need(2).await {
                return None;
            }
            let opcode = self.buf[0] & 0x0f;
            let mut len = (self.buf[1] & 0x7f) as usize;
            let mut offset = 2;
            if len == 126 {
                if !self.need(4).await {
                    return None;
                }
                len = u16::from_be_bytes([self.buf[2], self.buf[3]]) as usize;
                offset = 4;
            } else if len == 127 {
                if !self.need(10).await {
                    return None;
                }
                len = u64::from_be_bytes(self.buf[2..10].try_into().unwrap()) as usize;
                offset = 10;
            }
            if !self.need(offset + len).await {
                return None;
            }
            let payload = self.buf[offset..offset + len].to_vec();
            self.buf.drain(..offset + len);
            match opcode {
                0x1 => return Some(String::from_utf8(payload).unwrap()),
                0x8 => return None,
                _ => {} // ping, pong
            }
        }
    }

    async fn json(&mut self) -> Value {
        let text = self.text().await.expect("the server closed the socket");
        serde_json::from_str(&text).unwrap()
    }

    /// Say goodbye the way a browser does, with a masked close frame.
    async fn close(mut self) {
        self.stream
            .write_all(&[0x88, 0x80, 0, 0, 0, 0])
            .await
            .unwrap();
        // The server answers with its own close frame and hangs up.
        assert!(self.text().await.is_none());
    }
}

/// What a WebUI makes of the label feed: a snapshot replaces the map, an
/// event sets or removes one entry.
#[derive(Default)]
struct LabelView {
    map: BTreeMap<String, String>,
    snapshots: usize,
}

impl LabelView {
    fn apply(&mut self, frame: &Value) {
        match frame["type"].as_str().unwrap() {
            "labels" => {
                self.snapshots += 1;
                self.map = frame["data"]
                    .as_object()
                    .unwrap()
                    .iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
                    .collect();
            }
            "label" => {
                let device = frame["data"]["device"].as_str().unwrap().to_string();
                match frame["data"]["label"].as_str() {
                    Some(label) => self.map.insert(device, label.to_string()),
                    None => self.map.remove(&device),
                };
            }
            other => panic!("unexpected frame type {other:?}"),
        }
    }

    /// Apply frames until the map equals `want`.
    async fn follow(&mut self, client: &mut WsClient, want: &BTreeMap<String, String>) {
        for _ in 0..50 {
            if self.map == *want {
                return;
            }
            let frame = client.json().await;
            self.apply(&frame);
        }
        panic!(
            "the view never matched the database: {:?} vs {:?}",
            self.map, want
        );
    }
}

#[tokio::test]
async fn labels_ws_handshake_follows_the_origin_rule() {
    let rig = Setup::new().cors(&["https://ui.example"]).build();
    let addr = serve(rig.app()).await;

    let own = WsClient::connect(addr, "/ws/labels", Some(&format!("http://{addr}"))).await;
    let listed = WsClient::connect(addr, "/ws/labels", Some("https://ui.example")).await;
    let none = WsClient::connect(addr, "/ws/labels", None).await;
    let foreign = WsClient::connect(addr, "/ws/labels", Some("http://evil.example")).await;
    let sandboxed = WsClient::connect(addr, "/ws/labels", Some("null")).await;

    assert!(own.is_ok());
    assert!(listed.is_ok());
    assert!(none.is_ok());
    assert!(foreign.err().unwrap().contains(" 403 "));
    assert!(sandboxed.err().unwrap().contains(" 403 "));
}

#[tokio::test]
async fn labels_ws_sends_the_full_set_first_then_each_change() {
    let rig = Setup::new().build();
    let app = rig.app();
    let addr = serve(app.clone()).await;
    rig.storage
        .set_label(ADDR, "Havana", true, MAX_LABELS)
        .unwrap();
    rig.storage
        .set_label(EUI, "Havana join", true, MAX_LABELS)
        .unwrap();
    rig.seen.note(ADDR);
    rig.seen.note(EUI);
    let mut client = WsClient::connect(addr, "/ws/labels", None).await.unwrap();

    assert_eq!(
        client.json().await,
        json!({"type": "labels", "data": {ADDR: "Havana", EUI: "Havana join"}})
    );

    put(&app, ADDR, "Havana, renamed").await;
    assert_eq!(client.json().await, event(ADDR, Some("Havana, renamed")));
    put(&app, EUI, "").await;
    assert_eq!(client.json().await, event(EUI, None));
    delete(&app, ADDR, &[]).await;
    assert_eq!(client.json().await, event(ADDR, None));
    // A delete that removes nothing sends nothing: the next frame belongs to
    // the change after it.
    delete(&app, ADDR, &[]).await;
    put(&app, EUI, "back").await;
    assert_eq!(client.json().await, event(EUI, Some("back")));

    client.close().await;
}

#[tokio::test]
async fn labels_ws_sends_an_empty_set_when_there_are_no_labels() {
    let rig = Setup::new().build();
    let addr = serve(rig.app()).await;

    let mut client = WsClient::connect(addr, "/ws/labels", None).await.unwrap();

    assert_eq!(client.json().await, json!({"type": "labels", "data": {}}));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_puts_reach_two_clients_in_the_same_order() {
    let rig = Setup::new().build();
    let app = rig.app();
    let addr = serve(app.clone()).await;
    rig.seen.note(ADDR);
    let mut a = WsClient::connect(addr, "/ws/labels", None).await.unwrap();
    let mut b = WsClient::connect(addr, "/ws/labels", None).await.unwrap();
    assert_eq!(a.json().await["type"], json!("labels"));
    assert_eq!(b.json().await["type"], json!("labels"));

    let tasks: Vec<_> = (0..30)
        .map(|i| {
            let app = app.clone();
            tokio::spawn(async move { put(&app, ADDR, &format!("v{i}")).await.status })
        })
        .collect();
    for task in tasks {
        assert_eq!(task.await.unwrap(), StatusCode::OK);
    }

    let mut seen_a = Vec::new();
    let mut seen_b = Vec::new();
    for _ in 0..30 {
        seen_a.push(a.json().await);
        seen_b.push(b.json().await);
    }
    assert_eq!(seen_a, seen_b, "both clients see the same order");
    let stored = rig.storage.labels().unwrap();
    assert_eq!(
        seen_a.last().unwrap()["data"]["label"].as_str().unwrap(),
        stored[ADDR]
    );
}

/// The server's task for a connection can only run when the test awaits, so
/// the five changes below are all in the channel before it reads the first.
/// With room for two, it has missed three.
#[tokio::test]
async fn a_label_client_that_falls_behind_gets_the_full_set_again() {
    let rig = Setup::new().labels_capacity(2).build();
    let app = rig.app();
    let addr = serve(app.clone()).await;
    rig.seen.note("0000000F");
    let mut client = WsClient::connect(addr, "/ws/labels", None).await.unwrap();
    assert_eq!(client.json().await, json!({"type": "labels", "data": {}}));

    // No await between these: this test task holds the only thread.
    for i in 0..5 {
        let device = format!("0000000{i}");
        let label = format!("label {i}");
        rig.storage
            .set_label(&device, &label, true, MAX_LABELS)
            .unwrap();
        rig.labels_tx
            .send(make_label_json(&device, Some(&label)))
            .unwrap();
    }

    let resync = client.json().await;
    assert_eq!(resync["type"], json!("labels"));
    let database = rig.storage.labels().unwrap();
    assert_eq!(database.len(), 5);
    assert_eq!(resync["data"], json!(database));
    // The backlog the full set already covers is not replayed over it: the
    // next frame is the next real change.
    assert_eq!(put(&app, "0000000F", "next").await.status, StatusCode::OK);
    assert_eq!(client.json().await, event("0000000F", Some("next")));
}

#[tokio::test]
async fn a_prune_larger_than_the_channel_leaves_every_client_matching_the_database() {
    let rig = Setup::new().labels_capacity(2).build();
    let addr = serve(rig.app()).await;
    // One label keeps its record; six are orphans nobody has seen.
    rig.storage.insert(&data_record("0000000A")).unwrap();
    rig.storage
        .set_label("0000000A", "kept", true, MAX_LABELS)
        .unwrap();
    for i in 0..6 {
        rig.storage
            .set_label(&format!("1000000{i}"), "orphan", true, MAX_LABELS)
            .unwrap();
    }
    let mut views = Vec::new();
    for _ in 0..2 {
        let mut client = WsClient::connect(addr, "/ws/labels", None).await.unwrap();
        let mut view = LabelView::default();
        view.apply(&client.json().await);
        assert_eq!(view.map.len(), 7);
        views.push((client, view));
    }

    // The writer publishes the six removals in one go, with nothing in between
    // for the connection tasks to run on.
    let cutoff = Utc::now() - chrono::Duration::hours(1);
    let pruned = rig.writer.prune(cutoff, false).await.unwrap();
    assert_eq!(pruned.labels.len(), 6);

    let database = rig.storage.labels().unwrap();
    assert_eq!(database.len(), 1);
    for (client, view) in &mut views {
        view.follow(client, &database).await;
        assert_eq!(view.map, database);
        assert!(
            view.snapshots >= 2,
            "the client was sent the full set again"
        );
    }
}

#[tokio::test]
async fn the_message_feed_carries_no_label_frames() {
    let rig = Setup::new().build();
    let app = rig.app();
    let addr = serve(app.clone()).await;
    rig.seen.note(ADDR);
    let mut feed = WsClient::connect(addr, "/ws", None).await.unwrap();
    // The feed has no first frame to wait for; wait for its subscription.
    eventually("the feed to subscribe", || {
        rig.broadcast_tx.receiver_count() == 1
    })
    .await;

    put(&app, ADDR, "x").await;
    put(&app, ADDR, "").await;
    rig.broadcast_tx
        .send(make_ws_json(&data_record(ADDR)).unwrap())
        .unwrap();

    // The label changes came first and would have been read first.
    let frame = feed.json().await;
    assert_eq!(frame["type"], json!("message"));
    assert_eq!(frame["data"]["dev_addr"], json!(ADDR));
}

#[tokio::test]
async fn the_message_feed_handshake_still_follows_the_origin_rule() {
    let rig = Setup::new().build();
    let addr = serve(rig.app()).await;

    assert!(
        WsClient::connect(addr, "/ws", Some(&format!("http://{addr}")))
            .await
            .is_ok()
    );
    assert!(WsClient::connect(addr, "/ws", Some("http://evil.example"))
        .await
        .err()
        .unwrap()
        .contains(" 403 "));
}
