//! HTTP + WebSocket API server (axum).
//!
//! ## Endpoints
//!
//! | Method | Path                   | Description                                  |
//! |--------|------------------------|----------------------------------------------|
//! | GET    | `/ws`                  | WebSocket: real-time message feed            |
//! | GET    | `/ws/labels`           | WebSocket: the device labels, then each change |
//! | GET    | `/api/messages`        | REST: historical messages (query params)     |
//! | GET    | `/api/labels`          | REST: every device label                     |
//! | PUT    | `/api/labels/{device}` | REST: set or replace a device label          |
//! | DELETE | `/api/labels/{device}` | REST: remove a device label                  |
//! | GET    | `/api/session`         | REST: `{"login": true}` or `{"login": false}` |
//! | POST   | `/api/login`           | REST: start a session (login on only)        |
//! | POST   | `/api/logout`          | REST: end the session in this browser (login on only) |
//! | GET    | `/*`                   | Serve embedded React SPA                     |
//!
//! ## WebSocket protocol
//!
//! `/ws`, server → client:
//! ```json
//! { "type": "message", "data": { ...MessageRecord... } }
//! ```
//!
//! `/ws/labels`, server → client.  On connect, the full set; then one
//! `label` per change, in the order the changes were saved, with `null` for a
//! removal.  A client that falls behind gets a fresh full set instead of the
//! changes it missed:
//! ```json
//! { "type": "labels", "data": { "00E8A80B": "Havana sensor", ... } }
//! { "type": "label",  "data": { "device": "00E8A80B", "label": "Havana sensor" } }
//! { "type": "label",  "data": { "device": "00E8A80B", "label": null } }
//! ```
//! `/ws` carries no label messages.
//!
//! Client → server: ignored on both sockets.
//!
//! A `/ws` client that falls behind, or that stops accepting data for 10
//! seconds, is closed with code 1013 ("try again later"; the close frame is
//! attempted for at most a second) and dropped, instead of being left to miss
//! records without knowing.  The WebUI reconnects and reloads its range.
//! The socket is subscribed to the record feed before the handshake is
//! answered, so every record broadcast after the browser's `open` event
//! reaches it.
//!
//! ## Labels
//!
//! `PUT` takes `{"label": "text"}` and answers `200 {"device": "...",
//! "label": "..."}`, with `"label": null` when an empty label removed it.
//! `DELETE` answers `204`, also when there was nothing to delete.  Errors from
//! the label endpoints are `{"error": "<message for the person using the
//! WebUI>"}`: 400 (identifier, body or label), 403 (`Origin`), 404 (device
//! neither stored nor seen live), 409 (label limit), 415 (body not JSON),
//! 500 (storage).  Every change is made by the label writer ([`labels`]),
//! which publishes it on `/ws/labels` in the order it was saved.
//!
//! ## Login
//!
//! With `[sniffer.api] login = true`, everything under `/ws` and `/api/` needs
//! a credential ([`auth`]): the session cookie that `POST /api/login` sets, or
//! `Authorization: Bearer <token>` (curl, scripts).  The WebUI bundle stays
//! public, because it is the page that asks for the token.  A request without
//! a credential, or with a wrong one, gets `401` with a plain-text body and no
//! `WWW-Authenticate` header (that header would make a browser show its own
//! password dialog), and a token that is presented once the wrong-token limit
//! is reached gets `429` with `Retry-After` whatever it is.  `/api/session`
//! needs the credential too: a `200` from it is how the page learns that it is
//! logged in, and a `401` that it must log in.  With login off it answers
//! `{"login": false}` without one, and `/api/login` and `/api/logout` do not
//! exist.
//!
//! `POST /api/login` takes `{"token": "<token>"}` and answers `204` with the
//! cookie (`HttpOnly; SameSite=Strict; Path=/`, so it ends with the browser
//! session), `401 {"error": "Wrong token"}`, `429` with `Retry-After`, or `500`
//! when the sniffer cannot make a session.  `POST /api/logout` answers `204`
//! with an expired cookie.  Both accept only a request that declares exactly
//! one `Content-Type: application/json` and, when it carries an `Origin`, one
//! whose host and port are those of its `Host` header: a form or a script on
//! another site can do neither.  `cors_origins` do not widen this, and neither
//! does `X-Forwarded-Host`, which is never read.  A reverse proxy must
//! therefore pass the browser's `Host` header on unchanged.
//!
//! The login layer sits inside CORS (a preflight needs no credential, and the
//! CORS layer lists `Authorization` among the headers a listed origin may
//! send) and inside the Host check, which still answers first.  Its refusals
//! come from outside the `/api/messages` route, so they do not carry
//! `X-Storage-Write-Failures`.
//!
//! ## Host check
//!
//! Every request, WebSockets included, must be addressed to the sniffer by an
//! IP address, `localhost` or a name in `allowed_hosts` ([`host`]); anything
//! else gets a plain-text 403.  This is what stops DNS rebinding, which makes
//! a foreign page look same-origin and so defeats the rules below.
//!
//! ## Cross-origin access
//!
//! Whether or not the login is on, the API must not be readable from other
//! web sites the operator happens to open.  By default it sends no CORS
//! headers, so browsers only let the sniffer's own pages read it.  Browsers
//! never subject WebSocket handshakes to CORS, and a form can be made to send
//! some kinds of write, so those are checked against the `Origin` header
//! ([`origin_allowed`]): the handshakes of `/ws` and `/ws/labels` and the
//! `PUT` and `DELETE` label endpoints accept a request with no `Origin`
//! (curl and scripts), with an `Origin` naming the host the request was sent
//! to, or with one listed in `cors_origins`.  Origins listed in
//! `cors_origins` may also read the API through CORS, and write labels.  With
//! the login on they must send the token as `Authorization: Bearer`: the
//! session cookie is `SameSite=Strict`, which a browser never sends along with
//! a request that another site makes.
//!
//! ## `/api/messages`
//!
//! `GET`, a JSON array of records, oldest first, at most 5,000.
//!
//! | Parameter  | Meaning |
//! |------------|---------|
//! | `since`    | Only records strictly after this RFC 3339 time; the oldest `limit` of them.  Unparsable: ignored. |
//! | `from`     | Only records at or after this RFC 3339 time.  Unparsable, or together with a valid `since`: 400. |
//! | `newest`   | `true` keeps the newest `limit` matches, `false` the oldest.  Default: `false` when a bound was understood, `true` otherwise. |
//! | `limit`    | Most records, 1 to 5,000, default 500; clamped.  Not a number: axum's own 400. |
//! | `device`   | Text found in the DevAddr, the DevEUI or the label of the record's identifier, case-insensitively. |
//! | `mtype`    | Comma-separated type names (`UnconfirmedDataUp,JoinRequest`).  Unknown name: 400. |
//! | `status`   | `forwarded`, `blocked`, `captured` (any case) or `all`.  Else: 400. |
//! | `gateway`  | Gateway EUI, compared exactly as records carry it. |
//! | `malformed`| `include` (default) or `exclude`.  Else: 400. |
//! | `count`    | `true` adds `X-Total-Count` and `X-Malformed-Count`.  Default `false`. |
//! | `settle`   | `true` waits for the storage writer first (see below).  Default `false`. |
//!
//! An empty `device`, `mtype`, `status` or `gateway` means any.  `newest`,
//! `count` and `settle` take exactly `true` or `false`.  Errors in the new
//! parameters are `400 {"error": "<message>"}`.
//!
//! Headers of the answer:
//!
//! - `X-Total-Count`: records that match (with `malformed=exclude`, the
//!   malformed ones are not among them), as many as there are, not only
//!   those returned.  `count=true`.
//! - `X-Malformed-Count`: malformed records that match every other
//!   condition, whether the request returns them or not.  `count=true`.
//!   Absent when the database cannot count them without a scan on the
//!   connection that saves records (it is not in WAL mode).
//! - `X-History-Settled`: `true` when the storage writer had processed
//!   everything queued before the request, `false` when that took more than 5
//!   seconds and the answer is what was saved by then.  `settle=true`.
//!   "Processed" is not "saved": a record whose insert failed was counted
//!   instead.
//! - `X-Storage-Write-Failures`: inserts that failed since the sniffer
//!   started (the count restarts at 0 with it).  On every answer of this
//!   route, errors from the query string included; not on the Host check's
//!   403 or the login's 401 and 429, which are answered before any route.
//!
//! The 503s are `{"error": "<message>", "code": "<code>"}`: `no_wal` when a
//! filter is asked of a database that is not in WAL mode (a request with only
//! a time bound still works), and `busy` when the database stayed locked for a
//! second.
//!
//! One history query runs at a time, and at most three answers are held in
//! memory at once; further requests wait their turn.  A request whose client
//! has gone stops its query.
//!
//! A request without `from`, `newest` or any of the new parameters returns
//! exactly what it always did: `?limit=N` the newest N, `?since=T&limit=N` the
//! oldest N after T.

