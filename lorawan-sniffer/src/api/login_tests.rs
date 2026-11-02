//! Tests of the WebUI login through the router: what each endpoint answers,
//! which paths need a credential, and the order the layers answer in.
//!
//! Every router here is built by [`Setup`]: without `login()` there is no
//! login, and with it the stored secret is [`LOGIN_SECRET`], so a test can
//! also make sessions by hand.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{HeaderValue, Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use ring::hmac;
use serde_json::{json, Value};
use tokio::task::JoinSet;
use tokio::time::Instant;
use tower::ServiceExt;

use super::auth::{Clock, Comparer, SESSION_COOKIE, SESSION_KEY_LABEL};
use super::label_tests::{serve, WsClient};
use super::tests::{
    record_with_new_fields, send, FailsAfter, Reply, Rig, Setup, DEFAULT_HOST, LOGIN_SECRET,
};
use super::{make_ws_json, Frontend};
use crate::core::labels::MAX_LABELS;
use crate::core::linelog::{start_capture, take_capture};
use crate::core::metadata::MessageRecord;
use crate::core::random::hex;
use crate::test_support::capture_logger;

/// A token of the shape the login asks for.
const TOKEN: &str = "0f1e2d3c4b5a69788796a5b4c3d2e1f0";

const JSON_BODY: (&str, &str) = ("content-type", "application/json");

/// What every 401 says.
const LOG_IN_FIRST: &str =
    "Log in to the sniffer first: open the WebUI, or send Authorization: Bearer <token>.";

fn token_body(token: &str) -> String {
    json!({ "token": token }).to_string()
}

async fn login_with(app: &Router, token: &str, extra: &[(&str, &str)]) -> Reply {
    let mut headers = vec![JSON_BODY];
    headers.extend_from_slice(extra);
    send(
        app,
        "POST",
        "/api/login",
        &headers,
        Some(&token_body(token)),
    )
    .await
}

async fn logout_with(app: &Router, extra: &[(&str, &str)]) -> Reply {
    let mut headers = vec![JSON_BODY];
    headers.extend_from_slice(extra);
    send(app, "POST", "/api/logout", &headers, Some("{}")).await
}

async fn get(app: &Router, uri: &str, extra: &[(&str, &str)]) -> Reply {
    send(app, "GET", uri, extra, None).await
}

fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

/// The router with the login on, for a test that needs nothing else.
fn rig() -> Rig {
    Setup::new().login(TOKEN).build()
}

/// The `Set-Cookie` of a reply, which must be there exactly once.
fn set_cookie(reply: &Reply) -> String {
    let all: Vec<_> = reply.headers.get_all("set-cookie").iter().collect();
    assert_eq!(all.len(), 1, "{:?}", reply.headers);
    all[0].to_str().unwrap().to_string()
}

fn assert_plain_401(reply: &Reply) {
    assert_plain_401_for("", reply);
}

/// [`assert_plain_401`], with the request named in the failure message.
fn assert_plain_401_for(what: &str, reply: &Reply) {
    assert_eq!(
        reply.status,
        StatusCode::UNAUTHORIZED,
        "{what}: {}",
        reply.text()
    );
    assert!(
        reply
            .header("content-type")
            .is_some_and(|t| t.starts_with("text/plain")),
        "{what}: {:?}",
        reply.header("content-type")
    );
    assert_eq!(reply.text(), LOG_IN_FIRST, "{what}");
    assert!(reply.headers.get("www-authenticate").is_none(), "{what}");
    assert!(reply.headers.get("location").is_none(), "{what}");
    assert!(reply.headers.get("set-cookie").is_none(), "{what}");
}

fn assert_json_error(reply: &Reply, status: StatusCode, error: &str) {
    assert_eq!(reply.status, status, "{}", reply.text());
    assert!(
        reply
            .header("content-type")
            .is_some_and(|t| t.starts_with("application/json")),
        "{:?}",
        reply.header("content-type")
    );
    assert_eq!(reply.json(), json!({ "error": error }));
}

/// What the login endpoints answer to a request that came through another
/// site, or through a proxy that changed `Host`.
const PROXY_MESSAGE: &str =
    "Login refused: the request came from another site, or a reverse proxy changed the Host header";

const ENDPOINTS: [&str; 2] = ["/api/login", "/api/logout"];

/// A second token, which the sniffer is not set up with.
const OTHER_TOKEN: &str = "ffeeddccbbaa99887766554433221100";

/// A request built by hand, for what [`send`] cannot say: header values that
/// are not text, and the address a listener would have attached to it.
async fn raw(
    app: &Router,
    method: &str,
    uri: &str,
    headers: &[(&str, &[u8])],
    body: Option<&str>,
    peer: Option<SocketAddr>,
) -> Reply {
    let mut request = Request::builder().method(method).uri(uri);
    if !headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("host"))
    {
        request = request.header("host", DEFAULT_HOST);
    }
    for (name, value) in headers {
        request = request.header(*name, HeaderValue::from_bytes(value).unwrap());
    }
    let mut request = request
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap();
    if let Some(peer) = peer {
        request.extensions_mut().insert(ConnectInfo(peer));
    }
    let response = app.clone().oneshot(request).await.unwrap();
    let (parts, body) = response.into_parts();
    Reply {
        status: parts.status,
        headers: parts.headers,
        body: body.collect().await.unwrap().to_bytes().to_vec(),
    }
}

/// The body that makes `path` accept a request: the right token for a login,
/// `{}` for a logout.
fn body_for(path: &str) -> String {
    if path == "/api/login" {
        token_body(TOKEN)
    } else {
        "{}".to_string()
    }
}

/// `POST path` as JSON, with the headers in `extra` added (header values as
/// bytes, so that a test can send what is not text).
async fn post_json(app: &Router, path: &str, extra: &[(&str, &[u8])]) -> Reply {
    let mut headers: Vec<(&str, &[u8])> = vec![("content-type", b"application/json")];
    headers.extend_from_slice(extra);
    raw(app, "POST", path, &headers, Some(&body_for(path)), None).await
}

/// The WARN lines (message text only) among `lines`.
fn warnings(lines: Vec<(log::Level, String)>) -> Vec<String> {
    lines
        .into_iter()
        .filter(|(level, _)| *level == log::Level::Warn)
        .map(|(_, line)| line)
        .collect()
}

fn assert_proxy_refusal(what: &str, reply: &Reply) {
    assert_eq!(
        reply.status,
        StatusCode::FORBIDDEN,
        "{what}: {}",
        reply.text()
    );
    assert!(
        reply
            .header("content-type")
            .is_some_and(|t| t.starts_with("application/json")),
        "{what}: {:?}",
        reply.header("content-type")
    );
    assert_eq!(reply.json(), json!({ "error": PROXY_MESSAGE }), "{what}");
    assert!(reply.headers.get("set-cookie").is_none(), "{what}");
}

/// A comparison that says yes to `right` only and counts how often it is
/// asked.
fn counting(counted: &Arc<AtomicUsize>, right: &'static str) -> Comparer {
    let counted = counted.clone();
    Box::new(move |candidate| {
        counted.fetch_add(1, Ordering::SeqCst);
        candidate == right
    })
}

// -- Data that a leak would show ------------------------------------------------

const RECORD_ID: &str = "leak-check-record-9d41";
const LABEL_TEXT: &str = "Leak check label 5c2e";
/// The DevAddr of [`record_with_new_fields`].
const DEVICE: &str = "01ABCDEF";

fn leak_record() -> MessageRecord {
    MessageRecord {
        id: RECORD_ID.to_string(),
        ..record_with_new_fields()
    }
}

/// Put the record and a label for its device into `rig`'s database.
fn seed(rig: Rig, record: &MessageRecord) -> Rig {
    rig.storage.insert(record).unwrap();
    rig.storage
        .set_label(DEVICE, LABEL_TEXT, true, MAX_LABELS)
        .unwrap();
    rig
}

/// A router with the login on and something worth stealing in its database.
fn seeded(record: &MessageRecord) -> Rig {
    seed(rig(), record)
}

/// The same without a login.
fn seeded_open(record: &MessageRecord) -> Rig {
    seed(Setup::new().build(), record)
}

/// The answer holds nothing from the database and is not JSON at all.
fn assert_no_data(what: &str, reply: &Reply) {
    let text = reply.text();
    for datum in [RECORD_ID, LABEL_TEXT, DEVICE, "dev_addr", "\"login\""] {
        assert!(
            !text.contains(datum),
            "{what}: the answer holds {datum:?}: {text}"
        );
    }
    assert!(
        serde_json::from_slice::<Value>(&reply.body).is_err(),
        "{what}: the answer is JSON: {text}"
    );
}

// -- Sessions made by hand ------------------------------------------------------

/// The key a session is signed with when the stored secret is `secret`.
///
/// Written out again here, on purpose: a test that builds sessions with the
/// sniffer's own function could not tell a refusal of a forgery from a refusal
/// of everything.  [`a_session_built_by_hand_with_the_stored_secret_is_accepted`]
/// shows that this derivation is the sniffer's.
fn derived_key(secret: &[u8], token: &str) -> hmac::Key {
    let derive = hmac::Key::new(hmac::HMAC_SHA256, secret);
    let mut context = hmac::Context::with_key(&derive);
    context.update(SESSION_KEY_LABEL);
    context.update(&[0]);
    context.update(token.as_bytes());
    hmac::Key::new(hmac::HMAC_SHA256, context.sign().as_ref())
}

/// A `Cookie` header value holding a well-formed session signed with `key`.
fn session_signed_with(key: &hmac::Key) -> String {
    let signed = format!(
        "v1.{}.00112233445566778899aabbccddeeff",
        chrono::Utc::now().timestamp()
    );
    let mac = hmac::sign(key, signed.as_bytes());
    format!("{SESSION_COOKIE}={signed}.{}", hex(mac.as_ref()))
}

/// What a request presents as proof it may use the API.
struct Presented {
    name: &'static str,
    headers: Vec<(&'static str, String)>,
}

impl Presented {
    fn new(name: &'static str, header: &'static str, value: String) -> Self {
        Presented {
            name,
            headers: vec![(header, value)],
        }
    }

    fn pairs(&self) -> Vec<(&str, &str)> {
        self.headers
            .iter()
            .map(|(name, value)| (*name, value.as_str()))
            .collect()
    }

