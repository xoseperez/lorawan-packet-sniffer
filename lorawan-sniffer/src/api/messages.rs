//! `GET /api/messages`: the history of captured messages.
//!
//! The parameters, headers and errors are listed in the documentation of the
//! parent module.  This file holds what makes the endpoint safe to call from
//! many tabs at once against a large database:
//!
//! - **One query at a time.**  Queries take turns through a one-permit
//!   semaphore.  The permit moves into the blocking task that runs the query,
//!   so it is released when SQLite has stopped, not when the request goes
//!   away: a client that gives up mid-query never lets a second blocking task
//!   pile up behind the first.  A request that is dropped (its client
//!   disconnected) sets a flag that the query polls, and the query stops.
//! - **Three large answers in memory at most.**  A full answer is several
//!   megabytes of JSON, held until a slow client has taken it.  The permit
//!   for one is taken before the query and travels in the response body, so
//!   it comes back when the body is dropped, sent or abandoned.
//! - **A marker for a reload that must not miss anything** (`settle=true`),
//!   queued behind the records the storage writer has yet to save.

use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{Query, State};
use axum::http::{header::HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{DateTime, Utc};
use http_body::{Frame, SizeHint};
use serde::de::IntoDeserializer;
use serde::{Deserialize, Serialize};
use tokio::sync::{oneshot, OwnedSemaphorePermit};

use super::{json_error, AppState};
use crate::core::error::SnifferError;
use crate::core::history::{
    HistoryFilter, HistoryPage, HistoryQuery, LowerBound, MAX_HISTORY_ROWS,
};
use crate::core::linelog::error_line;
use crate::core::metadata::{MType, Status};
use crate::core::writer::WriterItem;

/// How long a request with `settle=true` waits, in all, to get its marker
/// into the writer queue and to have it answered.  When the time runs out
/// the request is answered with what is saved, and says so.
pub(super) const SETTLE_BUDGET: Duration = Duration::from_secs(5);

/// The number of records that match, when `count=true`.
pub(super) const TOTAL_COUNT: HeaderName = HeaderName::from_static("x-total-count");
/// The number of malformed records among those that match every other
/// condition, when `count=true` and it could be counted.
pub(super) const MALFORMED_COUNT: HeaderName = HeaderName::from_static("x-malformed-count");
/// Whether the storage writer had processed everything queued before the
/// request, when `settle=true`.
pub(super) const HISTORY_SETTLED: HeaderName = HeaderName::from_static("x-history-settled");
/// How many inserts have failed since the sniffer started.
pub(super) const STORAGE_WRITE_FAILURES: HeaderName =
    HeaderName::from_static("x-storage-write-failures");

/// The query string of `GET /api/messages`.
///
/// `since` and `limit` keep their types: a `limit` that is not a number is
/// refused by axum's own `Query` extractor, as it always was.  The other
/// parameters are text so that a bad value gets the JSON error below.
#[derive(Deserialize)]
pub(super) struct MessagesParams {
    since: Option<String>,
    limit: Option<i64>,
    from: Option<String>,
    newest: Option<String>,
    count: Option<String>,
    device: Option<String>,
    mtype: Option<String>,
    status: Option<String>,
    gateway: Option<String>,
    malformed: Option<String>,
    settle: Option<String>,
}

/// A request, understood.
#[derive(Debug)]
struct Request {
    query: HistoryQuery,
    /// Wait for the storage writer before reading.
    settle: bool,
}

/// Turn the query string into a request.  `Err` is the message of a 400.
fn parse(params: &MessagesParams) -> Result<Request, String> {
    // An unparsable `since` is ignored, as it always was.
    let since = params
        .since
        .as_deref()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc));

    let from = match params.from.as_deref() {
        None => None,
        Some(text) => Some(
            DateTime::parse_from_rfc3339(text)
                .map(|dt| dt.with_timezone(&Utc))
                .map_err(|_| {
                    "from must be an RFC 3339 timestamp, such as 2026-10-30T12:00:00.000Z"
                        .to_string()
                })?,
        ),
    };
    let lower = match (since, from) {
        (Some(_), Some(_)) => return Err("use either from or since, not both".to_string()),
        (Some(since), None) => Some(LowerBound::After(since)),
        (None, Some(from)) => Some(LowerBound::From(from)),
        (None, None) => None,
    };

    // Without a bound the newest records are the useful ones; with one, the
    // oldest after it are, which is how `since` has always worked.  The
    // default follows the bound that was understood, so that a `since` that
    // was ignored still returns the newest records.
    let newest = flag(params.newest.as_deref(), "newest")?.unwrap_or(lower.is_none());
    let count = flag(params.count.as_deref(), "count")?.unwrap_or(false);
    let settle = flag(params.settle.as_deref(), "settle")?.unwrap_or(false);

    let hide_malformed = match params.malformed.as_deref() {
        None | Some("include") => false,
        Some("exclude") => true,
        Some(_) => return Err("malformed must be include or exclude".to_string()),
    };

    let filter = HistoryFilter {
        device: params.device.clone().filter(|d| !d.is_empty()),
        mtypes: parse_mtypes(params.mtype.as_deref().unwrap_or(""))?,
        status: parse_status(params.status.as_deref().unwrap_or(""))?,
        gateway: params.gateway.clone().filter(|g| !g.is_empty()),
        hide_malformed,
    };

    Ok(Request {
        query: HistoryQuery {
            lower,
            filter,
            limit: params
                .limit
                .unwrap_or(500)
                .clamp(1, MAX_HISTORY_ROWS as i64) as usize,
            newest,
            count,
        },
        settle,
    })
}