pub mod auth;
pub mod host;
pub mod labels;
mod messages;

use std::collections::BTreeMap;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::{
        rejection::JsonRejection,
        ws::{
            rejection::WebSocketUpgradeRejection, CloseFrame, Message, WebSocket, WebSocketUpgrade,
        },
        DefaultBodyLimit, Path, State,
    },
    http::{header, HeaderMap, Method, StatusCode, Uri},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post, put},
    Json, Router,
};
use rust_embed::RustEmbed;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::{broadcast, mpsc, Semaphore};
use tower_http::cors::CorsLayer;

use self::auth::{require_login, Auth, LOGIN_BODY_LIMIT};
use self::host::{check_host, AllowedHosts};
use self::labels::{LabelWriter, WriterError};
use crate::core::labels::{normalize_label, parse_device_id};
use crate::core::linelog::warn_line;
use crate::core::ratelimit::RateLimitedLog;
use crate::core::{
    metadata::MessageRecord,
    storage::{LabelWrite, Storage},
    writer::WriterItem,
};

// ---------------------------------------------------------------------------
// Embedded frontend assets
// ---------------------------------------------------------------------------

/// Embed the contents of `../webui/dist/` into the binary at compile time.
/// Build the webui first: `cd v2/webui && npm run build`
#[derive(RustEmbed)]
#[folder = "../webui/dist"]
struct Frontend;

/// Largest request body a label write accepts.  A label is at most 64
/// characters, which even with every one escaped in JSON is well under half
/// a kilobyte; a larger body is not a label, and refusing it early keeps the
/// default 2 MB read off an endpoint anyone on the network can reach.
const MAX_LABEL_BODY_BYTES: usize = 4 * 1024;

/// History queries that run at once.  A scan can take hundreds of
/// milliseconds on a large database, and the reader has one connection, so a
/// second query would only wait behind the first.
const HISTORY_QUERIES: usize = 1;

/// Full history answers held in memory at once, from the start of their
/// query until the client has taken them.  An answer of 5,000 records is
/// about 3 MB, so three keep the sniffer's memory near 10 MB above its
/// baseline however many tabs ask.
const HISTORY_BODIES: usize = 3;

/// The longest a frame may take to be accepted by a `/ws` client.  Beyond it
/// the client has stopped reading, and waiting longer would hold this
/// handler, and the memory behind it, for as long as the peer likes.
const WS_SEND_TIMEOUT: Duration = Duration::from_secs(10);

/// The longest the close frame of a client that fell behind is given.  The
/// client is not reading, so the frame may never get out; it is a courtesy,
/// not something to wait for.
const WS_CLOSE_TIMEOUT: Duration = Duration::from_secs(1);

/// The close code for "try again later".
const WS_TRY_AGAIN_LATER: u16 = 1013;

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

/// What the API needs from the rest of the sniffer.
pub struct ApiContext {
    /// Pre-serialised records for the `/ws` clients.
    pub broadcast_tx: broadcast::Sender<Arc<String>>,
    /// Label events for the `/ws/labels` clients; published by `writer`.
    pub labels_tx: broadcast::Sender<Arc<String>>,
    pub storage: Storage,
    /// The only thing that changes labels.
    pub writer: LabelWriter,
    /// Origins other than the sniffer's own that may use the API.
    pub cors_origins: Vec<String>,
    /// Host names (besides IP addresses and `localhost`) the API answers to,
    /// as configured; `["*"]` turns the Host check off.
    pub allowed_hosts: Vec<String>,
    /// The storage writer's queue, to put markers on.  Weak, so that the API
    /// never keeps the queue open after the proxy, which owns the strong
    /// handles, has stopped.
    pub writer_queue: mpsc::WeakSender<WriterItem>,
    /// Inserts that did not save, counted by the writer.
    pub write_failures: Arc<AtomicU64>,
    /// The login, when it is on: every `/ws` and `/api` request then needs a
    /// session or the token.  `None` leaves the API open, as before.
    pub login: Option<Arc<Auth>>,
}