    fn is_cookie(&self) -> bool {
        self.headers.iter().any(|(name, _)| *name == "cookie")
    }
}

/// Everything a request can present that must not open the API.  Needs a
/// running Tokio runtime: it builds routers to take their sessions.
fn bad_credentials() -> Vec<Presented> {
    let session = rig().session_cookie();
    let value = session
        .strip_prefix(&format!("{SESSION_COOKIE}="))
        .unwrap()
        .to_string();
    let (signed, mac) = value.rsplit_once('.').unwrap();
    // One hex digit of the MAC changed.
    let first = mac.chars().next().unwrap();
    let tampered = format!(
        "{SESSION_COOKIE}={signed}.{}{}",
        if first == '0' { '1' } else { '0' },
        &mac[1..]
    );
    // The time it was issued changed, the MAC kept.
    let mut parts: Vec<&str> = signed.split('.').collect();
    let later = (parts[1].parse::<u64>().unwrap() + 1).to_string();
    parts[1] = &later;
    let retimed = format!("{SESSION_COOKIE}={}.{mac}", parts.join("."));

    let cookie = |name, value| Presented::new(name, "cookie", value);
    vec![
        Presented {
            name: "no credential",
            headers: vec![],
        },
        Presented::new(
            "a wrong Bearer token",
            "authorization",
            bearer("not-the-token"),
        ),
        Presented::new(
            "a session value sent as a Bearer token",
            "authorization",
            bearer(&value),
        ),
        cookie(
            "a well-formed cookie nobody signed",
            format!(
                "{SESSION_COOKIE}=v1.1700000000.{}.{}",
                "ab".repeat(16),
                "cd".repeat(32)
            ),
        ),
        cookie("a cookie of garbage", format!("{SESSION_COOKIE}=garbage")),
        cookie(
            "the token as the cookie",
            format!("{SESSION_COOKIE}={TOKEN}"),
        ),
        cookie("a cookie with a MAC digit changed", tampered),
        cookie("a cookie with the issue time changed", retimed),
        cookie(
            "a cookie made with another token",
            Setup::new().login(OTHER_TOKEN).build().session_cookie(),
        ),
        cookie(
            "a cookie made with another secret",
            Setup::new()
                .login(TOKEN)
                .secret([7; 32])
                .build()
                .session_cookie(),
        ),
        cookie(
            "the right token, MAC keyed with the token itself",
            session_signed_with(&hmac::Key::new(hmac::HMAC_SHA256, TOKEN.as_bytes())),
        ),
        cookie(
            "the right token, derived from an all-zero secret",
            session_signed_with(&derived_key(&[0; 32], TOKEN)),
        ),
        cookie(
            "the right token, derived with the token as the secret",
            session_signed_with(&derived_key(TOKEN.as_bytes(), TOKEN)),
        ),
        cookie(
            "the right token, derived from an empty secret",
            session_signed_with(&derived_key(&[], TOKEN)),
        ),
    ]
}

/// One request that needs a credential.
struct Probe {
    method: &'static str,
    uri: &'static str,
    /// A WebSocket upgrade request.
    upgrade: bool,
    body: Option<&'static str>,
}

const UPGRADE: [(&str, &str); 4] = [
    ("upgrade", "websocket"),
    ("connection", "upgrade"),
    ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
    ("sec-websocket-version", "13"),
];

/// Every kind of path the login stands in front of.  The two writes would
/// change the label of the seeded device.
const PROBES: [Probe; 8] = [
    Probe {
        method: "GET",
        uri: "/ws",
        upgrade: true,
        body: None,
    },
    Probe {
        method: "GET",
        uri: "/ws/labels",
        upgrade: true,
        body: None,
    },
    Probe {
        method: "GET",
        uri: "/api/messages",
        upgrade: false,
        body: None,
    },
    Probe {
        method: "GET",
        uri: "/api/labels",
        upgrade: false,
        body: None,
    },
    Probe {
        method: "PUT",
        uri: "/api/labels/01ABCDEF",
        upgrade: false,
        body: Some(r#"{"label":"OVERWRITTEN"}"#),
    },
    Probe {
        method: "DELETE",
        uri: "/api/labels/01ABCDEF",
        upgrade: false,
        body: None,
    },
    Probe {
        method: "GET",
        uri: "/api/session",
        upgrade: false,
        body: None,
    },
    Probe {
        method: "GET",
        uri: "/api/nothing",
        upgrade: false,
        body: None,
    },
];

async fn probe(app: &Router, probe: &Probe, credential: &[(&str, &str)]) -> Reply {
    let mut headers: Vec<(&str, &str)> = Vec::new();
    if probe.upgrade {
        headers.extend_from_slice(&UPGRADE);
    }
    if probe.body.is_some() {
        headers.push(JSON_BODY);
    }
    headers.extend_from_slice(credential);
    send(app, probe.method, probe.uri, &headers, probe.body).await
}

// ---------------------------------------------------------------------------
// Login off
// ---------------------------------------------------------------------------

#[tokio::test]
async fn with_login_off_the_session_endpoint_says_so_and_sets_no_cookie() {
    let rig = Setup::new().build();

    let reply = get(&rig.app(), "/api/session", &[]).await;

    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply
        .header("content-type")
        .is_some_and(|t| t.starts_with("application/json")));
    assert_eq!(reply.json(), json!({ "login": false }));
    assert!(reply.headers.get("set-cookie").is_none());
}

#[tokio::test]
async fn with_login_off_nothing_asks_for_a_credential() {
    let rig = Setup::new().build();

    for uri in ["/api/messages", "/api/labels", "/api/nothing"] {
        let reply = get(&rig.app(), uri, &[]).await;
        assert_eq!(reply.status, StatusCode::OK, "{uri}");
    }
    // Login and logout are not routes: the page answers, as for any path.
    for uri in ["/api/login", "/api/logout"] {
        let reply = send(&rig.app(), "POST", uri, &[JSON_BODY], Some("{}")).await;
        assert_eq!(reply.status, StatusCode::OK, "{uri}");
        assert!(reply.headers.get("set-cookie").is_none(), "{uri}");
    }
}

#[tokio::test]
async fn with_login_off_login_and_logout_are_answered_like_any_unknown_path() {
    let rig = Setup::new().build();
    let unknown = send(
        &rig.app(),
        "POST",
        "/api/whatever",
        &[JSON_BODY],
        Some("{}"),
    )
    .await;
    assert_eq!(unknown.status, StatusCode::OK);

    for uri in ENDPOINTS {
        for method in ["POST", "GET", "DELETE"] {
            let reply = send(&rig.app(), method, uri, &[JSON_BODY], Some("{}")).await;
            assert_eq!(reply.status, unknown.status, "{method} {uri}");
            assert_eq!(
                reply.header("content-type"),
                unknown.header("content-type"),
                "{method} {uri}"
            );
            assert_eq!(reply.body, unknown.body, "{method} {uri}");
        }
    }
}

#[tokio::test]
async fn with_login_off_no_answer_carries_a_cookie_and_no_credential_changes_one() {
    let record = leak_record();
    let rig = seeded_open(&record);
    let bundle = bundle_asset();
    // A script that sends a token, or a browser left with a cookie from an
    // earlier run with the login on, must get the same answers as anyone.
    let stray: [&[(&str, &str)]; 3] = [
        &[],
        &[("authorization", "Bearer not-the-token")],
        &[("cookie", "lorawan_sniffer_session=garbage")],
    ];

    for probe_case in &PROBES {
        let mut previous: Option<Reply> = None;
        for credential in stray {
            let rig = seeded_open(&record);
            let reply = probe(&rig.app(), probe_case, credential).await;
            assert!(
                reply.headers.get("set-cookie").is_none(),
                "{} {}",
                probe_case.method,
                probe_case.uri
            );
            assert_ne!(reply.status, StatusCode::UNAUTHORIZED);
            assert_ne!(reply.status, StatusCode::TOO_MANY_REQUESTS);
            if let Some(first) = &previous {
                assert_eq!(reply.status, first.status, "{}", probe_case.uri);
                assert_eq!(reply.body, first.body, "{}", probe_case.uri);
            }
            previous = Some(reply);
        }
    }
    for uri in ["/", "/index.html", bundle.as_str()] {
        let reply = get(&rig.app(), uri, &[("authorization", "Bearer x")]).await;
        assert_eq!(reply.status, StatusCode::OK, "{uri}");
        assert!(reply.headers.get("set-cookie").is_none(), "{uri}");
    }
    for uri in ENDPOINTS {
        let reply = send(&rig.app(), "POST", uri, &[JSON_BODY], Some("{}")).await;
        assert!(reply.headers.get("set-cookie").is_none(), "{uri}");
    }
}

// ---------------------------------------------------------------------------
// Login on: protected and public paths
// ---------------------------------------------------------------------------

#[tokio::test]
async fn without_a_credential_the_api_answers_401_in_plain_text() {
    let rig = rig();

    for uri in [
        "/api/messages",
        "/api/labels",
        "/api/session",
        "/api/nothing",
    ] {
        assert_plain_401(&get(&rig.app(), uri, &[]).await);
    }
    // The 401 comes from the login layer, which is outside the route that
    // adds the storage failure count to its answers.
    let reply = get(&rig.app(), "/api/messages", &[]).await;
    assert!(reply.headers.get("x-storage-write-failures").is_none());
}

#[tokio::test]
async fn the_page_is_public() {
    let rig = rig();

    for uri in ["/", "/index.html"] {
        let reply = get(&rig.app(), uri, &[]).await;
        assert_eq!(reply.status, StatusCode::OK, "{uri}");
        assert!(reply.headers.get("set-cookie").is_none(), "{uri}");
    }
}

#[tokio::test]
async fn a_valid_session_cookie_gets_the_normal_answer() {
    let rig = rig();
    let cookie = rig.session_cookie();

    let messages = get(&rig.app(), "/api/messages", &[("cookie", &cookie)]).await;
    assert_eq!(messages.status, StatusCode::OK, "{}", messages.text());
    assert_eq!(messages.json(), json!([]));
    assert!(messages.headers.get("x-storage-write-failures").is_some());

    let session = get(&rig.app(), "/api/session", &[("cookie", &cookie)]).await;
    assert_eq!(session.status, StatusCode::OK);
    assert_eq!(session.json(), json!({ "login": true }));
}

#[tokio::test]
async fn the_right_bearer_token_gets_the_normal_answer_and_a_wrong_one_a_401() {
    let rig = rig();

    let right = get(
        &rig.app(),
        "/api/messages",
        &[("authorization", &bearer(TOKEN))],
    )
    .await;
    assert_eq!(right.status, StatusCode::OK, "{}", right.text());

    let wrong = get(
        &rig.app(),
        "/api/messages",
        &[("authorization", &bearer("not-the-token"))],
    )
    .await;
    assert_plain_401(&wrong);
}

#[tokio::test]
async fn a_forged_cookie_gets_a_401() {
    let rig = rig();
    let session = rig.session_cookie();
    // The last character of the MAC changed.
    let mut tampered = session.clone();
    let last = tampered.pop().unwrap();
    tampered.push(if last == '0' { '1' } else { '0' });

    for cookie in [tampered.as_str(), "lorawan_sniffer_session=garbage"] {
        assert_plain_401(&get(&rig.app(), "/api/messages", &[("cookie", cookie)]).await);
    }
}

#[tokio::test]
async fn an_authorization_header_of_another_scheme_does_not_hide_the_cookie() {
    // A reverse proxy that asks for its own password makes the browser send
    // this header with every request.
    let rig = rig();
    let cookie = rig.session_cookie();

    let reply = get(
        &rig.app(),
        "/api/messages",
        &[("authorization", "Basic dXNlcjpwYXNz"), ("cookie", &cookie)],
    )
    .await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    // Without the cookie the header is no credential.
    assert_plain_401(
        &get(
            &rig.app(),
            "/api/messages",
            &[("authorization", "Basic dXNlcjpwYXNz")],
        )
        .await,
    );
}

#[tokio::test]
async fn every_protected_path_refuses_what_is_not_a_credential() {
    let record = leak_record();
    let kept = BTreeMap::from([(DEVICE.to_string(), LABEL_TEXT.to_string())]);

    for presented in bad_credentials() {
        // A router of its own: wrong tokens count toward a limit.
        let rig = seeded(&record);
        let credential = presented.pairs();
        for case in &PROBES {
            let what = format!("{} {} with {}", case.method, case.uri, presented.name);

            let reply = probe(&rig.app(), case, &credential).await;

            assert_plain_401_for(&what, &reply);
            assert_no_data(&what, &reply);
        }
        // The refused writes changed nothing.
        assert_eq!(rig.storage.labels().unwrap(), kept, "{}", presented.name);
        assert_eq!(rig.storage.count().unwrap(), 1, "{}", presented.name);
    }
}

#[tokio::test]
async fn every_protected_path_gives_its_normal_answer_to_a_session_or_the_token() {
    let record = leak_record();

    for by_cookie in [true, false] {
        for case in &PROBES {
            let what = format!(
                "{} {} with {}",
                case.method,
                case.uri,
                if by_cookie { "a session" } else { "the token" }
            );
            // What the same request gets when there is no login at all.
            let reference = probe(&seeded_open(&record).app(), case, &[]).await;
            let rig = seeded(&record);
            let credential = if by_cookie {
                ("cookie", rig.session_cookie())
            } else {
                ("authorization", bearer(TOKEN))
            };

            let reply = probe(&rig.app(), case, &[(credential.0, &credential.1)]).await;

            assert!(reply.headers.get("set-cookie").is_none(), "{what}");
            if case.uri == "/api/session" {
                assert_eq!(reply.status, StatusCode::OK, "{what}");
                assert_eq!(reply.json(), json!({ "login": true }), "{what}");
                continue;
            }
            assert_eq!(reply.status, reference.status, "{what}");
            assert_eq!(
                reply.header("content-type"),
                reference.header("content-type"),
                "{what}"
            );
            assert_eq!(reply.body, reference.body, "{what}: {}", reply.text());
            // A request that is not an upgrade the server can complete
            // in-process still got past the login and to the handler.
            assert!(
                ![
                    StatusCode::UNAUTHORIZED,
                    StatusCode::FORBIDDEN,
                    StatusCode::TOO_MANY_REQUESTS
                ]
                .contains(&reply.status),
                "{what}: {}",
                reply.status
            );
            match case.uri {
                "/api/messages" => assert!(reply.text().contains(RECORD_ID), "{what}"),
                "/api/labels" => assert!(reply.text().contains(LABEL_TEXT), "{what}"),
                _ => {}
            }
        }
    }
}

#[tokio::test]
async fn a_session_built_by_hand_with_the_stored_secret_is_accepted() {
    // The forgeries above are only as good as this derivation: with the right
    // secret and token it must be a session, and with another token not.
    let rig = rig();

    let made = session_signed_with(&derived_key(&LOGIN_SECRET, TOKEN));
    let reply = get(&rig.app(), "/api/messages", &[("cookie", &made)]).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());