/// `"true"` or `"false"`; `None` when the parameter is absent.
fn flag(value: Option<&str>, name: &str) -> Result<Option<bool>, String> {
    match value {
        None => Ok(None),
        Some("true") => Ok(Some(true)),
        Some("false") => Ok(Some(false)),
        Some(_) => Err(format!("{name} must be true or false")),
    }
}

/// A comma-separated list of message type names, as records spell them.
fn parse_mtypes(list: &str) -> Result<Vec<MType>, String> {
    if list.is_empty() {
        return Ok(Vec::new());
    }
    list.split(',')
        .map(|name| {
            let name: serde::de::value::StrDeserializer<'_, serde::de::value::Error> =
                name.into_deserializer();
            MType::deserialize(name).map_err(|_| {
                format!(
                    "unknown message type in mtype: use names such as \
                     UnconfirmedDataUp, JoinRequest or Unknown, separated by commas \
                     (got {list:?})"
                )
            })
        })
        .collect()
}

/// A status in any case, or `all`, or nothing.
fn parse_status(text: &str) -> Result<Option<Status>, String> {
    match text.to_ascii_lowercase().as_str() {
        "" | "all" => Ok(None),
        "forwarded" => Ok(Some(Status::Forwarded)),
        "blocked" => Ok(Some(Status::Blocked)),
        "captured" => Ok(Some(Status::Captured)),
        _ => Err("status must be all, forwarded, blocked or captured".to_string()),
    }
}

/// Sets a flag when it is dropped.
///
/// The handler owns one, so when axum drops the handler because its client
/// went away, the flag the query polls is set and the query stops.
pub(super) struct CancelOnDrop(Arc<AtomicBool>);

impl CancelOnDrop {
    pub(super) fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    pub(super) fn flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.0)
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// How much of a response body is handed to the HTTP server at a time.
const BODY_CHUNK: usize = 64 * 1024;

/// A response body that keeps a permit until it is dropped.
///
/// The JSON is handed over in pieces copied out of the buffer.  hyper asks
/// for the next piece only when its write buffer has room (a few hundred KiB),
/// so a client that reads slowly keeps this body, and the permit, until it
/// has taken nearly all of it.  Handed over in one piece, the whole buffer
/// would sit in hyper's write buffer and the body would be dropped at once,
/// giving the permit back before a byte was sent; slow clients would then
/// each hold a full response with nothing to limit how many.  Copying each
/// piece lets the buffer itself be freed with the permit.
struct HeldBody {
    data: Vec<u8>,
    sent: usize,
    _permit: OwnedSemaphorePermit,
}

impl http_body::Body for HeldBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let start = self.sent;
        if start >= self.data.len() {
            return Poll::Ready(None);
        }
        let end = (start + BODY_CHUNK).min(self.data.len());
        let piece = Bytes::copy_from_slice(&self.data[start..end]);
        self.sent = end;
        Poll::Ready(Some(Ok(Frame::data(piece))))
    }

    fn is_end_stream(&self) -> bool {
        self.sent >= self.data.len()
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact((self.data.len() - self.sent) as u64)
    }
}

/// How the wait for the storage writer ended.
enum Wait {
    /// Every item queued before the marker has been processed.
    Done,
    /// The queue is closed or the writer is gone: the sniffer is stopping, or
    /// the writer died.  There is nothing left to wait for.
    Stopped,
}

/// Queue a marker behind what the writer has yet to save, and wait for its
/// answer.  Part of one budget: see [`SETTLE_BUDGET`].
async fn queue_marker(state: &AppState) -> Wait {
    // Only one marker may be in the queue.  The permit goes into the marker
    // and comes back when the writer (or the shutdown drain) consumes it,
    // not when this request stops waiting, so a request that timed out and
    // asked again cannot add a second marker behind a stalled writer.
    let Ok(permit) = state.history_settle.clone().acquire_owned().await else {
        return Wait::Stopped;
    };
    // A weak handle: the API must not keep the queue open once the proxy,
    // which holds the strong ones, has stopped.
    let Some(queue) = state.writer_queue.upgrade() else {
        return Wait::Stopped;
    };
    let (done, answered) = oneshot::channel();
    // If the time runs out while this waits for room in a full queue, the
    // item, permit included, is dropped with the future, and nothing is
    // queued.  If the queue is closed, the item comes back in the error and
    // is dropped there.
    if queue
        .send(WriterItem::Settle { done, permit })
        .await
        .is_err()
    {
        return Wait::Stopped;
    }
    // Do not hold the queue open while waiting.
    drop(queue);
    match answered.await {
        Ok(()) => Wait::Done,
        Err(_) => Wait::Stopped,
    }
}

