//! MQTT output.
//!
//! Records leave the proxy through [`crate::proxy::Outputs`], which hands a
//! copy to the MQTT publisher over a bounded queue.  [`start`] spawns the two
//! tasks behind that queue and the counters in [`MqttStats`] let the rest of
//! the program account for what happened to every copy.
//!
//! # Delivery
//!
//! Publishing is best effort and never slows the proxy down.  Each record is
//! handed to the broker at most once: while the broker is connected and
//! keeping up every record arrives exactly once, and in any other state the
//! record is dropped and counted rather than held back.
//!
//! # Why the client's queue needs a policy of its own
//!
//! `rumqttc` keeps a second queue, [`EventLoop::pending`].  When an
//! established connection fails it moves everything that was in flight, and
//! everything still waiting in its request channel, into that queue so it can
//! be resent on the next connection.  Left alone that is a hidden buffer:
//! bounded only as long as every reconnect starts a clean session, invisible
//! to any counter, and a source of stale records arriving long after they
//! happened.  So this module:
//!
//! - pins clean sessions, so the broker never replays anything either;
//! - stops handing records to the client while the connection is down (the
//!   publisher drops them first), so the request channel cannot fill up
//!   during an outage;
//! - after every connection error counts what `pending` holds as dropped and
//!   empties it.
//!
//! Together these bound the messages held inside the client to its request
//! channel ([`REQUEST_CAP`]) plus the in-flight window, however long the
//! broker is away.
//!
//! # Secrets
//!
//! `rumqttc`'s `Debug` output for its options prints the username and
//! password, so nothing here formats the options (or the credentials) with
//! `{:?}`.  Log lines name the broker as `host:port` and nothing else from
//! the configuration.

pub mod topic;

mod tls;

