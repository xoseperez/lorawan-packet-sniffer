//! HTTP + WebSocket API server (axum).
//!
//! ## Endpoints
//!
//! | Method | Path           | Description                              |
//! |--------|----------------|------------------------------------------|
//! | GET    | `/ws`          | WebSocket — real-time message feed       |
//! | GET    | `/api/messages`| REST — historical messages (query params) |
//! | GET    | `/*`           | Serve embedded React SPA                |
//!
//! ## WebSocket protocol
//!
//! Server → client:
//! ```json
//! { "type": "message", "data": { ...MessageRecord... } }
//! ```
//!
//! Client → server: ignored for now.
//!
//! ## `/api/messages` query params
//!
//! - `since`: ISO-8601 timestamp — return only messages after this time (asc)
//! - `limit`: max rows (default 500)

use std::sync::Arc;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    http::{header, StatusCode, Uri},
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use chrono::{DateTime, Utc};
use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tower_http::cors::CorsLayer;

use crate::core::{metadata::MessageRecord, storage::Storage};

// ---------------------------------------------------------------------------
// Embedded frontend assets
// ---------------------------------------------------------------------------

/// Embed the contents of `../webui/dist/` into the binary at compile time.
/// Build the webui first: `cd v2/webui && npm run build`
#[derive(RustEmbed)]
#[folder = "../webui/dist"]
struct Frontend;

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct AppState {
    broadcast_tx: broadcast::Sender<Arc<String>>,
    storage: Storage,
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router(
    broadcast_tx: broadcast::Sender<Arc<String>>,
    storage: Storage,
    cors_origins: Vec<String>,
) -> Router {
    let state = AppState { broadcast_tx, storage };

    let cors = if cors_origins.is_empty() {
        CorsLayer::permissive()
    } else {
        let origins: Vec<axum::http::HeaderValue> =
            cors_origins.iter().filter_map(|o| o.parse().ok()).collect();
        CorsLayer::new().allow_origin(origins)
    };

    Router::new()
        .route("/ws", get(ws_handler))
        .route("/api/messages", get(messages_handler))
        .fallback(frontend_handler)
        .layer(cors)
        .with_state(state)
}

// ---------------------------------------------------------------------------
// WebSocket handler
// ---------------------------------------------------------------------------

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state.broadcast_tx.subscribe()))
}

async fn handle_socket(mut socket: WebSocket, mut rx: broadcast::Receiver<Arc<String>>) {
    loop {
        tokio::select! {
            result = rx.recv() => {
                match result {
                    Ok(json) => {
                        let text = (*json).clone();
                        if socket.send(Message::Text(text)).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!("WebSocket client lagged {} messages", n);
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

// ---------------------------------------------------------------------------
// REST: GET /api/messages
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct MessagesParams {
    /// Return messages strictly after this ISO-8601 timestamp (ascending).
    since: Option<String>,
    /// Maximum number of rows (default 500).
    limit: Option<i64>,
}

/// Returned JSON is a plain array of `MessageRecord` objects.
async fn messages_handler(
    State(state): State<AppState>,
    Query(params): Query<MessagesParams>,
) -> impl IntoResponse {
    let since: Option<DateTime<Utc>> = params
        .since
        .as_deref()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc));

    let limit = params.limit.unwrap_or(500).clamp(1, 5000);
    let storage = state.storage.clone();

    let result = tokio::task::spawn_blocking(move || {
        if let Some(since) = since {
            storage.query_since(since, limit)
        } else {
            storage.query_recent(limit)
        }
    })
    .await;

    match result {
        Ok(Ok(records)) => Json(records).into_response(),
        Ok(Err(e)) => {
            error!("Messages query error: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
        Err(e) => {
            error!("Messages query task panicked: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// Static file serving (embedded React SPA)
// ---------------------------------------------------------------------------

async fn frontend_handler(uri: Uri) -> axum::response::Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    // Try exact path, then fall back to index.html for SPA client-side routing.
    serve_embedded(path).unwrap_or_else(|| {
        serve_embedded("index.html")
            .unwrap_or_else(|| StatusCode::NOT_FOUND.into_response())
    })
}

fn serve_embedded(path: &str) -> Option<axum::response::Response> {
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
// WebSocket envelope (re-exported for use by the proxy layer)
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
    let env = Envelope { kind: "message", data: record };
    serde_json::to_string(&env).ok().map(Arc::new)
}