/// Wait for the storage writer, within the budget.  `true` when it has
/// processed everything that was queued before this request.
async fn wait_for_writer(state: &AppState) -> bool {
    match tokio::time::timeout(SETTLE_BUDGET, queue_marker(state)).await {
        Ok(Wait::Done) => true,
        Ok(Wait::Stopped) => {
            state.settle_log.warn(format_args!(
                "Storage writer stopped; history served without waiting for it"
            ));
            false
        }
        Err(_) => {
            state.settle_log.warn(format_args!(
                "Storage writer is behind; history served without waiting for it"
            ));
            false
        }
    }
}

/// The 500 for a request the sniffer failed to serve.
fn server_error() -> Response {
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}

/// A 503 with the machine-readable `code` the WebUI acts on.
fn unavailable(message: &str, code: &str) -> Response {
    #[derive(Serialize)]
    struct Unavailable<'a> {
        error: &'a str,
        code: &'a str,
    }
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(Unavailable {
            error: message,
            code,
        }),
    )
        .into_response()
}

/// `GET /api/messages`: a JSON array of `MessageRecord`s, oldest first.
pub(super) async fn messages_handler(
    State(state): State<AppState>,
    Query(params): Query<MessagesParams>,
) -> Response {
    let request = match parse(&params) {
        Ok(request) => request,
        Err(message) => return json_error(StatusCode::BAD_REQUEST, message),
    };

    // Dropped with this future if the client goes away, whichever step the
    // request has reached.
    let cancel = CancelOnDrop::new();

    // Before the places in memory and the turn for the query, so that a
    // request without `settle` never waits for a writer that is behind, and
    // so that requests that do are not held back by the three places: all of
    // them wait together, within the same few seconds.  Nothing is held
    // while they wait but the one marker.
    let settled = if request.settle {
        Some(wait_for_writer(&state).await)
    } else {
        None
    };

    // The answer's place among the large ones in memory, kept until the
    // client has taken it.
    let Ok(body_permit) = state.history_bodies.clone().acquire_owned().await else {
        return server_error();
    };

    let Ok(turn) = state.history.clone().acquire_owned().await else {
        return server_error();
    };
    let storage = state.storage.clone();
    let flag = cancel.flag();
    let query = request.query;
    // The turn lives in the blocking task, not in this future: if this future
    // is dropped, the query is still running until it sees the flag, and the
    // next query must not start before it has stopped.
    let result = tokio::task::spawn_blocking(move || {
        let _turn = turn;
        let result = storage.query_history(&query, flag);
        // Logged here, not where this handler matches the result: a client
        // that went away drops the handler, which is what sets the flag, so
        // the handler never sees the outcome.  This task always does.
        if matches!(result, Err(SnifferError::Cancelled)) {
            debug!("History query cancelled: the client went away");
        }
        result
    })
    .await;

    match result {
        Ok(Ok(page)) => answer(page, settled, body_permit),
        Ok(Err(SnifferError::HistoryUnavailable)) => unavailable(
            "history filters need the database in WAL mode; see the log",
            "no_wal",
        ),
        Ok(Err(SnifferError::Busy)) => unavailable("the database is busy; try again", "busy"),
        Ok(Err(SnifferError::Cancelled)) => server_error(),
        Ok(Err(e)) => {
            error_line!("Messages query error: {}", e);
            server_error()
        }
        Err(e) => {
            error_line!("Messages query task panicked: {}", e);
            server_error()
        }
    }
}

/// The 200: the records and the headers that describe them.
fn answer(page: HistoryPage, settled: Option<bool>, body_permit: OwnedSemaphorePermit) -> Response {
    let data = match serde_json::to_vec(&page.records) {
        Ok(data) => data,
        Err(e) => {
            error_line!("Messages serialisation error: {}", e);
            return server_error();
        }
    };
    let mut response = Response::new(Body::new(HeldBody {
        data,
        sent: 0,
        _permit: body_permit,
    }));
    let headers = response.headers_mut();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    if let Some(total) = page.total {
        headers.insert(TOTAL_COUNT, HeaderValue::from(total));
    }
    if let Some(malformed) = page.malformed {
        headers.insert(MALFORMED_COUNT, HeaderValue::from(malformed));
    }
    if let Some(settled) = settled {
        headers.insert(
            HISTORY_SETTLED,
            HeaderValue::from_static(if settled { "true" } else { "false" }),
        );
    }
    response
}

/// Add `X-Storage-Write-Failures` to a response of `/api/messages`.
///
/// It is a layer on the route and not a line in the handler, because axum's
/// own rejection of a query string (`limit=abc`) is produced before the
/// handler runs.  The value is read when the response leaves the route, which
/// for a request that waited for a marker is after the marker was answered, so
/// it includes every failure queued before that request.
pub(super) async fn add_failure_header(
    State(failures): State<Arc<AtomicU64>>,
    mut response: Response,
) -> Response {
    response.headers_mut().insert(
        STORAGE_WRITE_FAILURES,
        HeaderValue::from(failures.load(Ordering::Relaxed)),
    );
    response
}