#[cfg(test)]
mod tests;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use rumqttc::{
    AsyncClient, Event, EventLoop, Incoming, MqttOptions, Outgoing, QoS, TlsConfiguration,
    Transport,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::config::MqttConfig;
use crate::core::metadata::MessageRecord;
use crate::core::ratelimit::RateLimitedLog;

use self::topic::TopicTemplate;

/// Capacity of the client's request channel: the most records that can wait
/// there for the event loop to write them.
const REQUEST_CAP: usize = 256;

/// Capacity of the queue between [`crate::proxy::Outputs`] and the publisher.
const RECORD_QUEUE_CAP: usize = 1024;

/// First wait before reconnecting; doubles on every consecutive failure.
const MIN_BACKOFF: Duration = Duration::from_secs(1);

/// Longest wait between reconnection attempts.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Counters describing what the MQTT output did with the records it was given.
///
/// Shared between the producer side (which counts records it could not queue)
/// and the publisher (which counts what it sent and what it had to discard),
/// so every field is an atomic and the struct is passed around in an `Arc`.
///
/// For QoS 0, once traffic settles, `published + dropped` equals the number
/// of records offered.  For QoS 1 and 2 a message that was written but not yet
/// acknowledged when the connection failed counts as both, because the
/// client forgets it and it is not resent; the figures are then an upper
/// bound on loss, not an exact tally.
#[derive(Default, Debug)]
pub struct MqttStats {
    /// Records written to the broker connection.
    pub published: AtomicU64,
    /// Records refused or discarded before being sent: the queue was full or
    /// closed, or the broker connection was down.
    pub dropped: AtomicU64,
    /// Largest number of messages seen waiting inside the MQTT client at once.
    pub pending_high_water: AtomicU64,
    /// Limits the warning logged when the publisher discards a record because
    /// the broker is not connected.  Kept next to the counters, not inside the
    /// publisher task, so tests can compare the lines it let through with
    /// `dropped`.
    pub(crate) offline_drops: RateLimitedLog,
    /// Limits the warning logged when a record cannot be encoded or the
    /// client refuses it.  Kept here for the same reason as `offline_drops`.
    pub(crate) refused_drops: RateLimitedLog,
}

/// Timing knobs that tests shorten; production uses [`Tuning::default`].
#[derive(Debug, Clone, Copy)]
struct Tuning {
    /// Wait after the first connection failure.
    min_backoff: Duration,
    /// Ceiling for the wait between attempts.
    max_backoff: Duration,
}

impl Default for Tuning {
    fn default() -> Self {
        Tuning {
            min_backoff: MIN_BACKOFF,
            max_backoff: MAX_BACKOFF,
        }
    }
}

/// Start the MQTT publisher.
///
/// Returns the queue to offer records to (see
/// [`crate::proxy::Outputs::with_mqtt`]) and the counters.  An invalid topic
/// template, QoS or TLS setup is an error here, so it stops the sniffer at
/// startup; an unreachable broker is not: the connection is retried for as
/// long as the sniffer runs.
///
/// Must be called from within a tokio runtime.  Both tasks stop when `cancel`
/// is cancelled.
pub fn start(
    cfg: &MqttConfig,
    cancel: CancellationToken,
) -> Result<(mpsc::Sender<MessageRecord>, Arc<MqttStats>)> {
    let (tx, stats, _connected) = start_with(cfg, cancel, Tuning::default())?;
    Ok((tx, stats))
}

/// [`start`] with adjustable timing, and the connection flag exposed so tests
/// can wait for the broker to be reached.
fn start_with(
    cfg: &MqttConfig,
    cancel: CancellationToken,
    tuning: Tuning,
) -> Result<(mpsc::Sender<MessageRecord>, Arc<MqttStats>, Arc<AtomicBool>)> {
    let topic: TopicTemplate = cfg
        .topic
        .parse()
        .with_context(|| format!("invalid MQTT topic '{}'", cfg.topic))?;
    let qos = qos_from(cfg.qos)?;

    let mut options = MqttOptions::new(cfg.client_id.clone(), cfg.server.clone(), cfg.port);
    // Pinned rather than configurable: see the module documentation.
    options.set_clean_session(true);
    if !cfg.username.is_empty() {
        options.set_credentials(cfg.username.clone(), cfg.password.clone());
    }
    if cfg.tls {
        let tls_config = tls::client_config(cfg).context("invalid MQTT TLS settings")?;
        options.set_transport(Transport::tls_with_config(TlsConfiguration::Rustls(
            Arc::new(tls_config),
        )));
    }

    let broker = format!("{}:{}", cfg.server, cfg.port);
    info!(
        "MQTT output enabled: broker {} ({}), topic '{}', QoS {}",
        broker,
        if cfg.tls { "TLS" } else { "no TLS" },
        cfg.topic,
        cfg.qos
    );

    let (client, eventloop) = AsyncClient::new(options, REQUEST_CAP);
    let (tx, rx) = mpsc::channel(RECORD_QUEUE_CAP);
    let stats = Arc::new(MqttStats::default());
    let connected = Arc::new(AtomicBool::new(false));

    tokio::spawn(run_event_loop(
        eventloop,
        broker,
        Arc::clone(&connected),
        Arc::clone(&stats),
        tuning,
        cancel.clone(),
    ));
    tokio::spawn(run_publisher(
        rx,
        client,
        topic,
        qos,
        Arc::clone(&connected),
        Arc::clone(&stats),
        cancel,
    ));

    Ok((tx, stats, connected))
}

fn qos_from(level: u8) -> Result<QoS> {
    match level {
        0 => Ok(QoS::AtMostOnce),
        1 => Ok(QoS::AtLeastOnce),
        2 => Ok(QoS::ExactlyOnce),
        other => bail!("MQTT qos must be 0, 1 or 2, not {}", other),
    }
}

/// Drive the connection: poll the client forever, reconnecting with backoff.
///
/// `rumqttc` does the network work only while it is polled, and polling also
/// reconnects after an error, which is why this loop never returns on one.
async fn run_event_loop(
    mut eventloop: EventLoop,
    broker: String,
    connected: Arc<AtomicBool>,
    stats: Arc<MqttStats>,
    tuning: Tuning,
    cancel: CancellationToken,
) {
    let failures = RateLimitedLog::new();
    let mut backoff = tuning.min_backoff;

    loop {
        let polled = tokio::select! {
            _ = cancel.cancelled() => break,
            polled = eventloop.poll() => polled,
        };

        match polled {
            Ok(Event::Incoming(Incoming::ConnAck(_))) => {
                backoff = tuning.min_backoff;
                connected.store(true, Ordering::Release);
                info!(
                    "MQTT connected to {} ({} published, {} dropped so far)",
                    broker,
                    stats.published.load(Ordering::Relaxed),
                    stats.dropped.load(Ordering::Relaxed)
                );
            }
            // Emitted for QoS 0 as well, when the packet is written to the
            // connection, which is the nearest thing to "sent" a QoS 0
            // message has.
            Ok(Event::Outgoing(Outgoing::Publish(_))) => {
                stats.published.fetch_add(1, Ordering::Relaxed);
            }
            Ok(_) => {}
            Err(e) => {
                // Stop the publisher feeding the client before touching its
                // queue, so nothing new lands in it while it is emptied.
                connected.store(false, Ordering::Release);

                // The client moved whatever was in flight or queued into
                // `pending` to resend after reconnecting.  Resending would
                // deliver stale records and keep the buffer alive across a
                // flapping connection, so count them as lost and let go.
                let queued = eventloop.pending.len() as u64;
                stats
                    .pending_high_water
                    .fetch_max(queued, Ordering::Relaxed);
                stats.dropped.fetch_add(queued, Ordering::Relaxed);
                eventloop.pending.clear();

                failures.warn(format_args!(
                    "MQTT broker {} unavailable: {}; retrying in {:?} ({} record(s) discarded \
                     from the client queue)",
                    broker, e, backoff, queued
                ));

                let delay = backoff;
                backoff = backoff.saturating_mul(2).min(tuning.max_backoff);
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = tokio::time::sleep(delay) => {}
                }
            }
        }
    }