    let other = session_signed_with(&derived_key(&LOGIN_SECRET, OTHER_TOKEN));
    let reply = get(&rig.app(), "/api/messages", &[("cookie", &other)]).await;
    assert_plain_401(&reply);
}

/// Spellings of paths that might slip past a prefix test, or past the router's
/// own matching.
const ODD_PATHS: [&str; 23] = [
    "/api",
    "/api/",
    "//api/messages",
    "/api//messages",
    "/%61pi/messages",
    "/api/%2e%2e/ws",
    "/API/messages",
    "/ws/",
    "/ws//labels",
    "/ws/labels/",
    "/%77s",
    "/ws%2Flabels",
    "/api/messages/",
    "/api/%6dessages",
    "/api/./messages",
    "/api/messages%20",
    "/api/messages?from=0",
    "/api/labels/",
    "/api/labels//01ABCDEF",
    "/api/session/",
    "/api/..",
    "/api/../api/messages",
    "/index.html/../api/messages",
];

#[tokio::test]
async fn an_odd_spelling_of_a_path_reaches_no_data_without_a_credential() {
    let record = leak_record();
    let rig = seeded(&record);
    let index = Frontend::get("index.html")
        .expect("the WebUI is embedded")
        .data;

    for path in ODD_PATHS {
        let reply = get(&rig.app(), path, &[]).await;

        assert_no_data(path, &reply);
        match reply.status {
            StatusCode::UNAUTHORIZED => assert_plain_401_for(path, &reply),
            StatusCode::OK => {
                // Answered by the page itself, as any unknown path is.
                assert!(
                    reply
                        .header("content-type")
                        .is_some_and(|t| t.starts_with("text/html")),
                    "{path}: {:?}",
                    reply.header("content-type")
                );
                assert_eq!(reply.body, index.as_ref(), "{path}");
            }
            other => panic!("{path}: {other}: {}", reply.text()),
        }
    }
}

/// The path of a script of the embedded WebUI.
fn bundle_asset() -> String {
    let name = Frontend::iter()
        .find(|name| name.starts_with("assets/") && name.ends_with(".js"))
        .expect("the WebUI bundle is embedded");
    format!("/{name}")
}