#[derive(Clone)]
pub struct AppState {
    broadcast_tx: broadcast::Sender<Arc<String>>,
    labels_tx: broadcast::Sender<Arc<String>>,
    storage: Storage,
    writer: LabelWriter,
    /// Origins other than the sniffer's own that may use the API.
    allowed_origins: Arc<Vec<String>>,
    /// One history query at a time ([`HISTORY_QUERIES`]).
    history: Arc<Semaphore>,
    /// Large answers in memory ([`HISTORY_BODIES`]).
    history_bodies: Arc<Semaphore>,
    /// One permit, owned by the marker that is in the writer queue, if any.
    history_settle: Arc<Semaphore>,
    writer_queue: mpsc::WeakSender<WriterItem>,
    write_failures: Arc<AtomicU64>,
    /// Limits the warning for a writer that does not answer in time.
    settle_log: Arc<RateLimitedLog>,
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router(ctx: ApiContext) -> Router {
    build_router(ctx).0
}

/// [`router`], and the state behind it, which the tests look into.
fn build_router(ctx: ApiContext) -> (Router, AppState) {
    let state = AppState {
        broadcast_tx: ctx.broadcast_tx,
        labels_tx: ctx.labels_tx,
        storage: ctx.storage,
        writer: ctx.writer,
        allowed_origins: Arc::new(ctx.cors_origins),
        history: Arc::new(Semaphore::new(HISTORY_QUERIES)),
        history_bodies: Arc::new(Semaphore::new(HISTORY_BODIES)),
        history_settle: Arc::new(Semaphore::new(1)),
        writer_queue: ctx.writer_queue,
        write_failures: ctx.write_failures,
        settle_log: Arc::new(RateLimitedLog::new()),
    };

    // Every answer of this route says how many inserts have failed.  A route
    // layer, not a line in the handler: axum refuses a bad query string
    // before the handler runs.
    let messages =
        get(messages::messages_handler).route_layer(middleware::map_response_with_state(
            state.write_failures.clone(),
            messages::add_failure_header,
        ));

    let mut app = Router::new()
        .route("/ws", get(ws_handler))
        .route("/ws/labels", get(labels_ws_handler))
        .route("/api/messages", messages)
        .route("/api/labels", get(list_labels))
        .route(
            "/api/labels/:device",
            put(put_label)
                .delete(delete_label)
                .layer(DefaultBodyLimit::max(MAX_LABEL_BODY_BYTES)),
        )
        .fallback(frontend_handler);

    // Whether the WebUI has to log in is the first thing it asks, so the
    // route exists either way.  With login on, `require_login` stands in
    // front of it like any other `/api` path.
    app = app.route(
        "/api/session",
        if ctx.login.is_some() {
            get(auth::session_open)
        } else {
            get(auth::session_closed)
        },
    );

    // Login and logout exist only when there is a login.  Without one the
    // paths are unknown and the page answers, as for any other path.
    if let Some(login) = &ctx.login {
        let login_routes: Router<AppState> = Router::<Arc<Auth>>::new()
            .route(
                "/api/login",
                post(auth::login).layer(DefaultBodyLimit::max(LOGIN_BODY_LIMIT)),
            )
            .route("/api/logout", post(auth::logout))
            .with_state(login.clone());
        // The layer comes after the merge so that it covers every route and
        // the page's fallback, and before CORS and the Host check, so that a
        // preflight needs no credential and a foreign host is refused first.
        app = app
            .merge(login_routes)
            .layer(middleware::from_fn_with_state(login.clone(), require_login));
    }

    // Without CORS headers a browser only lets pages served by the sniffer
    // read the API.  Answering every origin would let any web site the
    // operator visits read the captured traffic from their browser.
    if !state.allowed_origins.is_empty() {
        let origins: Vec<axum::http::HeaderValue> = state
            .allowed_origins
            .iter()
            .filter_map(|o| o.parse().ok())
            .collect();
        app = app.layer(
            CorsLayer::new()
                .allow_origin(origins)
                // A label write is a PUT or DELETE with a JSON body, and
                // browsers ask permission for both before sending it.
                .allow_methods([Method::GET, Method::PUT, Method::DELETE])
                // With login on, `Authorization` carries the token for a
                // script on a listed origin, which cannot use the session
                // cookie: it is `SameSite=Strict`, so a cross-site request
                // never carries it.  With login off nothing reads the header,
                // and the answer stays what it was before there was a login.
                .allow_headers(if ctx.login.is_some() {
                    vec![header::CONTENT_TYPE, header::AUTHORIZATION]
                } else {
                    vec![header::CONTENT_TYPE]
                })
                // A script on a listed origin can only read the headers the
                // server names; these carry the counts and the flags of the
                // history answer.
                .expose_headers([
                    messages::TOTAL_COUNT,
                    messages::MALFORMED_COUNT,
                    messages::HISTORY_SETTLED,
                    messages::STORAGE_WRITE_FAILURES,
                ]),
        );
    }

    // Added last, so it is the outermost layer: a request for a foreign host
    // is refused before anything else sees it, CORS preflights included.
    let allowed_hosts = Arc::new(AllowedHosts::new(&ctx.allowed_hosts));
    let app = app
        .layer(middleware::from_fn_with_state(allowed_hosts, check_host))
        .with_state(state.clone());
    (app, state)
}

// ---------------------------------------------------------------------------
// WebSocket handlers
// ---------------------------------------------------------------------------

async fn ws_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    if !origin_allowed(&headers, &uri, &state.allowed_origins) {
        debug!(
            "WebSocket handshake refused for origin {:?}",
            headers.get(header::ORIGIN)
        );
        return StatusCode::FORBIDDEN.into_response();
    }
    match ws {
        Ok(ws) => {
            // Subscribe now, not inside `on_upgrade`: that closure runs after
            // the 101 answer has gone out, and a record broadcast in between
            // would reach neither this socket nor, if the page reloads its
            // range on opening, necessarily the reload.  A record broadcast
            // after the subscription reaches the socket; one broadcast before
            // it was queued for storage before the page asked for anything.
            let records = state.broadcast_tx.subscribe();
            ws.on_upgrade(move |socket| handle_socket(socket, records))
                .into_response()
        }
        Err(rejection) => rejection.into_response(),
    }
}

/// Whether a request that browsers do not subject to CORS may go ahead,
/// judged by its `Origin` header.  Used for the WebSocket handshakes and for
/// label writes.
///
/// Browsers do not apply CORS to WebSockets, so without this check any page
/// the operator opens could connect to the live feed.  Browsers always send
/// `Origin` on a handshake; it is accepted when it names the host the request
/// was sent to (one of the sniffer's own pages, also through the WebUI dev
/// server's proxy) or is listed in `cors_origins`.  A request without
/// `Origin` does not come from a browser and is accepted.
///
/// The host the request was sent to is its `Host` header or, for HTTP/2,
/// which has none, the authority of the request URI.
fn origin_allowed(headers: &HeaderMap, uri: &Uri, allowed: &[String]) -> bool {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return true;
    };
    let Ok(origin) = origin.to_str() else {
        return false;
    };
    if allowed
        .iter()
        .any(|a| a.trim_end_matches('/').eq_ignore_ascii_case(origin))
    {
        return true;
    }
    // An origin is `scheme://host[:port]`; a sandboxed page sends "null".
    let Some((_, authority)) = origin.split_once("://") else {
        return false;
    };
    let host = match headers.get(header::HOST) {
        Some(host) => host.to_str().ok(),
        None => uri.authority().map(|a| a.as_str()),
    };
    host.is_some_and(|host| host.eq_ignore_ascii_case(authority))
}

