use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use chrono::Utc;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::core::filter::FilterRule;
use crate::core::metadata::MessageRecord;
use crate::core::storage::Storage;

use crate::api;
use crate::config::{self, Configuration};
use crate::proxy;

/// How often the retention task sweeps the database for expired rows.
const RETENTION_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Buffered capacity of the record channel (records/messages).
/// LoRaWAN rates are << 1000/s, so 1024 is plenty.
const CHANNEL_CAPACITY: usize = 1024;

pub async fn run(config: &Configuration) -> Result<()> {
    info!("Starting LoRaWAN Packet Sniffer v{}", config::VERSION);

    // -----------------------------------------------------------------------
    // Filter rules
    // -----------------------------------------------------------------------
    let filters = build_filters(config);
    info!("Loaded {} filter rule(s)", filters.len());

    // -----------------------------------------------------------------------
    // Storage
    // -----------------------------------------------------------------------
    let storage = Storage::open(&config.sniffer.database.path).map_err(|e| {
        anyhow::anyhow!(
            "Failed to open database at '{}': {}",
            config.sniffer.database.path,
            e
        )
    })?;
    info!("Database opened at '{}'", config.sniffer.database.path);

    // Channel: proxy tasks → storage writer
    let (record_tx, record_rx) = mpsc::channel::<MessageRecord>(CHANNEL_CAPACITY);

    // Broadcast hub: proxy tasks → WebSocket clients (pre-serialised JSON).
    // Capacity 512: at LoRaWAN rates this is never hit; slow WS clients lag.
    let (broadcast_tx, _broadcast_rx) = broadcast::channel::<Arc<String>>(512);

    let cancel = CancellationToken::new();

    // -----------------------------------------------------------------------
    // Storage writer task
    //
    // Receives MessageRecords and writes them to SQLite via spawn_blocking.
    // -----------------------------------------------------------------------
    let writer_storage = storage.clone();
    let writer_cancel = cancel.clone();
    tokio::spawn(async move {
        run_writer(record_rx, writer_storage, writer_cancel).await;
    });

    // -----------------------------------------------------------------------
    // API server task (HTTP + WebSocket)
    // -----------------------------------------------------------------------
    let api_cancel = cancel.clone();
    let api_bind = config.sniffer.api.bind.clone();
    let api_cors = config.sniffer.api.cors_origins.clone();
    let api_broadcast = broadcast_tx.clone();
    let api_storage = storage.clone();
    tokio::spawn(async move {
        run_api(api_broadcast, api_storage, api_bind, api_cors, api_cancel).await;
    });

    // -----------------------------------------------------------------------
    // Retention cleanup task
    //
    // Periodically deletes rows older than the configured TTL.
    // -----------------------------------------------------------------------
    let retention_storage = storage.clone();
    let retention = config.sniffer.database.retention;
    let retention_cancel = cancel.clone();
    tokio::spawn(async move {
        run_retention(retention_storage, retention, retention_cancel).await;
    });

    // -----------------------------------------------------------------------
    // Proxy
    // -----------------------------------------------------------------------
    match config.gateway.protocol {
        config::Protocol::Udp => {
            info!(
                "Gateway protocol: UDP  downstream={} upstream={}:{}",
                config.gateway.udp.downstream.bind,
                config.gateway.udp.upstream.server,
                config.gateway.udp.upstream.port,
            );

            tokio::select! {
                result = proxy::udp::run(
                    &config.gateway.udp,
                    &filters,
                    record_tx,
                    broadcast_tx,
                    cancel.clone(),
                ) => {
                    result?;
                }
                _ = tokio::signal::ctrl_c() => {
                    warn!("Received SIGINT, shutting down");
                    cancel.cancel();
                }
            }
        }
        config::Protocol::Concentratord => {
            info!(
                "Gateway protocol: Concentratord  downstream event={} cmd={}, upstream event={} cmd={}",
                config.gateway.concentratord.downstream.event_url,
                config.gateway.concentratord.downstream.command_url,
                config.gateway.concentratord.upstream.event_bind,
                config.gateway.concentratord.upstream.command_bind,
            );

            tokio::select! {
                result = proxy::concentratord::run(
                    &config.gateway.concentratord,
                    &filters,
                    record_tx,
                    broadcast_tx,
                    cancel.clone(),
                ) => {
                    result?;
                }
                _ = tokio::signal::ctrl_c() => {
                    warn!("Received SIGINT, shutting down");
                    cancel.cancel();
                }
            }
        }
    }

    info!("Sniffer stopped");
    Ok(())
}

// ---------------------------------------------------------------------------
// Background tasks
// ---------------------------------------------------------------------------

/// Receive `MessageRecord`s and persist them to SQLite.
async fn run_writer(
    mut rx: mpsc::Receiver<MessageRecord>,
    storage: Storage,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            Some(record) = rx.recv() => {
                let s = storage.clone();
                match tokio::task::spawn_blocking(move || s.insert(&record)).await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => error!("Storage write error: {}", e),
                    Err(e)    => error!("Storage writer task panicked: {}", e),
                }
            }
            _ = cancel.cancelled() => {
                // Drain remaining records before exiting.
                while let Ok(record) = rx.try_recv() {
                    let s = storage.clone();
                    let _ = tokio::task::spawn_blocking(move || s.insert(&record)).await;
                }
                debug!("Storage writer task stopped");
                break;
            }
        }
    }
}

/// Periodically delete rows older than `retention` from the database.
async fn run_retention(storage: Storage, retention: Duration, cancel: CancellationToken) {
    let mut interval = tokio::time::interval(RETENTION_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = interval.tick() => {
                let s = storage.clone();
                let cutoff = Utc::now()
                    - chrono::Duration::from_std(retention)
                        .unwrap_or(chrono::Duration::days(30));

                match tokio::task::spawn_blocking(move || s.delete_older_than(cutoff)).await {
                    Ok(Ok(0)) => {}
                    Ok(Ok(n)) => info!("Retention: deleted {} expired message(s)", n),
                    Ok(Err(e)) => error!("Retention cleanup error: {}", e),
                    Err(e)    => error!("Retention task panicked: {}", e),
                }
            }
            _ = cancel.cancelled() => {
                debug!("Retention task stopped");
                break;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// API server task
// ---------------------------------------------------------------------------

async fn run_api(
    broadcast_tx: broadcast::Sender<Arc<String>>,
    storage: Storage,
    bind: String,
    cors_origins: Vec<String>,
    cancel: CancellationToken,
) {
    let app = api::router(broadcast_tx, storage, cors_origins);
    let listener = match tokio::net::TcpListener::bind(&bind).await {
        Ok(l) => {
            info!("API server listening on http://{}", bind);
            l
        }
        Err(e) => {
            error!("API server failed to bind '{}': {}", bind, e);
            return;
        }
    };

    tokio::select! {
        result = axum::serve(listener, app) => {
            if let Err(e) = result {
                error!("API server error: {}", e);
            }
        }
        _ = cancel.cancelled() => {
            debug!("API server stopped");
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn build_filters(config: &Configuration) -> Vec<FilterRule> {
    config
        .filters
        .iter()
        .map(|f| FilterRule {
            name: f.name.clone(),
            enabled: f.enabled,
            direction: f.direction.clone(),
            suppress_ack: f.suppress_ack,
            mtypes: f.mtype.clone(),
        })
        .collect()
}
