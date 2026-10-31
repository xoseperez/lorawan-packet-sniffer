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
    let state = AppState {
        broadcast_tx,
        storage,
    };

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

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
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
        serve_embedded("index.html").unwrap_or_else(|| StatusCode::NOT_FOUND.into_response())
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
    let env = Envelope {
        kind: "message",
        data: record,
    };
    serde_json::to_string(&env).ok().map(Arc::new)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use serde_json::{json, Value};
    use tower::ServiceExt;

    use super::*;
    use crate::core::metadata::{CrcStatus, Direction, MType, Protocol, Status};

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

    const NEW_KEYS: [&str; 9] = [
        "gateway_eui",
        "join_eui",
        "dev_nonce",
        "adr",
        "adr_ack_req",
        "ack",
        "class_b",
        "f_pending",
        "f_opts_len",
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
    fn record_with_new_fields() -> MessageRecord {
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
        }
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
        // Existing fields are unchanged.
        assert_eq!(data["status"], json!("Captured"));
        assert_eq!(data["direction"], json!("up"));
        assert_eq!(data["mtype"], json!("UnconfirmedDataUp"));
        assert_eq!(data["dev_addr"], json!("01ABCDEF"));
        assert_eq!(data["fcnt"], json!(1234));
    }

    #[tokio::test]
    async fn messages_endpoint_returns_new_fields() {
        let storage = Storage::open_in_memory().unwrap();
        storage.insert(&record_with_new_fields()).unwrap();
        let (tx, _rx) = broadcast::channel(4);
        let app = router(tx, storage, vec![]);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/messages")
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
        let storage = Storage::open_in_memory().unwrap();
        storage.insert(&record).unwrap();
        let (tx, _rx) = broadcast::channel(4);
        let app = router(tx, storage, vec![]);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/messages")
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
}