async fn handle_socket(mut socket: WebSocket, mut rx: broadcast::Receiver<Arc<String>>) {
    loop {
        tokio::select! {
            result = rx.recv() => {
                match result {
                    Ok(json) => {
                        let text = (*json).clone();
                        match tokio::time::timeout(WS_SEND_TIMEOUT, socket.send(Message::Text(text))).await {
                            Ok(Ok(())) => {}
                            Ok(Err(_)) => break,
                            Err(_) => {
                                warn_line!(
                                    "WebSocket client did not accept a message within {}s; closing it",
                                    WS_SEND_TIMEOUT.as_secs()
                                );
                                close_fallen_behind(&mut socket).await;
                                break;
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        // The records it missed are gone for it.  Closing the
                        // socket makes the page reconnect and reload its
                        // range, which is how it gets them back; leaving it
                        // open would show a table with a hole and nothing
                        // to say so.
                        warn_line!("WebSocket client lagged {} messages; closing it", n);
                        close_fallen_behind(&mut socket).await;
                        break;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => {} // ignore other client messages
                }
            }
        }
    }
    debug!("WebSocket client disconnected");
}

/// Tell a client that fell behind to come back, if it will listen.  A client
/// that is not reading cannot be told, so this gives up after a second and the
/// caller drops the socket either way.
async fn close_fallen_behind(socket: &mut WebSocket) {
    let close = Message::Close(Some(CloseFrame {
        code: WS_TRY_AGAIN_LATER,
        reason: "fell behind; reconnect and reload".into(),
    }));
    let _ = tokio::time::timeout(WS_CLOSE_TIMEOUT, socket.send(close)).await;
}

async fn labels_ws_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    if !origin_allowed(&headers, &uri, &state.allowed_origins) {
        debug!(
            "Label WebSocket handshake refused for origin {:?}",
            headers.get(header::ORIGIN)
        );
        return StatusCode::FORBIDDEN.into_response();
    }
    match ws {
        Ok(ws) => {
            // Listen before the full set is read.  A change saved after the
            // set is read is then always delivered as an event; one saved
            // before may arrive as an event too, which does no harm, because
            // an event carries the whole value for its identifier.
            let events = state.labels_tx.subscribe();
            ws.on_upgrade(move |socket| handle_labels_socket(socket, events, state.storage))
                .into_response()
        }
        Err(rejection) => rejection.into_response(),
    }
}

/// Serve one `/ws/labels` client: the full set, then every change.
async fn handle_labels_socket(
    mut socket: WebSocket,
    mut events: broadcast::Receiver<Arc<String>>,
    storage: Storage,
) {
    if !send_label_set(&mut socket, &storage).await {
        return;
    }
    loop {
        tokio::select! {
            event = events.recv() => {
                match event {
                    Ok(json) => {
                        if socket.send(Message::Text((*json).clone())).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(missed)) => {
                        debug!(
                            "Label WebSocket client missed {} event(s); sending the full set again",
                            missed
                        );
                        // Every event published so far was saved before it was
                        // published, so a set read from now on already holds
                        // it.  Starting a new subscription drops the backlog
                        // that set makes redundant, instead of replaying old
                        // values over it.
                        events = events.resubscribe();
                        if !send_label_set(&mut socket, &storage).await {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                    _ => {} // ignore other client messages
                }
            }
        }
    }
    debug!("Label WebSocket client disconnected");
}

/// Send the full label set.  `false` when it could not be read or sent, in
/// which case the socket is closed; the WebUI reconnects and tries again.
async fn send_label_set(socket: &mut WebSocket, storage: &Storage) -> bool {
    let Some(labels) = load_labels(storage).await else {
        return false;
    };
    socket
        .send(Message::Text(make_labels_json(&labels)))
        .await
        .is_ok()
}

// ---------------------------------------------------------------------------
// REST: /api/labels
// ---------------------------------------------------------------------------

/// `{"device": ..., "label": ...}`, the answer to a successful `PUT`.
#[derive(Serialize)]
struct LabelAnswer<'a> {
    device: &'a str,
    label: Option<&'a str>,
}

/// `{"error": ...}`, the body of every error from the label endpoints.  The
/// text is shown to the person using the WebUI as it is.
#[derive(Serialize)]
struct ErrorAnswer {
    error: String,
}

fn json_error(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        Json(ErrorAnswer {
            error: message.into(),
        }),
    )
        .into_response()
}

/// `GET /api/labels`: every label, as `{"<device>": "<label>", ...}`.  Like
/// `/api/messages`, it is read-only and does not look at `Origin`.
async fn list_labels(State(state): State<AppState>) -> Response {
    match load_labels(&state.storage).await {
        Some(labels) => Json(labels).into_response(),
        None => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "the labels could not be read because of a server error; see the sniffer log",
        ),
    }
}

/// `PUT /api/labels/{device}` with `{"label": "..."}`.
///
/// The checks run in this order, so the answer to a request that is wrong in
/// several ways does not depend on how it was assembled: `Origin` (403),
/// identifier (400), body (415 if it is not declared as JSON, otherwise 400),
/// label (400).  An empty label removes the label and needs no known device,
/// so that removing always works.
async fn put_label(
    State(state): State<AppState>,
    Path(device): Path<String>,
    headers: HeaderMap,
    uri: Uri,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    if let Some(refusal) = refuse_foreign_origin(&state, &headers, &uri) {
        return refusal;
    }
    let device = match parse_device_id(&device) {
        Ok(device) => device,
        Err(e) => return json_error(StatusCode::BAD_REQUEST, e.to_string()),
    };
    let Json(body) = match body {
        Ok(body) => body,
        Err(rejection) => return refuse_body(rejection),
    };
    // Read by hand rather than into a struct: serde would also fill a struct
    // from a JSON array (`["text"]`), and only an object is a label body.
    let Some(label) = body.get("label").and_then(Value::as_str) else {
        return refuse_body_shape();
    };
    let label = match normalize_label(label) {
        Ok(label) => label,
        Err(e) => return json_error(StatusCode::BAD_REQUEST, e.to_string()),
    };

    let Some(label) = label else {
        return match state.writer.delete(device.clone()).await {
            Ok(_) => label_answer(&device, None),
            Err(e) => writer_failure(e),
        };
    };
    match state.writer.set(device.clone(), label.clone()).await {
        Ok(LabelWrite::Set) => label_answer(&device, Some(&label)),
        Ok(LabelWrite::UnknownDevice) => json_error(
            StatusCode::NOT_FOUND,
            "unknown device: no stored record, and not seen live within the retention period",
        ),
        Ok(LabelWrite::LimitReached) => json_error(
            StatusCode::CONFLICT,
            format!(
                "label limit reached ({} labels); remove one first",
                state.writer.max_labels()
            ),
        ),
        Err(e) => writer_failure(e),
    }
}

/// `DELETE /api/labels/{device}`: 204, whether or not there was a label.
async fn delete_label(
    State(state): State<AppState>,
    Path(device): Path<String>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    if let Some(refusal) = refuse_foreign_origin(&state, &headers, &uri) {
        return refusal;
    }
    let device = match parse_device_id(&device) {
        Ok(device) => device,
        Err(e) => return json_error(StatusCode::BAD_REQUEST, e.to_string()),
    };
    match state.writer.delete(device).await {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => writer_failure(e),
    }
}

/// The 403 for a write that comes from a page on another site, if it does.
fn refuse_foreign_origin(state: &AppState, headers: &HeaderMap, uri: &Uri) -> Option<Response> {
    if origin_allowed(headers, uri, &state.allowed_origins) {
        return None;
    }
    debug!(
        "Label write refused for origin {:?}",
        headers.get(header::ORIGIN)
    );
    Some(json_error(
        StatusCode::FORBIDDEN,
        "cross-origin request refused; add the origin to [sniffer.api] cors_origins",
    ))
}

/// The error for a request whose body could not be read as JSON.
fn refuse_body(rejection: JsonRejection) -> Response {
    match rejection {
        JsonRejection::MissingJsonContentType(_) => json_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "the request body must be JSON: send Content-Type: application/json",
        ),
        // Malformed JSON, a body that is too large or could not be read: none
        // of them is `{"label": "<text>"}`.
        _ => refuse_body_shape(),
    }
}

/// The error for a body that is JSON but not `{"label": "<text>"}`.
fn refuse_body_shape() -> Response {
    json_error(
        StatusCode::BAD_REQUEST,
        "the request body must be a JSON object like {\"label\": \"text\"}",
    )
}

fn label_answer(device: &str, label: Option<&str>) -> Response {
    Json(LabelAnswer { device, label }).into_response()
}

/// The 500 for a label command that did not complete.  The writer has logged
/// the cause.
fn writer_failure(error: WriterError) -> Response {
    let message = match error {
        WriterError::Stopped => "the sniffer is shutting down; try again once it is back",
        WriterError::Storage(_) | WriterError::Task(_) => {
            "the label could not be saved because of a server error; see the sniffer log"
        }
    };
    json_error(StatusCode::INTERNAL_SERVER_ERROR, message)
}