#[tokio::test]
async fn the_page_and_its_bundle_are_served_without_a_credential_and_without_counting() {
    let compared = Arc::new(AtomicUsize::new(0));
    let rig = Setup::new()
        .login(TOKEN)
        .comparer(counting(&compared, TOKEN))
        .build();
    let bundle = bundle_asset();
    let script = Frontend::get(bundle.trim_start_matches('/')).unwrap().data;

    let reply = get(&rig.app(), &bundle, &[]).await;

    assert_eq!(reply.status, StatusCode::OK);
    assert!(
        reply
            .header("content-type")
            .is_some_and(|t| t.contains("javascript")),
        "{:?}",
        reply.header("content-type")
    );
    assert_eq!(reply.body, script.as_ref());
    assert!(reply.headers.get("set-cookie").is_none());
    // A token sent to a public path is not looked at, so it cannot lock
    // anyone out either.
    for uri in ["/", "/index.html", bundle.as_str()] {
        for _ in 0..15 {
            let reply = get(&rig.app(), uri, &[("authorization", "Bearer wrong")]).await;
            assert_eq!(reply.status, StatusCode::OK, "{uri}");
        }
    }
    assert_eq!(compared.load(Ordering::SeqCst), 0);
    let reply = get(
        &rig.app(),
        "/api/messages",
        &[("authorization", &bearer(TOKEN))],
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
}

// ---------------------------------------------------------------------------
// Login and logout
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_right_token_starts_a_session_that_the_api_accepts() {
    let rig = rig();

    let reply = login_with(&rig.app(), TOKEN, &[]).await;

    assert_eq!(reply.status, StatusCode::NO_CONTENT, "{}", reply.text());
    assert!(reply.body.is_empty());
    let cookie = set_cookie(&reply);
    let (pair, attributes) = cookie.split_once("; ").unwrap();
    assert_eq!(attributes, "HttpOnly; SameSite=Strict; Path=/");
    let value = pair.strip_prefix("lorawan_sniffer_session=").unwrap();
    let parts: Vec<&str> = value.split('.').collect();
    assert_eq!(parts.len(), 4, "{value}");
    assert_eq!(parts[0], "v1");
    assert!(parts[1].bytes().all(|b| b.is_ascii_digit()));
    assert_eq!((parts[2].len(), parts[3].len()), (32, 64));

    let messages = get(&rig.app(), "/api/messages", &[("cookie", pair)]).await;
    assert_eq!(messages.status, StatusCode::OK, "{}", messages.text());
    let session = get(&rig.app(), "/api/session", &[("cookie", pair)]).await;
    assert_eq!(session.json(), json!({ "login": true }));
}

#[tokio::test]
async fn a_wrong_token_is_refused_with_no_cookie() {
    let rig = rig();

    let reply = login_with(&rig.app(), "not-the-token", &[]).await;

    assert_json_error(&reply, StatusCode::UNAUTHORIZED, "Wrong token");
    assert!(reply.headers.get("set-cookie").is_none());
}

#[tokio::test]
async fn a_body_that_is_not_a_token_is_a_400_and_a_large_one_a_413() {
    let rig = rig();

    for body in [
        "",
        "not json",
        "{}",
        "[]",
        "\"token\"",
        r#"{"token": 5}"#,
        r#"{"token": null}"#,
        r#"{"Token": "x"}"#,
    ] {
        let reply = send(&rig.app(), "POST", "/api/login", &[JSON_BODY], Some(body)).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{body:?}");
        assert!(reply.headers.get("set-cookie").is_none(), "{body:?}");
        assert!(reply.json()["error"].is_string(), "{body:?}");
    }

    let large = token_body(&"a".repeat(5 * 1024));
    let reply = send(&rig.app(), "POST", "/api/login", &[JSON_BODY], Some(&large)).await;
    assert_json_error(
        &reply,
        StatusCode::PAYLOAD_TOO_LARGE,
        "The request is too large",
    );
    assert!(reply.headers.get("set-cookie").is_none());
}

#[tokio::test]
async fn logout_expires_the_cookie() {
    let rig = rig();

    let reply = logout_with(&rig.app(), &[]).await;

    assert_eq!(reply.status, StatusCode::NO_CONTENT, "{}", reply.text());
    assert_eq!(
        set_cookie(&reply),
        "lorawan_sniffer_session=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0"
    );
}

#[tokio::test]
async fn a_failing_random_source_is_a_500_and_an_error_line_and_no_cookie() {
    // Setting the login up takes one fill; the session needs another.
    let rig = Setup::new().login(TOKEN).random(FailsAfter::new(1)).build();
    start_capture();

    let reply = login_with(&rig.app(), TOKEN, &[]).await;

    assert_json_error(
        &reply,
        StatusCode::INTERNAL_SERVER_ERROR,
        "The sniffer could not start a session; see its log",
    );
    assert!(reply.headers.get("set-cookie").is_none());
    let lines = take_capture();
    let errors: Vec<_> = lines
        .iter()
        .filter(|(level, _)| *level == log::Level::Error)
        .collect();
    assert_eq!(errors.len(), 1, "{lines:?}");
    assert!(errors[0].1.contains("random"), "{}", errors[0].1);
    assert!(!errors[0].1.contains(TOKEN));
}

/// A JSON login body of exactly `len` bytes whose token is wrong.
fn login_body_of(len: usize) -> String {
    let frame = token_body("");
    format!(
        "{}{}{}",
        &frame[..frame.len() - 2],
        "a".repeat(len - frame.len()),
        &frame[frame.len() - 2..]
    )
}

#[tokio::test]
async fn the_login_reads_4_kib_and_refuses_one_byte_more() {
    let compared = Arc::new(AtomicUsize::new(0));
    let rig = Setup::new()
        .login(TOKEN)
        .comparer(counting(&compared, TOKEN))
        .build();
    let post = |body: String| {
        let app = rig.app();
        async move { send(&app, "POST", "/api/login", &[JSON_BODY], Some(&body)).await }
    };

    let exact = post(login_body_of(4096)).await;
    assert_json_error(&exact, StatusCode::UNAUTHORIZED, "Wrong token");
    assert_eq!(compared.load(Ordering::SeqCst), 1);

    let over = post(login_body_of(4097)).await;
    assert_json_error(
        &over,
        StatusCode::PAYLOAD_TOO_LARGE,
        "The request is too large",
    );
    assert!(over.headers.get("set-cookie").is_none());
    // A body that large is never read as a token, so it is not compared.
    assert_eq!(compared.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_request_refused_before_its_token_is_read_is_not_counted_as_a_wrong_token() {
    let compared = Arc::new(AtomicUsize::new(0));
    let rig = Setup::new()
        .login(TOKEN)
        .comparer(counting(&compared, TOKEN))
        .build();

    for _ in 0..4 {
        // Not JSON, from another site, not a token, too large.
        let reply = send(
            &rig.app(),
            "POST",
            "/api/login",
            &[("content-type", "text/plain")],
            Some(&token_body("wrong")),
        )
        .await;
        assert_eq!(reply.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        let reply = login_with(&rig.app(), "wrong", &[("origin", "https://evil.example")]).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN);
        let reply = send(&rig.app(), "POST", "/api/login", &[JSON_BODY], Some("{}")).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        let reply = send(
            &rig.app(),
            "POST",
            "/api/login",
            &[JSON_BODY],
            Some(&login_body_of(5000)),
        )
        .await;
        assert_eq!(reply.status, StatusCode::PAYLOAD_TOO_LARGE);
    }
    assert_eq!(compared.load(Ordering::SeqCst), 0);

    // All ten wrong tokens are still available.
    for _ in 0..10 {
        let reply = login_with(&rig.app(), "wrong", &[]).await;
        assert_json_error(&reply, StatusCode::UNAUTHORIZED, "Wrong token");
    }
    let reply = login_with(&rig.app(), "wrong", &[]).await;
    assert_eq!(reply.status, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn logout_needs_no_credential_and_no_random_bytes() {
    // Setting the login up takes the only fill this source serves.
    let rig = Setup::new().login(TOKEN).random(FailsAfter::new(1)).build();

    let reply = logout_with(&rig.app(), &[]).await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT, "{}", reply.text());
    assert_eq!(
        set_cookie(&reply),
        "lorawan_sniffer_session=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0"
    );
    // The same router still cannot start a session.
    let reply = login_with(&rig.app(), TOKEN, &[]).await;
    assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(reply.headers.get("set-cookie").is_none());
}

#[tokio::test]
async fn a_failed_session_is_not_counted_as_a_wrong_token() {
    let compared = Arc::new(AtomicUsize::new(0));
    let rig = Setup::new()
        .login(TOKEN)
        .random(FailsAfter::new(1))
        .comparer(counting(&compared, TOKEN))
        .build();

    // More attempts than the limit allows wrong tokens.
    for _ in 0..12 {
        let reply = login_with(&rig.app(), TOKEN, &[]).await;
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(reply.headers.get("set-cookie").is_none());
    }
    assert_eq!(compared.load(Ordering::SeqCst), 12);
}

// ---------------------------------------------------------------------------
// Login and logout: JSON only, same host only
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_login_that_is_not_json_is_refused_before_the_token_is_looked_at() {
    let rig = rig();

    // What a form or a `no-cors` fetch can send, with the right token.
    for content_type in [
        "text/plain",
        "text/plain;charset=UTF-8",
        "application/x-www-form-urlencoded",
        "multipart/form-data; boundary=x",
        "application/jsonp",
        "application/vnd.x+json",
        "application/json+x",
        "application/json; charset",
        "application/json; =x",
        "application/json; charset=",
    ] {
        for path in ["/api/login", "/api/logout"] {
            let reply = send(
                &rig.app(),
                "POST",
                path,
                &[
                    ("content-type", content_type),
                    ("origin", "http://127.0.0.1:8888"),
                ],
                Some(&token_body(TOKEN)),
            )
            .await;
            assert_json_error(&reply, StatusCode::UNSUPPORTED_MEDIA_TYPE, "Send JSON");
            assert!(
                reply.headers.get("set-cookie").is_none(),
                "{path}: {content_type}"
            );
        }
    }

    // No `Content-Type`, and two.
    let reply = send(
        &rig.app(),
        "POST",
        "/api/login",
        &[],
        Some(&token_body(TOKEN)),
    )
    .await;
    assert_eq!(reply.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    for pair in [
        ["application/json", "application/json"],
        ["application/json", "text/plain"],
    ] {
        let reply = send(
            &rig.app(),
            "POST",
            "/api/login",
            &[("content-type", pair[0]), ("content-type", pair[1])],
            Some(&token_body(TOKEN)),
        )
        .await;
        assert_eq!(reply.status, StatusCode::UNSUPPORTED_MEDIA_TYPE, "{pair:?}");
        assert!(reply.headers.get("set-cookie").is_none());
    }
}

#[tokio::test]
async fn json_in_any_case_and_with_parameters_is_accepted() {
    let rig = rig();

    for content_type in [
        "application/json",
        "Application/JSON",
        "application/json; charset=utf-8",
        "application/json;charset=UTF-8",
        "application/json; charset=\"utf-8\"",
    ] {
        let reply = send(
            &rig.app(),
            "POST",
            "/api/login",
            &[("content-type", content_type)],
            Some(&token_body(TOKEN)),
        )
        .await;
        assert_eq!(reply.status, StatusCode::NO_CONTENT, "{content_type}");
    }
}

#[tokio::test]
async fn an_origin_other_than_the_host_is_refused_with_no_cookie() {
    let rig = rig();

    for origin in [
        "http://evil.example",
        "https://evil.example",
        "http://127.0.0.1:9999",
        "null",
    ] {
        let reply = login_with(&rig.app(), TOKEN, &[("origin", origin)]).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{origin}");
        assert!(reply.headers.get("set-cookie").is_none(), "{origin}");
        assert!(reply.json()["error"]
            .as_str()
            .unwrap()
            .contains("another site, or a reverse proxy changed the Host header"));
        let reply = logout_with(&rig.app(), &[("origin", origin)]).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{origin}");
        assert!(reply.headers.get("set-cookie").is_none(), "{origin}");
    }
}

/// The one WARN line that refusing `origin` logs, on a router of its own so
/// that the interval between two lines never hides it.
async fn refusal_line(origin: &[u8]) -> String {
    let login = rig();
    start_capture();
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/api/login")
        .header("host", "127.0.0.1:8888")
        .header("content-type", "application/json")
        .header(
            "origin",
            axum::http::HeaderValue::from_bytes(origin).unwrap(),
        )
        .body(axum::body::Body::from(token_body(TOKEN)))
        .unwrap();
    let response = tower::ServiceExt::oneshot(login.app(), request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let mut lines = take_capture();
    assert_eq!(lines.len(), 1, "{lines:?}");
    let (level, line) = lines.remove(0);
    assert_eq!(level, log::Level::Warn);
    line
}

#[tokio::test]
async fn a_refused_origin_is_logged_with_both_hosts() {
    assert_eq!(
        refusal_line(b"https://sniffer.example").await,
        "WebUI login refused: Origin host sniffer.example does not match Host 127.0.0.1:8888; \
         a reverse proxy must pass the browser's Host header"
    );
}

#[tokio::test]
async fn a_second_refusal_within_the_interval_is_counted_and_not_logged() {
    let login = rig();
    start_capture();

    for _ in 0..3 {
        login_with(
            &login.app(),
            TOKEN,
            &[("origin", "https://sniffer.example")],
        )
        .await;
    }

    assert_eq!(take_capture().len(), 1);
}

#[tokio::test]
async fn what_the_client_wrote_is_cut_and_made_printable_before_it_is_logged() {
    let long = format!("https://{}", "a".repeat(10_000));
    let line = refusal_line(long.as_bytes()).await;
    assert!(
        line.contains(&format!("Origin host {}… does not", "a".repeat(64))),
        "{line}"
    );

    // Bytes that are not UTF-8, and a tab, each shown as `?`.
    let line = refusal_line(b"http://caf\xc3\xa9\t.example").await;
    assert!(line.contains("Origin host caf???.example does"), "{line}");
}

#[tokio::test]
async fn an_origin_with_the_hosts_name_and_port_is_accepted_whatever_its_scheme() {
    let rig = rig();

    for origin in ["http://127.0.0.1:8888", "https://127.0.0.1:8888"] {
        let reply = login_with(&rig.app(), TOKEN, &[("origin", origin)]).await;
        assert_eq!(reply.status, StatusCode::NO_CONTENT, "{origin}");
    }
}

#[tokio::test]
async fn a_listed_cors_origin_is_still_no_reason_to_accept_a_login() {
    let rig = Setup::new()
        .login(TOKEN)
        .cors(&["https://ui.example"])
        .build();

    let reply = login_with(&rig.app(), TOKEN, &[("origin", "https://ui.example")]).await;

    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert!(reply.headers.get("set-cookie").is_none());
}

#[tokio::test]
async fn the_host_check_answers_before_the_login() {
    let rig = rig();

    let reply = get(&rig.app(), "/api/messages", &[("host", "evil.example")]).await;

    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert!(reply.text().contains("evil.example"), "{}", reply.text());
    let reply = send(
        &rig.app(),
        "POST",
        "/api/login",
        &[JSON_BODY, ("host", "evil.example")],
        Some(&token_body(TOKEN)),
    )
    .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert!(reply.headers.get("set-cookie").is_none());
}

#[tokio::test]
async fn a_cors_preflight_needs_no_credential_and_lists_authorization() {
    let rig = Setup::new()
        .login(TOKEN)
        .cors(&["https://ui.example"])
        .build();

    let reply = send(
        &rig.app(),
        "OPTIONS",
        "/api/messages",
        &[
            ("origin", "https://ui.example"),
            ("access-control-request-method", "GET"),
            ("access-control-request-headers", "authorization"),
        ],
        None,
    )
    .await;

    assert!(reply.status.is_success(), "{}", reply.status);
    assert!(reply
        .header("access-control-allow-headers")
        .unwrap()
        .to_ascii_lowercase()
        .contains("authorization"));
}

/// Every way a request can fail to declare exactly one `application/json`
/// `Content-Type`, as the values of that header.
fn content_types_that_are_not_json() -> Vec<(&'static str, Vec<&'static [u8]>)> {
    vec![
        (
            "urlencoded",
            vec![b"application/x-www-form-urlencoded" as &[u8]],
        ),
        ("multipart", vec![b"multipart/form-data; boundary=x"]),
        ("text/plain", vec![b"text/plain"]),
        (
            "text/plain with a charset",
            vec![b"text/plain;charset=UTF-8"],
        ),
        ("none", vec![]),
        ("jsonp", vec![b"application/jsonp"]),
        ("a +json type", vec![b"application/vnd.x+json"]),
        ("json with a suffix", vec![b"application/json+x"]),
        (
            "a parameter without a value",
            vec![b"application/json; charset"],
        ),
        ("a parameter without a name", vec![b"application/json; =x"]),
        (
            "an empty parameter value",
            vec![b"application/json; charset="],
        ),
        ("bytes outside visible ASCII", vec![b"application/json\xff"]),
        ("JSON twice", vec![b"application/json", b"application/json"]),
        (
            "JSON and text/plain",
            vec![b"application/json", b"text/plain"],
        ),
        (
            "text/plain and JSON",
            vec![b"text/plain", b"application/json"],
        ),
    ]
}

#[tokio::test]
async fn only_one_json_content_type_is_accepted_by_login_and_logout_alike() {
    let rig = rig();
    let session = rig.session_cookie();

    for (name, values) in content_types_that_are_not_json() {
        for path in ENDPOINTS {
            // The most favourable request there can be otherwise: the
            // browser's own origin, the right token, a session to end.
            let mut headers: Vec<(&str, &[u8])> = vec![
                ("origin", b"http://127.0.0.1:8888"),
                ("cookie", session.as_bytes()),
            ];
            headers.extend(values.iter().map(|value| ("content-type", *value)));
            let reply = raw(
                &rig.app(),
                "POST",
                path,
                &headers,
                Some(&body_for(path)),
                None,
            )
            .await;

            assert_json_error(&reply, StatusCode::UNSUPPORTED_MEDIA_TYPE, "Send JSON");
            // Neither a session started nor the one sent ended.
            assert!(reply.headers.get("set-cookie").is_none(), "{path}: {name}");
        }
    }
}

#[tokio::test]
async fn the_content_type_is_judged_before_the_origin() {
    let rig = rig();

    for path in ENDPOINTS {
        let reply = raw(
            &rig.app(),
            "POST",
            path,
            &[
                ("content-type", b"text/plain"),
                ("origin", b"https://evil.example"),
            ],
            Some(&body_for(path)),
            None,
        )
        .await;

        assert_json_error(&reply, StatusCode::UNSUPPORTED_MEDIA_TYPE, "Send JSON");
    }
}

#[tokio::test]
async fn logout_accepts_the_same_json_spellings_as_login() {
    let rig = rig();

    for content_type in [
        "application/json",
        "Application/JSON",
        "APPLICATION/JSON; CHARSET=UTF-8",
        "application/json; charset=utf-8",
    ] {
        let reply = send(
            &rig.app(),
            "POST",
            "/api/logout",
            &[("content-type", content_type)],
            Some("{}"),
        )
        .await;

        assert_eq!(reply.status, StatusCode::NO_CONTENT, "{content_type}");
        assert_eq!(
            set_cookie(&reply),
            "lorawan_sniffer_session=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0"
        );
    }
}

#[tokio::test]
async fn the_same_host_and_port_is_accepted_on_either_scheme_by_login_and_logout() {
    let rig = Setup::new().login(TOKEN).hosts(&["h"]).build();

    for (host, origin) in [
        ("127.0.0.1:8888", "http://127.0.0.1:8888"),
        ("127.0.0.1:8888", "https://127.0.0.1:8888"),
        // Browsers leave the default port out of both, so these are equal.
        ("h", "http://h"),
        ("h", "https://h"),
        ("H", "https://h"),
        ("h:8080", "https://h:8080"),
        ("[::1]:8888", "http://[::1]:8888"),
    ] {
        for path in ENDPOINTS {
            let reply = post_json(
                &rig.app(),
                path,
                &[("host", host.as_bytes()), ("origin", origin.as_bytes())],
            )
            .await;

            assert_eq!(
                reply.status,
                StatusCode::NO_CONTENT,
                "{path}: Host {host}, Origin {origin}: {}",
                reply.text()
            );
            assert!(reply.headers.get("set-cookie").is_some());
        }
    }
}

#[tokio::test]
async fn an_origin_that_is_not_the_host_is_refused_by_logout_and_leaves_the_cookie() {
    // `https://ui.example` is listed: it may read the API, not log in or out.
    let rig = Setup::new()
        .login(TOKEN)
        .cors(&["https://ui.example", "http://127.0.0.1:9999"])
        .build();
    let session = rig.session_cookie();

    for origin in [
        "https://ui.example",
        "http://127.0.0.1:9999",
        "http://127.0.0.1",
        "https://127.0.0.1",
        "http://localhost:8888",
        "http://evil.example",
        "null",
        "file://",
    ] {
        for path in ENDPOINTS {
            let reply = post_json(
                &rig.app(),
                path,
                &[
                    ("origin", origin.as_bytes()),
                    ("cookie", session.as_bytes()),
                ],
            )
            .await;

            assert_proxy_refusal(&format!("{path} from {origin}"), &reply);
        }
    }
}

// ---------------------------------------------------------------------------
// A reverse proxy that changes Host
// ---------------------------------------------------------------------------

/// What a login or logout request gets from a router set up as `setup`, and
/// the WARN lines it logged.  Each case builds its own router: the line about
/// a refused `Origin` is limited to one per interval for each.
async fn refused_with(setup: Setup, path: &str, headers: &[(&str, &[u8])]) -> (Reply, Vec<String>) {
    let rig = setup.build();
    start_capture();
    let reply = post_json(&rig.app(), path, headers).await;
    (reply, warnings(take_capture()))
}

#[tokio::test]
async fn a_proxy_that_changes_host_is_told_so_and_the_log_names_both_hosts() {
    for path in ENDPOINTS {
        let (reply, warns) = refused_with(
            Setup::new().login(TOKEN),
            path,
            &[
                ("origin", b"https://sniffer.example"),
                ("host", b"127.0.0.1:8888"),
            ],
        )
        .await;

        assert_proxy_refusal(path, &reply);
        assert_eq!(warns.len(), 1, "{path}: {warns:?}");
        assert!(
            warns[0].contains("Origin host sniffer.example "),
            "{}",
            warns[0]
        );
        assert!(warns[0].contains("Host 127.0.0.1:8888;"), "{}", warns[0]);
    }
}

#[tokio::test]
async fn forwarding_headers_change_nothing_about_which_host_counts() {
    let forwarded: [(&str, &[u8]); 6] = [
        ("x-forwarded-host", b"sniffer.example"),
        ("x-forwarded-proto", b"https"),
        ("x-forwarded-port", b"443"),
        ("x-forwarded-for", b"203.0.113.9"),
        ("forwarded", b"host=sniffer.example;proto=https"),
        ("x-real-ip", b"203.0.113.9"),
    ];
    for path in ENDPOINTS {
        let direct: Vec<(&str, &[u8])> = vec![
            ("origin", b"https://sniffer.example"),
            ("host", b"127.0.0.1:8888"),
        ];
        let mut proxied = direct.clone();
        proxied.extend_from_slice(&forwarded);

        let (plain, plain_warns) = refused_with(Setup::new().login(TOKEN), path, &direct).await;
        let (reply, warns) = refused_with(Setup::new().login(TOKEN), path, &proxied).await;

        // Naming the browser's host in `X-Forwarded-Host` does not make the
        // request the browser's own.
        assert_proxy_refusal(path, &plain);
        assert_proxy_refusal(path, &reply);
        assert_eq!(warns, plain_warns, "{path}");

        // And naming another host there does not make it foreign.
        let rig = rig();
        let own = post_json(
            &rig.app(),
            path,
            &[
                ("origin", b"http://127.0.0.1:8888"),
                ("x-forwarded-host", b"evil.example"),
                ("forwarded", b"host=evil.example"),
            ],
        )
        .await;
        assert_eq!(own.status, StatusCode::NO_CONTENT, "{path}");
    }
}

#[tokio::test]
async fn a_host_the_host_check_refuses_never_reaches_the_login() {
    for path in ENDPOINTS {
        let (reply, warns) = refused_with(
            Setup::new().login(TOKEN),
            path,
            &[
                ("origin", b"https://sniffer.example"),
                ("host", b"backend.internal"),
            ],
        )
        .await;

        // The Host check's own answer: plain text that says what to add to
        // `allowed_hosts`, and nothing about a proxy.
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{path}");
        assert!(
            reply
                .header("content-type")
                .is_some_and(|t| t.starts_with("text/plain")),
            "{path}: {:?}",
            reply.header("content-type")
        );
        let text = reply.text();
        assert!(text.contains("backend.internal"), "{text}");
        assert!(text.contains("allowed_hosts"), "{text}");
        assert!(!text.contains("reverse proxy"), "{text}");
        assert_ne!(text, PROXY_MESSAGE);
        assert!(reply.headers.get("set-cookie").is_none());
        assert!(warns.is_empty(), "{path}: {warns:?}");
    }
}

#[tokio::test]
async fn a_listed_host_that_is_not_the_browsers_gets_the_proxy_message() {
    for path in ENDPOINTS {
        let (reply, warns) = refused_with(
            Setup::new().login(TOKEN).hosts(&["backend.internal"]),
            path,
            &[
                ("origin", b"https://sniffer.example"),
                ("host", b"backend.internal"),
            ],
        )
        .await;

        assert_proxy_refusal(path, &reply);
        assert_eq!(warns.len(), 1, "{path}: {warns:?}");
        assert!(warns[0].contains("sniffer.example"), "{}", warns[0]);
        assert!(warns[0].contains("Host backend.internal;"), "{}", warns[0]);
    }
}

#[tokio::test]
async fn a_proxy_that_adds_the_default_port_gets_the_proxy_message() {
    for path in ENDPOINTS {
        let (reply, warns) = refused_with(
            Setup::new().login(TOKEN).hosts(&["sniffer.example"]),
            path,
            &[
                ("origin", b"https://sniffer.example"),
                ("host", b"sniffer.example:443"),
            ],
        )
        .await;

        assert_proxy_refusal(path, &reply);
        assert_eq!(warns.len(), 1, "{path}: {warns:?}");
        assert!(
            warns[0].contains("Host sniffer.example:443;"),
            "{}",
            warns[0]
        );

        // The same proxy, passing the browser's Host as it was.
        let rig = Setup::new()
            .login(TOKEN)
            .hosts(&["sniffer.example"])
            .build();
        let reply = post_json(
            &rig.app(),
            path,
            &[
                ("origin", b"https://sniffer.example"),
                ("host", b"sniffer.example"),
            ],
        )
        .await;
        assert_eq!(reply.status, StatusCode::NO_CONTENT, "{path}");
    }
}

#[tokio::test]
async fn what_a_refused_origin_or_host_adds_to_the_log_is_bounded() {
    let long_origin = format!("https://{}", "a".repeat(10_000));
    let no_scheme = "x".repeat(10_000);
    let long_host = "b".repeat(10_000);
    for path in ENDPOINTS {
        let (_, warns) = refused_with(
            Setup::new().login(TOKEN),
            path,
            &[("origin", long_origin.as_bytes())],
        )
        .await;
        assert_eq!(warns.len(), 1, "{path}: {warns:?}");
        assert!(
            warns[0].contains(&format!("Origin host {}… does not", "a".repeat(64))),
            "{}",
            warns[0]
        );
        assert!(warns[0].len() < 300, "{}", warns[0].len());

        // A value with no scheme is shown whole, and cut the same way.
        let (_, warns) = refused_with(
            Setup::new().login(TOKEN),
            path,
            &[("origin", no_scheme.as_bytes())],
        )
        .await;
        assert!(
            warns[0].contains(&format!("Origin host {}… does not", "x".repeat(64))),
            "{}",
            warns[0]
        );
        assert!(warns[0].len() < 300, "{}", warns[0].len());

        // The Host is client text too, when the Host check is off.
        let (_, warns) = refused_with(
            Setup::new().login(TOKEN).hosts(&["*"]),
            path,
            &[
                ("origin", b"https://sniffer.example"),
                ("host", long_host.as_bytes()),
            ],
        )
        .await;
        assert!(
            warns[0].contains(&format!("Host {}…;", "b".repeat(64))),
            "{}",
            warns[0]
        );
        assert!(warns[0].len() < 300, "{}", warns[0].len());
    }
}

#[tokio::test]
async fn bytes_outside_printable_ascii_are_shown_as_question_marks_in_the_log() {
    for path in ENDPOINTS {
        let (_, warns) = refused_with(
            Setup::new().login(TOKEN),
            path,
            &[("origin", b"https://sniffer\x80\xff\t.example")],
        )
        .await;
        assert!(
            warns[0].contains("Origin host sniffer???.example does"),
            "{}",
            warns[0]
        );

        let (_, warns) = refused_with(
            Setup::new().login(TOKEN).hosts(&["*"]),
            path,
            &[
                ("origin", b"https://sniffer.example"),
                ("host", b"caf\xc3\xa9.example"),
            ],
        )
        .await;
        assert!(warns[0].contains("Host caf??.example;"), "{}", warns[0]);

        // Cut by bytes, not characters: a letter of two bytes counts as two.
        let accents = format!("https://{}", "é".repeat(100));
        let (_, warns) = refused_with(
            Setup::new().login(TOKEN),
            path,
            &[("origin", accents.as_bytes())],
        )
        .await;
        assert!(
            warns[0].contains(&format!("Origin host {}… does", "?".repeat(64))),
            "{}",
            warns[0]
        );
    }
}

#[tokio::test]
async fn a_foreign_host_gets_the_host_checks_403_whatever_it_presents_and_nothing_is_compared() {
    let compared = Arc::new(AtomicUsize::new(0));
    let record = leak_record();
    let rig = seed(
        Setup::new()
            .login(TOKEN)
            .comparer(counting(&compared, TOKEN))
            .build(),
        &record,
    );
    let session = rig.session_cookie();
    let credentials: [&[(&str, &str)]; 4] = [
        &[],
        &[("authorization", "Bearer wrong")],
        &[("authorization", "Bearer 0f1e2d3c4b5a69788796a5b4c3d2e1f0")],
        &[("cookie", session.as_str())],
    ];

    for credential in credentials {
        for case in &PROBES {
            let mut headers = credential.to_vec();
            headers.push(("host", "evil.example"));
            let reply = probe(&rig.app(), case, &headers).await;

            let what = format!("{} {}", case.method, case.uri);
            assert_eq!(reply.status, StatusCode::FORBIDDEN, "{what}");
            assert!(reply.text().contains("allowed_hosts"), "{what}");
            assert_no_data(&what, &reply);
        }
        for path in ENDPOINTS {
            let reply = post_json(
                &rig.app(),
                path,
                &[
                    ("host", b"evil.example"),
                    ("authorization", b"Bearer wrong"),
                ],
            )
            .await;
            assert_eq!(reply.status, StatusCode::FORBIDDEN, "{path}");
            assert!(reply.text().contains("allowed_hosts"), "{path}");
        }
    }
    assert_eq!(compared.load(Ordering::SeqCst), 0);
}

// ---------------------------------------------------------------------------
// CORS preflights
// ---------------------------------------------------------------------------

fn preflight_headers<'a>(
    origin: &'a str,
    method: &'a str,
    headers: &'a str,
) -> [(&'a str, &'a str); 3] {
    [
        ("origin", origin),
        ("access-control-request-method", method),
        ("access-control-request-headers", headers),
    ]
}

#[tokio::test]
async fn with_login_off_a_preflight_allows_the_same_headers_as_before_there_was_a_login() {
    let rig = Setup::new().cors(&["https://ui.example"]).build();

    let reply = send(
        &rig.app(),
        "OPTIONS",
        "/api/messages",
        &preflight_headers("https://ui.example", "GET", "content-type"),
        None,
    )
    .await;

    assert!(reply.status.is_success(), "{}", reply.status);
    assert_eq!(
        reply
            .header("access-control-allow-headers")
            .map(|h| h.to_ascii_lowercase()),
        Some("content-type".to_string())
    );
}

#[tokio::test]
async fn a_listed_origin_may_ask_to_send_authorization_to_every_data_route() {
    let rig = Setup::new()
        .login(TOKEN)
        .cors(&["https://ui.example"])
        .build();

    for (method, uri) in [
        ("GET", "/api/messages"),
        ("GET", "/api/labels"),
        ("PUT", "/api/labels/01ABCDEF"),
        ("DELETE", "/api/labels/01ABCDEF"),
    ] {
        // No credential: a preflight never carries one.
        let reply = send(
            &rig.app(),
            "OPTIONS",
            uri,
            &preflight_headers("https://ui.example", method, "authorization, content-type"),
            None,
        )
        .await;

        assert!(
            reply.status.is_success(),
            "{method} {uri}: {}",
            reply.status
        );
        assert_eq!(
            reply.header("access-control-allow-origin"),
            Some("https://ui.example"),
            "{method} {uri}"
        );
        let methods = reply.header("access-control-allow-methods").unwrap();
        assert!(methods.contains(method), "{method} {uri}: {methods}");
        let allowed = reply
            .header("access-control-allow-headers")
            .unwrap()
            .to_ascii_lowercase();
        assert!(allowed.contains("authorization"), "{allowed}");
        assert!(allowed.contains("content-type"), "{allowed}");
    }
    // An origin that is not listed gets no permission.
    let reply = send(
        &rig.app(),
        "OPTIONS",
        "/api/messages",
        &preflight_headers("https://evil.example", "GET", "authorization"),
        None,
    )
    .await;
    assert!(reply.header("access-control-allow-origin").is_none());
}

#[tokio::test]
async fn a_preflight_for_login_or_logout_does_not_allow_post() {
    let rig = Setup::new()
        .login(TOKEN)
        .cors(&["https://ui.example"])
        .build();

    for path in ENDPOINTS {
        let reply = send(
            &rig.app(),
            "OPTIONS",
            path,
            &preflight_headers("https://ui.example", "POST", "content-type"),
            None,
        )
        .await;

        // The CORS layer answers for the listed origin, but its list of
        // methods has no POST, so a browser will not send the request.
        assert!(reply.status.is_success(), "{path}: {}", reply.status);
        let methods = reply
            .header("access-control-allow-methods")
            .expect("the layer lists the methods it allows");
        assert!(
            !methods.to_ascii_uppercase().contains("POST"),
            "{path}: {methods}"
        );
        assert!(reply.headers.get("set-cookie").is_none());
    }
}

#[tokio::test]
async fn without_cors_origins_a_preflight_for_login_or_logout_gets_a_405_and_no_cors_headers() {
    let rig = rig();

    for path in ENDPOINTS {
        let reply = send(
            &rig.app(),
            "OPTIONS",
            path,
            &preflight_headers("https://ui.example", "POST", "content-type"),
            None,
        )
        .await;

        assert_eq!(reply.status, StatusCode::METHOD_NOT_ALLOWED, "{path}");
        assert_eq!(reply.header("allow"), Some("POST"), "{path}");
        for (name, _) in reply.headers.iter() {
            assert!(
                !name.as_str().starts_with("access-control-"),
                "{path}: {name}"
            );
        }
        assert!(reply.headers.get("set-cookie").is_none());
    }
}

// ---------------------------------------------------------------------------
// The secret of the test router
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_session_made_with_another_secret_is_refused() {
    let other = Setup::new().login(TOKEN).secret([7; 32]).build();
    assert_ne!(LOGIN_SECRET, [7; 32]);
    let foreign = other.session_cookie();

    let reply = get(&rig().app(), "/api/messages", &[("cookie", &foreign)]).await;

    assert_plain_401(&reply);
}

// ---------------------------------------------------------------------------
// The guessing limit, as the router applies it
// ---------------------------------------------------------------------------

/// A clock the test moves.
struct ManualClock {
    base: Instant,
    offset: Mutex<Duration>,
}

impl ManualClock {
    fn new() -> Arc<Self> {
        Arc::new(ManualClock {
            base: Instant::now(),
            offset: Mutex::new(Duration::ZERO),
        })
    }

    fn advance(&self, by: Duration) {
        *self.offset.lock().unwrap() += by;
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Instant {
        self.base + *self.offset.lock().unwrap()
    }
}

#[tokio::test]
async fn ten_wrong_tokens_lock_every_token_out_until_the_window_ends() {
    // The comparison says no to everything and counts how often it is asked.
    let compared = Arc::new(AtomicUsize::new(0));
    let counter = compared.clone();
    let clock = ManualClock::new();
    let rig = Setup::new()
        .login(TOKEN)
        .clock(clock.clone())
        .comparer(Box::new(move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
            false
        }))
        .build();
    let wrong = [("authorization", "Bearer wrong")];

    for _ in 0..10 {
        assert_plain_401(&get(&rig.app(), "/api/messages", &wrong).await);
    }
    assert_eq!(compared.load(Ordering::SeqCst), 10);

    // The eleventh is not even compared; neither is a login.
    let locked = get(&rig.app(), "/api/messages", &wrong).await;
    assert_eq!(locked.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(locked
        .header("content-type")
        .is_some_and(|t| t.starts_with("text/plain")));
    assert_eq!(locked.header("retry-after"), Some("60"));
    assert_eq!(locked.text(), "Too many wrong tokens; try again in 60 s");
    let login = login_with(&rig.app(), TOKEN, &[]).await;
    assert_json_error(
        &login,
        StatusCode::TOO_MANY_REQUESTS,
        "Too many wrong tokens; try again in 60 s",
    );
    assert_eq!(login.header("retry-after"), Some("60"));
    assert!(login.headers.get("set-cookie").is_none());
    assert_eq!(compared.load(Ordering::SeqCst), 10);

    // A session cookie is not a token: it still gets in, and a request with
    // no credential still gets its 401.
    let cookie = rig.session_cookie();
    let inside = get(&rig.app(), "/api/messages", &[("cookie", &cookie)]).await;
    assert_eq!(inside.status, StatusCode::OK);
    assert_plain_401(&get(&rig.app(), "/api/messages", &[]).await);

    // Once the window is over, tokens are compared again.
    clock.advance(Duration::from_secs(61));
    assert_plain_401(&get(&rig.app(), "/api/messages", &wrong).await);
    assert_eq!(compared.load(Ordering::SeqCst), 11);
}

/// How many of `lines` announce a lockout.
fn lockout_announcements(lines: Vec<(log::Level, String)>) -> usize {
    warnings(lines)
        .iter()
        .filter(|line| line.contains("WebUI login locked for"))
        .count()
}

fn assert_locked(what: &str, reply: &Reply, seconds: u64) {
    assert_eq!(reply.status, StatusCode::TOO_MANY_REQUESTS, "{what}");
    assert!(
        reply
            .header("content-type")
            .is_some_and(|t| t.starts_with("text/plain")),
        "{what}: {:?}",
        reply.header("content-type")
    );
    let retry = seconds.to_string();
    assert_eq!(reply.header("retry-after"), Some(retry.as_str()), "{what}");
    assert_eq!(
        reply.text(),
        format!("Too many wrong tokens; try again in {seconds} s"),
        "{what}"
    );
    // This answer comes from the login layer, outside the messages route.
    assert!(reply.headers.get("x-storage-write-failures").is_none());
    assert!(reply.headers.get("set-cookie").is_none(), "{what}");
}

fn assert_login_locked(what: &str, reply: &Reply, seconds: u64) {
    let retry = seconds.to_string();
    assert_json_error(
        reply,
        StatusCode::TOO_MANY_REQUESTS,
        &format!("Too many wrong tokens; try again in {seconds} s"),
    );
    assert_eq!(reply.header("retry-after"), Some(retry.as_str()), "{what}");
    assert!(reply.headers.get("set-cookie").is_none(), "{what}");
}

#[tokio::test(start_paused = true)]
async fn the_lockout_follows_the_paused_tokio_clock() {
    let compared = Arc::new(AtomicUsize::new(0));
    let rig = Setup::new()
        .login(TOKEN)
        .comparer(counting(&compared, TOKEN))
        .build();
    let wrong = [("authorization", "Bearer wrong")];
    let right_token = bearer(TOKEN);
    let right = [("authorization", right_token.as_str())];
    start_capture();

    for n in 1..=10 {
        assert_plain_401(&get(&rig.app(), "/api/messages", &wrong).await);
        assert_eq!(compared.load(Ordering::SeqCst), n);
    }
    assert_eq!(lockout_announcements(take_capture()), 0);

    // The eleventh token, and the right one, are refused unexamined.
    assert_locked("wrong", &get(&rig.app(), "/api/messages", &wrong).await, 60);
    assert_locked("right", &get(&rig.app(), "/api/messages", &right).await, 60);
    for _ in 0..3 {
        assert_locked("again", &get(&rig.app(), "/api/messages", &wrong).await, 60);
    }
    // A login is a token too: wrong or right, it gets the JSON answer.
    assert_login_locked("login", &login_with(&rig.app(), "wrong", &[]).await, 60);
    assert_login_locked("login", &login_with(&rig.app(), TOKEN, &[]).await, 60);
    assert_eq!(compared.load(Ordering::SeqCst), 10);
    assert_eq!(lockout_announcements(take_capture()), 1);

    // A session cookie is no token: it still reaches the data, and a request
    // with no credential still gets its 401, not a wait.
    let cookie = rig.session_cookie();
    let inside = get(&rig.app(), "/api/messages", &[("cookie", &cookie)]).await;
    assert_eq!(inside.status, StatusCode::OK);
    assert_plain_401(&get(&rig.app(), "/api/messages", &[]).await);
    assert_eq!(compared.load(Ordering::SeqCst), 10);

    // 59 s on, a second is left of the window.
    tokio::time::advance(Duration::from_secs(59)).await;
    assert_locked("late", &get(&rig.app(), "/api/messages", &right).await, 1);
    assert_eq!(compared.load(Ordering::SeqCst), 10);
    assert_eq!(lockout_announcements(take_capture()), 0);

    // At 60 s tokens are compared again, and the right one gets in.
    tokio::time::advance(Duration::from_secs(1)).await;
    let reply = get(&rig.app(), "/api/messages", &right).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(compared.load(Ordering::SeqCst), 11);
    // A new window has its own ten, and its own line.
    for _ in 0..10 {
        assert_plain_401(&get(&rig.app(), "/api/messages", &wrong).await);
    }
    assert_locked(
        "second window",
        &get(&rig.app(), "/api/messages", &wrong).await,
        60,
    );
    assert_eq!(lockout_announcements(take_capture()), 1);
    assert_eq!(compared.load(Ordering::SeqCst), 21);
}

#[tokio::test(start_paused = true)]
async fn forged_cookies_are_neither_counted_nor_let_through() {
    let compared = Arc::new(AtomicUsize::new(0));
    let record = leak_record();
    let rig = seed(
        Setup::new()
            .login(TOKEN)
            .comparer(counting(&compared, TOKEN))
            .build(),
        &record,
    );
    let forged: Vec<Presented> = bad_credentials()
        .into_iter()
        .filter(Presented::is_cookie)
        .collect();
    // More than the ten wrong tokens a window allows.
    assert!(forged.len() * 2 >= 20, "{}", forged.len());

    for presented in forged.iter().chain(&forged) {
        let reply = get(&rig.app(), "/api/messages", &presented.pairs()).await;
        assert_plain_401_for(presented.name, &reply);
        assert_no_data(presented.name, &reply);
    }
    assert_eq!(compared.load(Ordering::SeqCst), 0);

    // All ten wrong tokens are still available...
    for n in 1..=10 {
        let reply = get(
            &rig.app(),
            "/api/messages",
            &[("authorization", "Bearer wrong")],
        )
        .await;
        assert_plain_401(&reply);
        assert_eq!(compared.load(Ordering::SeqCst), n);
    }
    let reply = get(
        &rig.app(),
        "/api/messages",
        &[("authorization", "Bearer wrong")],
    )
    .await;
    assert_eq!(reply.status, StatusCode::TOO_MANY_REQUESTS);
    // ...and once they are used, a forgery is still a 401, not a wait, and
    // still reaches nothing.
    for presented in &forged {
        let reply = get(&rig.app(), "/api/messages", &presented.pairs()).await;
        assert_plain_401_for(presented.name, &reply);
        assert_no_data(presented.name, &reply);
    }
    assert_eq!(compared.load(Ordering::SeqCst), 10);
}

#[tokio::test(start_paused = true)]
async fn logins_and_bearer_calls_share_one_limit_and_right_tokens_never_count_against_it() {
    let compared = Arc::new(AtomicUsize::new(0));
    let rig = Setup::new()
        .login(TOKEN)
        .comparer(counting(&compared, TOKEN))
        .build();
    let right = bearer(TOKEN);

    for _ in 0..12 {
        let reply = login_with(&rig.app(), TOKEN, &[]).await;
        assert_eq!(reply.status, StatusCode::NO_CONTENT);
        let reply = get(&rig.app(), "/api/labels", &[("authorization", &right)]).await;
        assert_eq!(reply.status, StatusCode::OK);
    }
    assert_eq!(compared.load(Ordering::SeqCst), 24);

    // Five wrong logins and five wrong Bearer calls fill the window together.
    for _ in 0..5 {
        let reply = login_with(&rig.app(), "wrong", &[]).await;
        assert_json_error(&reply, StatusCode::UNAUTHORIZED, "Wrong token");
        assert_plain_401(
            &get(
                &rig.app(),
                "/api/labels",
                &[("authorization", "Bearer wrong")],
            )
            .await,
        );
    }
    assert_login_locked("login", &login_with(&rig.app(), "wrong", &[]).await, 60);
    assert_login_locked("right login", &login_with(&rig.app(), TOKEN, &[]).await, 60);
    assert_locked(
        "bearer",
        &get(&rig.app(), "/api/labels", &[("authorization", &right)]).await,
        60,
    );
    assert_eq!(compared.load(Ordering::SeqCst), 34);

    // Logging out takes no token, so it works during a lockout.
    assert_eq!(
        logout_with(&rig.app(), &[]).await.status,
        StatusCode::NO_CONTENT
    );
    tokio::time::advance(Duration::from_secs(60)).await;
    assert_eq!(
        login_with(&rig.app(), TOKEN, &[]).await.status,
        StatusCode::NO_CONTENT
    );
}

// ---------------------------------------------------------------------------
// The guessing limit under concurrency
// ---------------------------------------------------------------------------

/// A comparison that takes two milliseconds, to widen any gap between looking
/// at the count and adding to it.  It sleeps the thread it runs on and waits
/// for nothing else, so it cannot hold up the test itself.
fn slow_counting(counted: &Arc<AtomicUsize>, right: &'static str) -> Comparer {
    let counted = counted.clone();
    Box::new(move |candidate| {
        counted.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(2));
        candidate == right
    })
}

/// Start `count` requests at the same moment, and collect the answers.  A
/// Tokio barrier holds them: unlike a thread barrier it waits without taking
/// up one of the runtime's threads.
async fn together<F, Fut>(count: usize, request: F) -> Vec<Reply>
where
    F: Fn(usize) -> Fut,
    Fut: std::future::Future<Output = Reply> + Send + 'static,
{
    let barrier = Arc::new(tokio::sync::Barrier::new(count));
    let mut tasks = JoinSet::new();
    for n in 0..count {
        let barrier = barrier.clone();
        let request = request(n);
        tasks.spawn(async move {
            barrier.wait().await;
            request.await
        });
    }
    let mut replies = Vec::new();
    while let Some(reply) = tasks.join_next().await {
        replies.push(reply.unwrap());
    }
    replies
}

/// How many answers there were of each status.
fn tally(replies: &[Reply]) -> BTreeMap<u16, usize> {
    let mut counts = BTreeMap::new();
    for reply in replies {
        *counts.entry(reply.status.as_u16()).or_insert(0) += 1;
    }
    counts
}

const WITHIN: Duration = Duration::from_secs(10);

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn sixty_four_wrong_bearer_tokens_together_get_ten_comparisons_and_fifty_four_refusals() {
    let compared = Arc::new(AtomicUsize::new(0));
    let rig = Setup::new()
        .login(TOKEN)
        .comparer(slow_counting(&compared, TOKEN))
        .build();
    let app = rig.app();

    let replies = tokio::time::timeout(
        WITHIN,
        together(64, |n| {
            let app = app.clone();
            async move {
                let wrong = format!("Bearer wrong-{n}");
                send(
                    &app,
                    "GET",
                    "/api/messages",
                    &[("authorization", &wrong)],
                    None,
                )
                .await
            }
        }),
    )
    .await
    .expect("the requests finished");

    // Every request reached the login code: none got the Host check's 403.
    assert_eq!(tally(&replies), BTreeMap::from([(401, 10), (429, 54)]));
    assert_eq!(compared.load(Ordering::SeqCst), 10);
    for reply in &replies {
        match reply.status {
            StatusCode::UNAUTHORIZED => assert_plain_401(reply),
            _ => assert!(reply.header("retry-after").is_some()),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn sixty_four_wrong_logins_together_get_ten_comparisons_and_fifty_four_refusals() {
    let compared = Arc::new(AtomicUsize::new(0));
    let rig = Setup::new()
        .login(TOKEN)
        .comparer(slow_counting(&compared, TOKEN))
        .build();
    let app = rig.app();

    let replies = tokio::time::timeout(
        WITHIN,
        together(64, |n| {
            let app = app.clone();
            async move { login_with(&app, &format!("wrong-{n}"), &[]).await }
        }),
    )
    .await
    .expect("the requests finished");

    assert_eq!(tally(&replies), BTreeMap::from([(401, 10), (429, 54)]));
    assert_eq!(compared.load(Ordering::SeqCst), 10);
    for reply in &replies {
        assert!(reply.headers.get("set-cookie").is_none());
        match reply.status {
            StatusCode::UNAUTHORIZED => {
                assert_json_error(reply, StatusCode::UNAUTHORIZED, "Wrong token")
            }
            _ => assert_login_locked("login", reply, 60),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn wrong_logins_and_wrong_bearer_tokens_together_share_the_ten() {
    let compared = Arc::new(AtomicUsize::new(0));
    let rig = Setup::new()
        .login(TOKEN)
        .comparer(slow_counting(&compared, TOKEN))
        .build();
    let app = rig.app();

    let replies = tokio::time::timeout(
        WITHIN,
        together(64, |n| {
            let app = app.clone();
            async move {
                if n % 2 == 0 {
                    login_with(&app, &format!("wrong-{n}"), &[]).await
                } else {
                    let wrong = format!("Bearer wrong-{n}");
                    send(
                        &app,
                        "GET",
                        "/api/labels",
                        &[("authorization", &wrong)],
                        None,
                    )
                    .await
                }
            }
        }),
    )
    .await
    .expect("the requests finished");

    assert_eq!(tally(&replies), BTreeMap::from([(401, 10), (429, 54)]));
    assert_eq!(compared.load(Ordering::SeqCst), 10);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn sixty_four_right_bearer_tokens_together_are_all_served_and_none_refused() {
    let compared = Arc::new(AtomicUsize::new(0));
    let record = leak_record();
    let rig = seed(
        Setup::new()
            .login(TOKEN)
            .comparer(slow_counting(&compared, TOKEN))
            .build(),
        &record,
    );
    let app = rig.app();

    let replies = tokio::time::timeout(
        WITHIN,
        together(64, |_| {
            let app = app.clone();
            async move {
                let right = bearer(TOKEN);
                send(
                    &app,
                    "GET",
                    "/api/labels",
                    &[("authorization", &right)],
                    None,
                )
                .await
            }
        }),
    )
    .await
    .expect("the requests finished");

    assert_eq!(tally(&replies), BTreeMap::from([(200, 64)]));
    assert_eq!(compared.load(Ordering::SeqCst), 64);
    for reply in &replies {
        assert!(reply.text().contains(LABEL_TEXT));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn two_hundred_parallel_wrong_tokens_are_answered_and_leave_no_task_behind() {
    let rig = rig();
    let app = rig.app();
    let metrics = tokio::runtime::Handle::current().metrics();
    let before = metrics.num_alive_tasks();

    let replies = tokio::time::timeout(
        WITHIN,
        together(200, |n| {
            let app = app.clone();
            async move {
                let wrong = format!("Bearer wrong-{n}");
                send(
                    &app,
                    "GET",
                    "/api/messages",
                    &[("authorization", &wrong)],
                    None,
                )
                .await
            }
        }),
    )
    .await
    .expect("the requests finished");

    // Ten were compared, the rest refused at once.
    assert_eq!(tally(&replies), BTreeMap::from([(401, 10), (429, 190)]));
    // The login queues nothing: once the requests are answered, no task of
    // theirs is left.  The runtime drops a finished task a moment after its
    // result is taken, so give it that moment.
    let deadline = std::time::Instant::now() + WITHIN;
    while metrics.num_alive_tasks() != before {
        assert!(
            std::time::Instant::now() < deadline,
            "{} tasks alive, {before} before",
            metrics.num_alive_tasks()
        );
        tokio::task::yield_now().await;
    }
}

// ---------------------------------------------------------------------------
// WebSocket over a real socket
// ---------------------------------------------------------------------------

/// Open `path` with extra request headers.  The label tests' client writes the
/// `Origin` it is given into the request as it is, so the headers ride in
/// behind it.
async fn ws_connect(
    addr: SocketAddr,
    path: &str,
    headers: &[(&str, &str)],
) -> Result<WsClient, String> {
    let mut origin = format!("http://{addr}");
    for (name, value) in headers {
        origin.push_str(&format!("\r\n{name}: {value}"));
    }
    WsClient::connect(addr, path, Some(&origin)).await
}

#[tokio::test]
async fn a_handshake_without_a_credential_is_refused_with_a_401() {
    let rig = rig();
    let addr = serve(rig.app()).await;
    let forged = [("cookie", "lorawan_sniffer_session=garbage")];
    let wrong = [("authorization", "Bearer wrong")];

    for headers in [&[][..], &wrong[..], &forged[..]] {
        for path in ["/ws", "/ws/labels"] {
            let refused = ws_connect(addr, path, headers).await;

            assert_eq!(
                refused.err().as_deref(),
                Some("HTTP/1.1 401 Unauthorized"),
                "{path} with {headers:?}"
            );
        }
    }
}

#[tokio::test]
async fn a_valid_session_gets_the_live_feed_and_the_token_gets_the_labels() {
    let rig = rig();
    rig.storage
        .insert(&MessageRecord {
            id: "stored".to_string(),
            ..record_with_new_fields()
        })
        .unwrap();
    rig.storage
        .set_label(DEVICE, LABEL_TEXT, true, MAX_LABELS)
        .unwrap();
    let addr = serve(rig.app()).await;
    let cookie = rig.session_cookie();

    let mut feed = ws_connect(addr, "/ws", &[("cookie", &cookie)])
        .await
        .expect("a session opens the feed");
    // The socket is subscribed before the handshake is answered, so a record
    // broadcast now reaches it.
    let record = leak_record();
    rig.broadcast_tx
        .send(make_ws_json(&record).unwrap())
        .unwrap();
    let message = feed.json().await;
    assert_eq!(message["type"], "message");
    assert_eq!(message["data"]["id"], RECORD_ID);

    let token = bearer(TOKEN);
    let mut labels = ws_connect(addr, "/ws/labels", &[("authorization", &token)])
        .await
        .expect("the token opens the label feed");
    let snapshot = labels.json().await;
    assert_eq!(snapshot["type"], "labels");
    assert_eq!(snapshot["data"], json!({ DEVICE: LABEL_TEXT }));

    // The same listener refuses the next one without a credential.
    let refused = ws_connect(addr, "/ws", &[]).await;
    assert_eq!(refused.err().as_deref(), Some("HTTP/1.1 401 Unauthorized"));
}

// ---------------------------------------------------------------------------
// What the log holds
// ---------------------------------------------------------------------------

#[tokio::test]
async fn nothing_secret_reaches_the_log_even_at_trace_level() {
    // Values no other test uses, so that they can be looked for in a log that
    // every test of the process writes to.
    const CANARY: &str = "c4a9e1d77b2f4036a8d15e90bb3c7f21";
    const SECRET: [u8; 32] = *b"trace-test session secret 000042";
    const WRONG_LOGIN: &str = "wrong-login-marker-71";
    const WRONG_BEARER: &str = "wrong-bearer-marker-72";
    const FORGED: &str = "forged-cookie-marker-73";
    let peer: SocketAddr = "198.51.100.77:40123".parse().unwrap();
    let logger = capture_logger();
    // One router for each failure: the line about one is limited to one per
    // interval for each.
    let build = || Setup::new().login(CANARY).secret(SECRET).build();
    let from_peer = |app: Router,
                     method: &'static str,
                     uri: &'static str,
                     headers: Vec<(&'static str, String)>,
                     body: Option<String>| async move {
        let headers: Vec<(&str, &[u8])> = headers
            .iter()
            .map(|(name, value)| (*name, value.as_bytes()))
            .collect();
        raw(&app, method, uri, &headers, body.as_deref(), Some(peer)).await
    };
    let json_type = ("content-type", "application/json".to_string());

    // A login that works, a call with the token, and a login that does not.
    let rig = build();
    let login = from_peer(
        rig.app(),
        "POST",
        "/api/login",
        vec![json_type.clone()],
        Some(token_body(CANARY)),
    )
    .await;
    assert_eq!(login.status, StatusCode::NO_CONTENT, "{}", login.text());
    let cookie = set_cookie(&login);
    let session = cookie
        .strip_prefix("lorawan_sniffer_session=")
        .and_then(|rest| rest.split(';').next())
        .unwrap()
        .to_string();
    let called = from_peer(
        rig.app(),
        "GET",
        "/api/messages",
        vec![("authorization", bearer(CANARY))],
        None,
    )
    .await;
    assert_eq!(called.status, StatusCode::OK);
    let wrong_login = from_peer(
        rig.app(),
        "POST",
        "/api/login",
        vec![json_type.clone()],
        Some(token_body(WRONG_LOGIN)),
    )
    .await;
    assert_eq!(wrong_login.status, StatusCode::UNAUTHORIZED);
    // A wrong token in a header, a forged cookie, and a refused origin.
    let wrong_bearer = from_peer(
        build().app(),
        "GET",
        "/api/messages",
        vec![("authorization", bearer(WRONG_BEARER))],
        None,
    )
    .await;
    assert_eq!(wrong_bearer.status, StatusCode::UNAUTHORIZED);
    let forged = from_peer(
        build().app(),
        "GET",
        "/api/messages",
        vec![("cookie", format!("lorawan_sniffer_session={FORGED}"))],
        None,
    )
    .await;
    assert_eq!(forged.status, StatusCode::UNAUTHORIZED);
    let refused = from_peer(
        build().app(),
        "POST",
        "/api/login",
        vec![
            json_type,
            ("origin", "https://evil-trace-marker.example".to_string()),
        ],
        Some(token_body(CANARY)),
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);

    let lines: Vec<String> = logger.lines.lock().unwrap().clone();
    let mac = session.rsplit('.').next().unwrap().to_string();
    let nonce = session.split('.').nth(2).unwrap().to_string();
    let secrets = [
        CANARY.to_string(),
        hex(&SECRET),
        format!("{SECRET:?}"),
        "trace-test session secret".to_string(),
        session.clone(),
        mac,
        nonce,
        WRONG_LOGIN.to_string(),
        WRONG_BEARER.to_string(),
        FORGED.to_string(),
    ];
    for line in &lines {
        for secret in &secrets {
            assert!(
                !line.contains(secret.as_str()),
                "a log line holds {secret}: {line}"
            );
        }
    }

    // What the log does say: who, and what kind of failure.
    let about_peer: Vec<&String> = lines
        .iter()
        .filter(|l| l.contains("198.51.100.77:40123"))
        .collect();
    let count = |level: &str, what: &str| {
        about_peer
            .iter()
            .filter(|l| l.starts_with(level) && l.contains(what))
            .count()
    };
    assert_eq!(
        count("INFO", "WebUI login from 198.51.100.77:40123"),
        1,
        "{about_peer:?}"
    );
    assert_eq!(
        count(
            "WARN",
            "WebUI login failed from 198.51.100.77:40123: wrong token"
        ),
        2,
        "{about_peer:?}"
    );
    assert_eq!(
        count(
            "WARN",
            "WebUI login failed from 198.51.100.77:40123: invalid session"
        ),
        1,
        "{about_peer:?}"
    );
    let origin_lines: Vec<&String> = lines
        .iter()
        .filter(|l| l.contains("evil-trace-marker.example"))
        .collect();
    assert_eq!(origin_lines.len(), 1, "{origin_lines:?}");
    assert!(origin_lines[0].starts_with("WARN"));
    assert!(origin_lines[0].contains("Host 127.0.0.1:8888"));
}