    connected.store(false, Ordering::Release);
    debug!("MQTT event loop stopped");
}

/// Turn queued records into MQTT publishes.
///
/// A record is handed to the client only while the connection is up.  The
/// client's own queue is bounded, but during an outage it would keep the
/// first records it was given and fail every later one, and what it kept
/// would be sent minutes late; dropping up front is simpler and the counters
/// stay exact.
async fn run_publisher(
    mut rx: mpsc::Receiver<MessageRecord>,
    client: AsyncClient,
    topic: TopicTemplate,
    qos: QoS,
    connected: Arc<AtomicBool>,
    stats: Arc<MqttStats>,
    cancel: CancellationToken,
) {
    loop {
        let record = tokio::select! {
            _ = cancel.cancelled() => break,
            record = rx.recv() => match record {
                Some(record) => record,
                None => break,
            },
        };

        if !connected.load(Ordering::Acquire) {
            stats.dropped.fetch_add(1, Ordering::Relaxed);
            stats
                .offline_drops
                .warn("MQTT broker not connected, dropping record");
            continue;
        }

        let payload = match serde_json::to_vec(&record) {
            Ok(payload) => payload,
            Err(e) => {
                stats.dropped.fetch_add(1, Ordering::Relaxed);
                stats
                    .refused_drops
                    .warn(format_args!("cannot encode record as JSON: {}", e));
                continue;
            }
        };

        if let Err(e) = client.try_publish(topic.render(&record), qos, false, payload) {
            stats.dropped.fetch_add(1, Ordering::Relaxed);
            stats.refused_drops.warn(format_args!(
                "MQTT client queue refused a record, dropping it: {}",
                e
            ));
        }
    }

    debug!("MQTT publisher stopped");
}