/// Read every label from the database without blocking the runtime.  `None`,
/// after logging, when that failed.
async fn load_labels(storage: &Storage) -> Option<BTreeMap<String, String>> {
    let storage = storage.clone();
    match tokio::task::spawn_blocking(move || storage.labels()).await {
        Ok(Ok(labels)) => Some(labels),
        Ok(Err(e)) => {
            error!("Labels query error: {}", e);
            None
        }
        Err(e) => {
            error!("Labels query task panicked: {}", e);
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Static file serving (embedded React SPA)
// ---------------------------------------------------------------------------

async fn frontend_handler(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    // Try exact path, then fall back to index.html for SPA client-side routing.
    serve_embedded(path).unwrap_or_else(|| {
        serve_embedded("index.html").unwrap_or_else(|| StatusCode::NOT_FOUND.into_response())
    })
}

fn serve_embedded(path: &str) -> Option<Response> {
    let file = Frontend::get(path)?;
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    Some(
        (
            [(header::CONTENT_TYPE, mime.as_ref().to_owned())],
            file.data.into_owned(),
        )
            .into_response(),
    )
}

// ---------------------------------------------------------------------------
// WebSocket envelopes
// ---------------------------------------------------------------------------

/// Serialize a `MessageRecord` into the WebSocket envelope JSON:
/// `{"type":"message","data":{...}}`.
///
/// Returns `None` if serialization fails (should never happen in practice).
pub fn make_ws_json(record: &MessageRecord) -> Option<Arc<String>> {
    #[derive(Serialize)]
    struct Envelope<'a> {
        #[serde(rename = "type")]
        kind: &'static str,
        data: &'a MessageRecord,
    }
    let env = Envelope {
        kind: "message",
        data: record,
    };
    serde_json::to_string(&env).ok().map(Arc::new)
}

/// Serialize one label change for `/ws/labels`:
/// `{"type":"label","data":{"device":"00E8A80B","label":"..."}}`, with
/// `"label":null` for a removal.
///
/// The label is operator-typed text, so it goes through the serializer and is
/// never pasted into a string.
pub fn make_label_json(device: &str, label: Option<&str>) -> Arc<String> {
    #[derive(Serialize)]
    struct Envelope<'a> {
        #[serde(rename = "type")]
        kind: &'static str,
        data: LabelAnswer<'a>,
    }
    let env = Envelope {
        kind: "label",
        data: LabelAnswer { device, label },
    };
    // Two strings and a constant cannot fail to serialize.
    Arc::new(serde_json::to_string(&env).expect("a label event always serializes"))
}

/// Serialize the full label set for `/ws/labels`:
/// `{"type":"labels","data":{"00E8A80B":"..."}}`.
fn make_labels_json(labels: &BTreeMap<String, String>) -> String {
    #[derive(Serialize)]
    struct Envelope<'a> {
        #[serde(rename = "type")]
        kind: &'static str,
        data: &'a BTreeMap<String, String>,
    }
    let env = Envelope {
        kind: "labels",
        data: labels,
    };
    // A map of strings cannot fail to serialize.
    serde_json::to_string(&env).expect("the label set always serializes")
}

#[cfg(test)]
mod label_tests;
#[cfg(test)]
mod login_tests;
#[cfg(test)]
mod message_tests;

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::time::Duration;

    use axum::body::Body;
    use axum::http::Request;
    use chrono::Utc;
    use http_body_util::BodyExt;
    use serde_json::{json, Value};
    use tokio_util::sync::CancellationToken;
    use tower::ServiceExt;

    use super::auth::{Clock, Comparer, SESSION_COOKIE};
    use super::labels::Gate;
    use super::*;
    use crate::cmd::root::{run_writer, CHANNEL_CAPACITY};
    use crate::core::decoder;
    use crate::core::labels::MAX_LABELS;
    use crate::core::metadata::{CrcStatus, Direction, MType, Malformed, Protocol, Status};
    use crate::core::random::{RandomError, RandomSource, SystemRandomSource};
    use crate::core::seen::SeenDevices;
    use crate::proxy::frame_fields;

    /// The WebUI footer shows the version from `webui/package.json`, which is
    /// written by hand next to the crate's.  If they drift apart, the page
    /// would report a version other than the binary it is embedded in.
    #[test]
    fn webui_version_matches_the_binary() {
        let package: Value = serde_json::from_str(include_str!("../../../webui/package.json"))
            .expect("webui/package.json is valid JSON");
        assert_eq!(
            package["version"].as_str(),
            Some(env!("CARGO_PKG_VERSION")),
            "webui/package.json and lorawan-sniffer/Cargo.toml must carry the same version"
        );
    }

    // -- fixtures shared with the label tests -------------------------------

    /// Retention period the test writer uses for "seen live".
    const LIVE_WINDOW: Duration = Duration::from_secs(3600);

    /// The `Host` of a test request that does not name one: an IP address,
    /// so the Host check lets it through.
    pub(super) const DEFAULT_HOST: &str = "127.0.0.1:8888";

    /// The stored session secret of a router built with a login, unless the
    /// test chooses another with [`Setup::secret`].
    pub(super) const LOGIN_SECRET: [u8; 32] = *b"router-tests session secret 0001";

    /// A random source that serves `fills` requests and then fails.  Setting
    /// up the login takes one, so `FailsAfter::new(1)` gives a login that
    /// works until the first session has to be made.
    pub(super) struct FailsAfter {
        left: AtomicUsize,
    }

    impl FailsAfter {
        pub(super) fn new(fills: usize) -> Arc<Self> {
            Arc::new(FailsAfter {
                left: AtomicUsize::new(fills),
            })
        }
    }

    impl RandomSource for FailsAfter {
        fn fill(&self, out: &mut [u8]) -> Result<(), RandomError> {
            let served = self
                .left
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok();
            if !served {
                return Err(RandomError);
            }
            SystemRandomSource::new().fill(out)
        }
    }

    /// The login of a router under test.
    struct LoginSetup {
        token: String,
        secret: [u8; 32],
        random: Arc<dyn RandomSource>,
        clock: Arc<dyn Clock>,
        compare: Option<Comparer>,
    }

    /// How the API under test is assembled.
    pub(super) struct Setup {
        max_labels: usize,
        labels_capacity: usize,
        broadcast_capacity: usize,
        gated: bool,
        cors_origins: Vec<String>,
        allowed_hosts: Vec<String>,
        storage: Option<Storage>,
        record_writer: bool,
        login: Option<LoginSetup>,
    }

    impl Setup {
        pub(super) fn new() -> Self {
            Setup {
                max_labels: MAX_LABELS,
                labels_capacity: 256,
                broadcast_capacity: 512,
                gated: false,
                cors_origins: vec![],
                allowed_hosts: vec![],
                storage: None,
                record_writer: true,
                login: None,
            }
        }

        /// Turn the login on, with this token, the stored secret
        /// [`LOGIN_SECRET`], the system's random source and the Tokio clock.
        /// Without it the router has no login, as when the setting is off.
        pub(super) fn login(mut self, token: &str) -> Self {
            self.login = Some(LoginSetup {
                token: token.to_string(),
                secret: LOGIN_SECRET,
                random: Arc::new(SystemRandomSource::new()),
                clock: Arc::new(auth::TokioClock),
                compare: None,
            });
            self
        }

        fn login_setup(&mut self) -> &mut LoginSetup {
            self.login
                .as_mut()
                .expect("call login() before choosing how the login is built")
        }

        /// The stored session secret the login signs with.
        pub(super) fn secret(mut self, secret: [u8; 32]) -> Self {
            self.login_setup().secret = secret;
            self
        }

        /// The random source the login uses, for example one that fails
        /// ([`FailsAfter`]).
        pub(super) fn random(mut self, random: Arc<dyn RandomSource>) -> Self {
            self.login_setup().random = random;
            self
        }

        /// The clock the guessing limit reads, to move it by hand.
        pub(super) fn clock(mut self, clock: Arc<dyn Clock>) -> Self {
            self.login_setup().clock = clock;
            self
        }

        /// The comparison of a token with the right one, to count its calls
        /// or slow it down.  Without it the real comparison is used.
        pub(super) fn comparer(mut self, compare: Comparer) -> Self {
            self.login_setup().compare = Some(compare);
            self
        }

        /// A smaller record broadcast channel, to make a client fall behind.
        pub(super) fn broadcast_capacity(mut self, capacity: usize) -> Self {
            self.broadcast_capacity = capacity;
            self
        }

        /// Serve this database instead of an in-memory one.
        pub(super) fn storage(mut self, storage: Storage) -> Self {
            self.storage = Some(storage);
            self
        }

        /// Leave the storage writer's queue to the test: no writer task is
        /// started, and the receiving end is in [`Rig::record_rx`].
        pub(super) fn without_record_writer(mut self) -> Self {
            self.record_writer = false;
            self
        }

        /// A lower label limit, to reach it with a handful of labels.
        pub(super) fn max_labels(mut self, max_labels: usize) -> Self {
            self.max_labels = max_labels;
            self
        }

        /// A smaller label event channel, to make a client fall behind.
        pub(super) fn labels_capacity(mut self, capacity: usize) -> Self {
            self.labels_capacity = capacity;
            self
        }

        /// Hold the label writer at a gate before each command is saved.
        pub(super) fn gated(mut self) -> Self {
            self.gated = true;
            self
        }

        pub(super) fn cors(mut self, origins: &[&str]) -> Self {
            self.cors_origins = origins.iter().map(|o| o.to_string()).collect();
            self
        }

        pub(super) fn hosts(mut self, hosts: &[&str]) -> Self {
            self.allowed_hosts = hosts.iter().map(|h| h.to_string()).collect();
            self
        }

        /// Needs a running tokio runtime: the label writer is a task.
        pub(super) fn build(self) -> Rig {
            let storage = self
                .storage
                .unwrap_or_else(|| Storage::open_in_memory().unwrap());
            let seen = SeenDevices::new();
            let (broadcast_tx, _) = broadcast::channel(self.broadcast_capacity);
            let (labels_tx, _) = broadcast::channel(self.labels_capacity);
            let cancel = CancellationToken::new();
            let gate = self.gated.then(Gate::closed);
            let writer = LabelWriter::spawn_for_test(
                storage.clone(),
                seen.clone(),
                LIVE_WINDOW,
                labels_tx.clone(),
                self.max_labels,
                gate.clone(),
                cancel.clone(),
            );
            // The storage writer's queue, as the sniffer builds it; the API
            // gets the weak end.
            let (record_tx, record_rx) = mpsc::channel(CHANNEL_CAPACITY);
            let write_failures = Arc::new(AtomicU64::new(0));
            let writer_cancel = CancellationToken::new();
            let record_rx = if self.record_writer {
                tokio::spawn(run_writer(
                    record_rx,
                    storage.clone(),
                    write_failures.clone(),
                    writer_cancel.clone(),
                ));
                None
            } else {
                Some(record_rx)
            };
            let login = self.login.map(|l| {
                Arc::new(
                    Auth::with_parts(&l.token, &l.secret, l.random, l.clock, l.compare)
                        .expect("the test's random source serves the login's setup"),
                )
            });
            let (router, state) = build_router(ApiContext {
                broadcast_tx: broadcast_tx.clone(),
                labels_tx: labels_tx.clone(),
                storage: storage.clone(),
                writer: writer.clone(),
                cors_origins: self.cors_origins,
                allowed_hosts: self.allowed_hosts,
                writer_queue: record_tx.downgrade(),
                write_failures: write_failures.clone(),
                login: login.clone(),
            });
            Rig {
                storage,
                seen,
                broadcast_tx,
                labels_tx,
                writer,
                cancel,
                gate,
                record_tx,
                record_rx,
                writer_cancel,
                write_failures,
                state,
                auth: login,
                router,
            }
        }
    }

    /// The API under test, with handles to the parts behind it.
    pub(super) struct Rig {
        pub(super) storage: Storage,
        pub(super) seen: SeenDevices,
        pub(super) broadcast_tx: broadcast::Sender<Arc<String>>,
        pub(super) labels_tx: broadcast::Sender<Arc<String>>,
        pub(super) writer: LabelWriter,
        pub(super) cancel: CancellationToken,
        pub(super) gate: Option<Arc<Gate>>,
        /// The proxy's end of the storage writer's queue.
        pub(super) record_tx: mpsc::Sender<WriterItem>,
        /// The writer's end, when the test runs the writer itself.
        pub(super) record_rx: Option<mpsc::Receiver<WriterItem>>,
        /// Stops the storage writer.
        pub(super) writer_cancel: CancellationToken,
        pub(super) write_failures: Arc<AtomicU64>,
        pub(super) state: AppState,
        /// The login, when the test turned it on.
        pub(super) auth: Option<Arc<Auth>>,
        router: Router,
    }

    impl Rig {
        /// The `Cookie` header value of a session the login would accept.
        pub(super) fn session_cookie(&self) -> String {
            let auth = self.auth.as_ref().expect("the test turned the login on");
            let session = auth.new_session(Utc::now()).unwrap();
            format!("{SESSION_COOKIE}={session}")
        }

        /// A handle to the router; clones share the same state.
        pub(super) fn app(&self) -> Router {
            self.router.clone()
        }
    }

    /// What came back from a request.
    #[derive(Debug)]
    pub(super) struct Reply {
        pub(super) status: StatusCode,
        pub(super) headers: HeaderMap,
        pub(super) body: Vec<u8>,
    }

    impl Reply {
        pub(super) fn json(&self) -> Value {
            serde_json::from_slice(&self.body)
                .unwrap_or_else(|e| panic!("not JSON ({e}): {:?}", self.text()))
        }

        pub(super) fn text(&self) -> String {
            String::from_utf8_lossy(&self.body).into_owned()
        }

        pub(super) fn header(&self, name: &str) -> Option<&str> {
            self.headers.get(name).and_then(|v| v.to_str().ok())
        }
    }

    /// Send one request through `app`.  Without a `host` header, and with a
    /// relative URI, the request is addressed to [`DEFAULT_HOST`].
    pub(super) async fn send(
        app: &Router,
        method: &str,
        uri: &str,
        headers: &[(&str, &str)],
        body: Option<&str>,
    ) -> Reply {
        let mut request = Request::builder().method(method).uri(uri);
        let names_host = headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("host"));
        if !names_host && uri.starts_with('/') {
            request = request.header("host", DEFAULT_HOST);
        }
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let body = body
            .map(|b| Body::from(b.to_string()))
            .unwrap_or_else(Body::empty);
        let response = app
            .clone()
            .oneshot(request.body(body).unwrap())
            .await
            .unwrap();
        let (parts, body) = response.into_parts();
        Reply {
            status: parts.status,
            headers: parts.headers,
            body: body.collect().await.unwrap().to_bytes().to_vec(),
        }
    }

    /// The keys every record has carried since the first release.
    const EXISTING_KEYS: [&str; 18] = [
        "id",
        "timestamp",
        "direction",
        "status",
        "blocked_by",
        "source_protocol",
        "freq_mhz",
        "datarate",
        "rssi",
        "snr",
        "crc_status",
        "mtype",
        "dev_addr",
        "dev_eui",
        "fcnt",
        "fport",
        "payload_size",
        "toa_ms",
    ];

    const NEW_KEYS: [&str; 10] = [
        "gateway_eui",
        "join_eui",
        "dev_nonce",
        "adr",
        "adr_ack_req",
        "ack",
        "class_b",
        "f_pending",
        "f_opts_len",
        "malformed",
    ];

    fn expected_keys() -> BTreeSet<String> {
        EXISTING_KEYS
            .iter()
            .chain(NEW_KEYS.iter())
            .map(|k| k.to_string())
            .collect()
    }

    fn keys_of(value: &Value) -> BTreeSet<String> {
        value.as_object().unwrap().keys().cloned().collect()
    }

    /// An uplink data frame record with every new field set.
    pub(super) fn record_with_new_fields() -> MessageRecord {
        MessageRecord {
            id: "rec-1".to_string(),
            timestamp: Utc::now(),
            direction: Direction::Up,
            status: Status::Captured,
            blocked_by: None,
            source_protocol: Protocol::Udp,
            freq_mhz: 868.1,
            datarate: "SF7BW125".to_string(),
            rssi: Some(-55),
            snr: Some(8.5),
            crc_status: Some(CrcStatus::Ok),
            mtype: MType::UnconfirmedDataUp,
            dev_addr: Some("01ABCDEF".to_string()),
            dev_eui: None,
            fcnt: Some(1234),
            fport: Some(1),
            payload_size: 23,
            toa_ms: Some(61.7),
            gateway_eui: Some("AA:BB:CC:DD:00:11:22:33".to_string()),
            join_eui: Some("00:00:00:00:00:00:00:01".to_string()),
            dev_nonce: Some(0x1234),
            adr: Some(true),
            adr_ack_req: Some(false),
            ack: Some(true),
            class_b: Some(false),
            f_pending: None,
            f_opts_len: Some(5),
            malformed: None,
        }
    }

    /// The DevAddr that [`frame_of_noise_dev_addr`] reads as.
    pub(super) const NOISE_DEV_ADDR: &str = "6874FA22";

    /// A 15-byte UnconfirmedDataUp frame whose bytes read as DevAddr
    /// `6874FA22`: the kind of fake identifier a frame with a failed CRC used
    /// to put in the table.
    pub(super) fn frame_of_noise_dev_addr() -> Vec<u8> {
        let mut frame = vec![0x40, 0x22, 0xFA, 0x74, 0x68]; // MHDR, DevAddr
        frame.push(0x00); // FCtrl
        frame.extend_from_slice(&[0x05, 0x00]); // FCnt
        frame.push(0x01); // FPort
        frame.extend_from_slice(&[0x11, 0x22]); // FRMPayload
        frame.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]); // MIC
        frame
    }

    /// The record a proxy builds for `frame` received with CRC status `crc`:
    /// the fields come from the decoder, as they do in the backends.
    pub(super) fn record_from_frame(frame: &[u8], crc: CrcStatus) -> MessageRecord {
        let crc = Some(crc);
        let fields = frame_fields(
            decoder::decode_phy_bytes(frame, crc.as_ref()),
            frame.len() as u16,
            crc.as_ref(),
        );
        MessageRecord {
            id: MessageRecord::new_id(),
            crc_status: crc,
            mtype: fields.mtype,
            dev_addr: fields.dev_addr,
            dev_eui: fields.dev_eui,
            join_eui: fields.join_eui,
            dev_nonce: fields.dev_nonce,
            fcnt: fields.fcnt,
            fport: fields.fport,
            payload_size: fields.payload_size,
            adr: fields.fctrl.map(|c| c.adr),
            adr_ack_req: fields.fctrl.and_then(|c| c.adr_ack_req),
            ack: fields.fctrl.map(|c| c.ack),
            class_b: fields.fctrl.and_then(|c| c.class_b),
            f_pending: fields.fctrl.and_then(|c| c.f_pending),
            f_opts_len: fields.fctrl.map(|c| c.f_opts_len),
            malformed: fields.malformed,
            ..record_with_new_fields()
        }
    }

    /// One record for each reason, built from frames carrying DevAddr
    /// `6874FA22` that the decoder rejects: a failed CRC, a reserved MHDR bit
    /// and a frame too short for its type.
    fn malformed_records() -> Vec<(Malformed, MessageRecord)> {
        let frame = frame_of_noise_dev_addr();
        let mut bad_mhdr = frame.clone();
        bad_mhdr[0] |= 0x04;
        let records = vec![
            (Malformed::Crc, record_from_frame(&frame, CrcStatus::Fail)),
            (Malformed::Mhdr, record_from_frame(&bad_mhdr, CrcStatus::Ok)),
            (
                Malformed::Length,
                record_from_frame(&frame[..9], CrcStatus::Ok),
            ),
        ];
        for (reason, record) in &records {
            assert_eq!(record.malformed, Some(*reason), "the decoder's verdict");
        }
        records
    }

    /// Every field the frame body would have filled is `null` in the JSON of a
    /// malformed record, and the reason is its text.
    fn assert_malformed_json(data: &Value, reason: Malformed, record: &MessageRecord) {
        assert_eq!(keys_of(data), expected_keys());
        assert_eq!(data["malformed"], json!(reason.as_str()));
        for key in [
            "dev_addr",
            "dev_eui",
            "join_eui",
            "dev_nonce",
            "fcnt",
            "fport",
            "adr",
            "adr_ack_req",
            "ack",
            "class_b",
            "f_pending",
            "f_opts_len",
        ] {
            assert_eq!(data[key], Value::Null, "{key}");
        }
        // What was measured about the reception stays.
        assert_eq!(data["mtype"], json!("UnconfirmedDataUp"));
        assert_eq!(data["freq_mhz"], json!(868.1));
        assert_eq!(data["rssi"], json!(-55));
        assert_eq!(data["payload_size"], json!(record.payload_size));
    }

    /// Assert the new fields of a serialized record match
    /// `record_with_new_fields`.
    fn assert_new_field_values(data: &Value) {
        assert_eq!(data["gateway_eui"], json!("AA:BB:CC:DD:00:11:22:33"));
        assert_eq!(data["join_eui"], json!("00:00:00:00:00:00:00:01"));
        assert_eq!(data["dev_nonce"], json!(0x1234));
        assert_eq!(data["adr"], json!(true));
        assert_eq!(data["adr_ack_req"], json!(false));
        assert_eq!(data["ack"], json!(true));
        assert_eq!(data["class_b"], json!(false));
        assert_eq!(data["f_pending"], Value::Null);
        assert_eq!(data["f_opts_len"], json!(5));
        // A valid record has no reason.
        assert_eq!(data["malformed"], Value::Null);
        // Existing fields are unchanged.
        assert_eq!(data["status"], json!("Captured"));
        assert_eq!(data["direction"], json!("up"));
        assert_eq!(data["mtype"], json!("UnconfirmedDataUp"));
        assert_eq!(data["dev_addr"], json!("01ABCDEF"));
        assert_eq!(data["fcnt"], json!(1234));
    }

    #[tokio::test]
    async fn messages_endpoint_returns_new_fields() {
        let rig = Setup::new().build();
        rig.storage.insert(&record_with_new_fields()).unwrap();
        let app = rig.app();

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/messages")
                    .header("host", DEFAULT_HOST)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let rows: Value = serde_json::from_slice(&body).unwrap();
        let rows = rows.as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(keys_of(&rows[0]), expected_keys());
        assert_eq!(rows[0]["id"], json!("rec-1"));
        assert_new_field_values(&rows[0]);
    }

    #[tokio::test]
    async fn messages_endpoint_returns_null_for_rows_without_new_fields() {
        // A row as an older release stored it: every new field is NULL.
        let mut record = record_with_new_fields();
        record.gateway_eui = None;
        record.join_eui = None;
        record.dev_nonce = None;
        record.adr = None;
        record.adr_ack_req = None;
        record.ack = None;
        record.class_b = None;
        record.f_opts_len = None;
        let rig = Setup::new().build();
        rig.storage.insert(&record).unwrap();
        let app = rig.app();

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/messages")
                    .header("host", DEFAULT_HOST)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let rows: Value = serde_json::from_slice(&body).unwrap();
        let row = &rows.as_array().unwrap()[0];
        assert_eq!(keys_of(row), expected_keys());
        for key in NEW_KEYS {
            assert_eq!(row[key], Value::Null, "{key}");
        }
    }

    #[test]
    fn ws_envelope_carries_new_fields() {
        let json = make_ws_json(&record_with_new_fields()).unwrap();
        let envelope: Value = serde_json::from_str(&json).unwrap();

        assert_eq!(
            keys_of(&envelope),
            BTreeSet::from(["type".to_string(), "data".to_string()])
        );
        assert_eq!(envelope["type"], json!("message"));
        assert_eq!(keys_of(&envelope["data"]), expected_keys());
        assert_new_field_values(&envelope["data"]);
    }

    #[tokio::test]
    async fn messages_endpoint_returns_the_reason_of_a_malformed_record_and_no_identifiers() {
        let rig = Setup::new().build();
        let records = malformed_records();
        for (_, record) in &records {
            rig.storage.insert(record).unwrap();
        }
        let app = rig.app();

        let reply = send(&app, "GET", "/api/messages", &[], None).await;
        assert_eq!(reply.status, StatusCode::OK);
        let rows = reply.json();
        let rows = rows.as_array().unwrap();
        assert_eq!(rows.len(), records.len());
        for (reason, record) in &records {
            let row = rows
                .iter()
                .find(|r| r["id"] == json!(record.id))
                .unwrap_or_else(|| panic!("no row for {reason:?}"));
            assert_malformed_json(row, *reason, record);
        }
        // The fake identifier is nowhere in the response.
        assert!(!reply.text().contains(NOISE_DEV_ADDR), "{}", reply.text());
    }

    #[test]
    fn ws_envelope_carries_the_reason_of_a_malformed_record_and_no_identifiers() {
        for (reason, record) in malformed_records() {
            let json = make_ws_json(&record).unwrap();
            let envelope: Value = serde_json::from_str(&json).unwrap();

            assert_eq!(envelope["type"], json!("message"));
            assert_malformed_json(&envelope["data"], reason, &record);
            assert!(!json.contains(NOISE_DEV_ADDR), "{json}");
        }
    }

    // -- cross-origin access ------------------------------------------------

    fn app_with(cors_origins: &[&str]) -> Router {
        Setup::new().cors(cors_origins).build().app()
    }

    /// A GET addressed to [`DEFAULT_HOST`] unless `headers` says otherwise.
    async fn get_with(
        app: Router,
        uri: &str,
        headers: &[(&str, &str)],
    ) -> axum::response::Response {
        let mut request = Request::builder().uri(uri);
        if !headers.iter().any(|(name, _)| *name == "host") {
            request = request.header("host", DEFAULT_HOST);
        }
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        app.oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, value.parse().unwrap());
        }
        map
    }

    #[tokio::test]
    async fn api_sends_no_cors_headers_by_default() {
        let response = get_with(
            app_with(&[]),
            "/api/messages",
            &[("origin", "http://evil.example")],
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        assert!(response
            .headers()
            .get("access-control-allow-origin")
            .is_none());
    }

    #[tokio::test]
    async fn api_allows_only_configured_origins() {
        let allowed = get_with(
            app_with(&["https://ui.example"]),
            "/api/messages",
            &[("origin", "https://ui.example")],
        )
        .await;
        let other = get_with(
            app_with(&["https://ui.example"]),
            "/api/messages",
            &[("origin", "http://evil.example")],
        )
        .await;

        assert_eq!(
            allowed
                .headers()
                .get("access-control-allow-origin")
                .unwrap(),
            "https://ui.example"
        );
        assert!(other.headers().get("access-control-allow-origin").is_none());
    }

    #[tokio::test]
    async fn ws_refuses_a_foreign_origin() {
        let response = get_with(
            app_with(&[]),
            "/ws",
            &[("host", "sniffer:8888"), ("origin", "http://evil.example")],
        )
        .await;

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn origin_rules() {
        let none: &[String] = &[];
        let listed = ["https://ui.example/".to_string()];
        let path = Uri::from_static("/ws");
        let allowed = |pairs: &[(&'static str, &str)], list: &[String]| {
            origin_allowed(&headers(pairs), &path, list)
        };

        // Not a browser.
        assert!(allowed(&[("host", "lns01:8888")], none));
        // The sniffer's own pages, any case.
        assert!(allowed(
            &[("host", "lns01:8888"), ("origin", "http://lns01:8888")],
            none
        ));
        assert!(allowed(
            &[("host", "LNS01:8888"), ("origin", "http://lns01:8888")],
            none
        ));
        assert!(allowed(
            &[("host", "[::1]:8888"), ("origin", "http://[::1]:8888")],
            none
        ));
        // Another site, another port of the same host, or a sandboxed page.
        assert!(!allowed(
            &[("host", "lns01:8888"), ("origin", "http://evil.example")],
            none
        ));
        assert!(!allowed(
            &[("host", "lns01:8888"), ("origin", "http://lns01:9999")],
            none
        ));
        assert!(!allowed(
            &[("host", "lns01:8888"), ("origin", "null")],
            none
        ));
        assert!(!allowed(&[("origin", "http://lns01:8888")], none));
        // Listed in cors_origins (a trailing slash in the setting is fine).
        assert!(allowed(
            &[("host", "127.0.0.1:8888"), ("origin", "https://ui.example")],
            &listed
        ));
    }

    /// HTTP/2 has no `Host` header: the authority is in the request URI.
    #[test]
    fn origin_rules_use_the_uri_authority_without_a_host_header() {
        let none: &[String] = &[];
        let uri = Uri::from_static("https://lns01:8888/ws/labels");
        let allowed = |pairs: &[(&'static str, &str)]| origin_allowed(&headers(pairs), &uri, none);

        assert!(allowed(&[("origin", "https://lns01:8888")]));
        assert!(allowed(&[("origin", "https://LNS01:8888")]));
        assert!(!allowed(&[("origin", "https://evil.example")]));
        assert!(!allowed(&[("origin", "https://lns01:9999")]));
        // Without an authority anywhere there is nothing to compare with.
        let relative = Uri::from_static("/ws/labels");
        assert!(!origin_allowed(
            &headers(&[("origin", "https://lns01:8888")]),
            &relative,
            none
        ));
        // A Host header, when there is one, is what counts.
        assert!(!origin_allowed(
            &headers(&[("host", "lns01:8888"), ("origin", "https://evil.example")]),
            &uri,
            none
        ));
    }

    /// The full handshake over a real socket: the in-process test service
    /// cannot upgrade a connection, so this is where a successful handshake
    /// is proven.
    #[tokio::test]
    async fn ws_handshake_over_tcp() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app_with(&[])).await.unwrap();
        });

        async fn status_of(addr: std::net::SocketAddr, origin: Option<String>) -> String {
            let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
            let origin = origin
                .map(|o| format!("Origin: {o}\r\n"))
                .unwrap_or_default();
            let request = format!(
                "GET /ws HTTP/1.1\r\nHost: {addr}\r\nUpgrade: websocket\r\n\
                 Connection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
                 Sec-WebSocket-Version: 13\r\n{origin}\r\n"
            );
            stream.write_all(request.as_bytes()).await.unwrap();
            let mut buf = [0u8; 64];
            let n = stream.read(&mut buf).await.unwrap();
            let text = String::from_utf8_lossy(&buf[..n]).to_string();
            text.lines().next().unwrap_or_default().to_string()
        }

        let own = status_of(addr, Some(format!("http://{addr}"))).await;
        let foreign = status_of(addr, Some("http://evil.example".to_string())).await;
        let no_origin = status_of(addr, None).await;

        assert!(own.contains(" 101 "), "{own}");
        assert!(foreign.contains(" 403 "), "{foreign}");
        assert!(no_origin.contains(" 101 "), "{no_origin}");
    }
}
