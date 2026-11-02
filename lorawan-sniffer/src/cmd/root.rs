use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use serde::Deserialize;
use tokio::signal::unix::{signal, Signal, SignalKind};
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::core::error::SnifferError;
use crate::core::eui::Eui64;
use crate::core::filter::FilterRule;
use crate::core::linelog::error_line;
use crate::core::metadata::MessageRecord;
use crate::core::ratelimit::RateLimitedLog;
use crate::core::seen::SeenDevices;
use crate::core::storage::Storage;
use crate::core::writer::WriterItem;

use crate::api::labels::{LabelWriter, WriterError};
use crate::api::{self, ApiContext};
use crate::config::{self, Configuration, UdpMode};
use crate::mqtt;
use crate::proxy;

/// Longest wait between two retention sweeps.
const MAX_SWEEP_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Shortest wait between two retention sweeps.  Also keeps `interval` away
/// from a zero period, which it rejects.
const MIN_SWEEP_INTERVAL: Duration = Duration::from_secs(1);

/// Buffered capacity of the label event channel.  Labels change when a person
/// edits one or the retention sweep removes some, so a client only falls
/// behind when a sweep removes many at once, and a client that does gets the
/// full set again instead of the events it missed.
const LABELS_CHANNEL_CAPACITY: usize = 64;

/// Buffered capacity of the record channel (records/messages).
/// LoRaWAN rates are << 1000/s, so 1024 is plenty.
pub(crate) const CHANNEL_CAPACITY: usize = 1024;

/// How long a stop request waits for the storage writer to save the records
/// still queued.  Long enough for a full channel on a slow SD card, short
/// enough to stay well inside the stop timeouts of systemd and Docker.
const WRITER_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// The two ways the sniffer is asked to stop: Ctrl-C in a terminal (SIGINT),
/// and SIGTERM from `systemctl stop`, `docker stop` or `kill`.
///
/// Both handlers are installed when this is created, at the very start of
/// `run`: until then SIGTERM would kill the process outright, and under
/// Docker, where the sniffer is PID 1, the kernel drops any signal that has
/// no handler, so `docker stop` would wait out its timeout and then kill it.
struct StopSignals {
    interrupt: Signal,
    terminate: Signal,
}

impl StopSignals {
    fn install() -> Result<Self> {
        Ok(Self {
            interrupt: signal(SignalKind::interrupt()).context("cannot handle SIGINT")?,
            terminate: signal(SignalKind::terminate()).context("cannot handle SIGTERM")?,
        })
    }

    /// Wait for either signal and return its name.
    async fn recv(&mut self) -> &'static str {
        tokio::select! {
            _ = self.interrupt.recv() => "SIGINT",
            _ = self.terminate.recv() => "SIGTERM",
        }
    }
}

pub async fn run(config: &Configuration) -> Result<()> {
    let mut stop = StopSignals::install()?;
    info!("Starting LoRaWAN Packet Sniffer v{}", config::VERSION);

    // -----------------------------------------------------------------------
    // Filter rules
    // -----------------------------------------------------------------------
    let filters = build_filters(config)?;
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

    // Channel: proxy tasks → storage writer.  The API keeps a weak handle to
    // queue markers on, so that it never holds the queue open once the proxy
    // has stopped.
    let (record_tx, record_rx) = mpsc::channel::<WriterItem>(CHANNEL_CAPACITY);
    let writer_queue = record_tx.downgrade();

    // Inserts that did not save, since the sniffer started.  The writer counts
    // them and the API reports the count.
    let write_failures = Arc::new(AtomicU64::new(0));

    // Broadcast hub: proxy tasks → WebSocket clients (pre-serialised JSON).
    // Capacity 512: at LoRaWAN rates this is never hit; slow WS clients lag.
    // The first receiver feeds the live device list (see `run_seen_listener`);
    // like any other receiver it can only lag, never slow the senders down.
    let (broadcast_tx, seen_rx) = broadcast::channel::<Arc<String>>(512);

    // Label changes → `/ws/labels` clients (pre-serialised JSON).
    let (labels_tx, _) = broadcast::channel::<Arc<String>>(LABELS_CHANNEL_CAPACITY);

    // Everywhere a captured record goes, behind one non-blocking `emit`.
    let outputs = proxy::Outputs::new(record_tx, broadcast_tx.clone());

    let cancel = CancellationToken::new();

    // -----------------------------------------------------------------------
    // Live device list and label writer
    //
    // The list records which device identifiers have been seen in live
    // traffic, so that a device whose records the storage writer dropped can
    // still be labelled.  It is fed from the record broadcast, off the
    // forwarding path.  The label writer is the one task that changes labels.
    // -----------------------------------------------------------------------
    let retention = config.sniffer.database.retention;
    let seen = SeenDevices::new();
    tokio::spawn(run_seen_listener(seen_rx, seen.clone(), cancel.clone()));
    let label_writer = LabelWriter::spawn(
        storage.clone(),
        seen.clone(),
        retention,
        labels_tx.clone(),
        cancel.clone(),
    );

    // MQTT publisher: the third consumer of every record, when enabled.
    let outputs = if config.sniffer.mqtt.enabled {
        let (mqtt_tx, mqtt_stats) = mqtt::start(&config.sniffer.mqtt, cancel.clone())?;
        outputs.with_mqtt(mqtt_tx, mqtt_stats)
    } else {
        outputs
    };

    // -----------------------------------------------------------------------
    // Storage writer task
    //
    // Receives MessageRecords and writes them to SQLite via spawn_blocking.
    // -----------------------------------------------------------------------
    let writer_storage = storage.clone();
    let writer_cancel = cancel.clone();
    let writer_failures = write_failures.clone();
    let writer = tokio::spawn(async move {
        run_writer(record_rx, writer_storage, writer_failures, writer_cancel).await;
    });

    // -----------------------------------------------------------------------
    // API server task (HTTP + WebSocket)
    // -----------------------------------------------------------------------
    let api_cancel = cancel.clone();
    let api_bind = config.sniffer.api.bind.clone();
    let api_context = ApiContext {
        broadcast_tx: broadcast_tx.clone(),
        labels_tx,
        storage: storage.clone(),
        writer: label_writer.clone(),
        cors_origins: config.sniffer.api.cors_origins.clone(),
        allowed_hosts: config.sniffer.api.allowed_hosts.clone(),
        writer_queue,
        write_failures,
    };
    tokio::spawn(async move {
        run_api(api_context, api_bind, api_cancel).await;
    });

    // -----------------------------------------------------------------------
    // Retention cleanup task
    //
    // Periodically deletes rows older than the configured TTL, and the labels
    // of devices that have disappeared.
    // -----------------------------------------------------------------------
    let retention_cancel = cancel.clone();
    tokio::spawn(async move {
        run_retention(label_writer, seen, retention, retention_cancel).await;
    });

    // -----------------------------------------------------------------------
    // Proxy
    // -----------------------------------------------------------------------
    match config.gateway.protocol {
        config::Protocol::Udp => {
            let udp = &config.gateway.udp;
            let upstream = match udp.mode {
                UdpMode::Proxy => {
                    info!(
                        "Gateway protocol: UDP  mode=proxy  downstream={} upstream={}:{}",
                        udp.downstream.bind, udp.upstream.server, udp.upstream.port,
                    );
                    Some(resolve_upstream(udp).await?)
                }
                UdpMode::Listen => {
                    info!(
                        "Gateway protocol: UDP  mode=listen  downstream={}",
                        udp.downstream.bind,
                    );
                    None
                }
            };
            if let Some(warning) = listen_filters_warning(config) {
                warn!("{}", warning);
            }

            tokio::select! {
                result = proxy::udp::run(
                    udp,
                    upstream,
                    &filters,
                    outputs,
                    cancel.clone(),
                ) => {
                    result?;
                }
                name = stop.recv() => {
                    info!("Received {}, shutting down", name);
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
                    outputs,
                    cancel.clone(),
                ) => {
                    result?;
                }
                name = stop.recv() => {
                    info!("Received {}, shutting down", name);
                    cancel.cancel();
                }
            }
        }
    }

    // The proxy has stopped and dropped its side of the record channel.  Wait
    // for the writer to save what is still queued: returning straight away
    // would end the runtime and lose the last records captured before the
    // stop.  A stuck disk must not hold the shutdown forever, hence the limit.
    cancel.cancel();
    if tokio::time::timeout(WRITER_DRAIN_TIMEOUT, writer)
        .await
        .is_err()
    {
        warn!(
            "Queued records not saved within {}s; some may be lost",
            WRITER_DRAIN_TIMEOUT.as_secs()
        );
    }

    info!("Sniffer stopped");
    Ok(())
}

// ---------------------------------------------------------------------------
// Background tasks
// ---------------------------------------------------------------------------

/// Take items from the writer queue, one at a time, and persist them to
/// SQLite.
///
/// Each item is finished before the next is taken, which is what gives a
/// marker its meaning: when it is answered, everything queued before it has
/// been processed, saved or counted in `failures` (see [`WriterItem`]).  The
/// count goes up before the next item is taken, so before any later marker is
/// answered, and whoever reads it after that answer sees it.
pub(crate) async fn run_writer(
    rx: mpsc::Receiver<WriterItem>,
    storage: Storage,
    failures: Arc<AtomicU64>,
    cancel: CancellationToken,
) {
    process_items(rx, move |record| storage.insert(&record), failures, cancel).await;
}

/// [`run_writer`] with the insert passed in, so that tests can make it fail,
/// panic or wait.
pub(crate) async fn process_items<F>(
    mut rx: mpsc::Receiver<WriterItem>,
    insert: F,
    failures: Arc<AtomicU64>,
    cancel: CancellationToken,
) where
    F: Fn(MessageRecord) -> Result<(), SnifferError> + Clone + Send + Sync + 'static,
{
    loop {
        // A stop is looked at first, so that a queue that never empties
        // cannot keep the writer from draining and exiting.
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                // Drain remaining items before exiting.  A marker still in
                // the queue is answered like any other, so that a request
                // waiting for it does not wait out its time.
                while let Ok(item) = rx.try_recv() {
                    process_item(item, &insert, &failures).await;
                }
                debug!("Storage writer task stopped");
                break;
            }
            Some(item) = rx.recv() => {
                process_item(item, &insert, &failures).await;
            }
        }
    }
}

/// Save one record, or answer one marker.
async fn process_item<F>(item: WriterItem, insert: &F, failures: &AtomicU64)
where
    F: Fn(MessageRecord) -> Result<(), SnifferError> + Clone + Send + Sync + 'static,
{
    match item {
        WriterItem::Record(record) => {
            let insert = insert.clone();
            match tokio::task::spawn_blocking(move || insert(record)).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    error_line!("Storage write error: {}", e);
                    failures.fetch_add(1, Ordering::Relaxed);
                }
                Err(e) => {
                    error_line!("Storage writer task panicked: {}", e);
                    failures.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        WriterItem::Settle { done, permit } => {
            // The requester may have given up; the answer then goes nowhere.
            let _ = done.send(());
            drop(permit);
        }
    }
}

/// How long to wait between retention sweeps.
///
/// Half the retention period, so expired rows never linger for much more
/// than a period and a short retention (as in tests and demonstrations) is
/// pruned promptly, but never more often than once a second and never less
/// often than every five minutes.  For any retention of ten minutes or more
/// that is the five minutes the sweep has always used.
fn sweep_interval(retention: Duration) -> Duration {
    (retention / 2).clamp(MIN_SWEEP_INTERVAL, MAX_SWEEP_INTERVAL)
}

/// Periodically delete rows older than `retention` from the database, and
/// the labels of devices that are gone.
async fn run_retention(
    writer: LabelWriter,
    seen: SeenDevices,
    retention: Duration,
    cancel: CancellationToken,
) {
    let started = Instant::now();
    let mut interval = tokio::time::interval(sweep_interval(retention));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = interval.tick() => {
                retention_tick(&writer, &seen, retention, started.elapsed()).await;
            }
            _ = cancel.cancelled() => {
                debug!("Retention task stopped");
                break;
            }
        }
    }
}

/// One retention sweep.  `uptime` is how long the sniffer has been running.
///
/// Labels are judged against stored records and the live list, and the live
/// list starts empty at every start.  A label whose device simply has not
/// been heard since the start would look abandoned, so nothing is removed
/// until the sniffer has run for a whole retention period: that gives every
/// label's device that long to be stored or seen again.  The grace period
/// restarts with each start, so a restart never removes a label.
async fn retention_tick(
    writer: &LabelWriter,
    seen: &SeenDevices,
    retention: Duration,
    uptime: Duration,
) {
    let forgotten = seen.expire(retention);
    let cutoff =
        Utc::now() - chrono::Duration::from_std(retention).unwrap_or(chrono::Duration::days(30));
    let keep_orphans = uptime < retention;

    match writer.prune(cutoff, keep_orphans).await {
        Ok(pruned) if pruned.messages == 0 && pruned.labels.is_empty() && forgotten == 0 => {}
        Ok(pruned) => info!(
            "Retention: deleted {} expired message(s) and {} label(s); \
             forgot {} device(s) not seen live, {} still tracked",
            pruned.messages,
            pruned.labels.len(),
            forgotten,
            seen.len()
        ),
        // Only while the sniffer is stopping.
        Err(WriterError::Stopped) => debug!("Retention sweep skipped: the label writer stopped"),
        Err(e) => error!("Retention cleanup error: {}", e),
    }
}

// ---------------------------------------------------------------------------
// Live device list
// ---------------------------------------------------------------------------

/// The parts of a record envelope (`{"type":"message","data":{...}}`) that
/// say which device it came from.  Everything else in it is skipped.
#[derive(Deserialize)]
struct Sighting {
    data: SightingData,
}

#[derive(Deserialize)]
struct SightingData {
    dev_addr: Option<String>,
    dev_eui: Option<String>,
}

/// Note every device identifier that goes by on the record broadcast.
///
/// This is how a device the storage writer dropped can still be labelled, and
/// it stays off the forwarding path: a broadcast receiver never makes senders
/// wait, it only falls behind.  When that happens the missed records are
/// gone, a rate-limited warning says so, and the listener carries on; a
/// device that was missed is noted again with its next record.
pub(crate) async fn run_seen_listener(
    mut rx: broadcast::Receiver<Arc<String>>,
    seen: SeenDevices,
    cancel: CancellationToken,
) {
    let lag_log = RateLimitedLog::new();
    loop {
        tokio::select! {
            result = rx.recv() => match result {
                Ok(envelope) => note_sighting(&seen, &envelope),
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    lag_log.warn(format_args!(
                        "Live device tracking fell behind and skipped {} record(s); \
                         those devices can be labelled again once they are next seen",
                        missed
                    ));
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            _ = cancel.cancelled() => break,
        }
    }
    debug!("Live device tracking stopped");
}

/// Note the identifier(s) in one record envelope.  A record has a DevAddr or
/// a DevEUI, never both, and records without either (join accepts, for one)
/// are not a device that can be labelled.
fn note_sighting(seen: &SeenDevices, envelope: &str) {
    match serde_json::from_str::<Sighting>(envelope) {
        Ok(sighting) => {
            for device in [sighting.data.dev_addr, sighting.data.dev_eui]
                .into_iter()
                .flatten()
            {
                seen.note(&device);
            }
        }
        Err(e) => debug!("Live device tracking skipped a record envelope: {}", e),
    }
}

// ---------------------------------------------------------------------------
// API server task
// ---------------------------------------------------------------------------

async fn run_api(context: ApiContext, bind: String, cancel: CancellationToken) {
    let app = api::router(context);
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

/// Resolve the LNS address once and make sure it does not lead back to the
/// sniffer's own gateway-facing socket.
///
/// Every datagram the sniffer forwarded to such an address would arrive again
/// as if a gateway had sent it, so this refuses to start instead.  Returns the
/// address the proxy uses; see [`pick_upstream`].
async fn resolve_upstream(udp: &config::UdpConfig) -> Result<SocketAddr> {
    let server = &udp.upstream.server;
    let port = udp.upstream.port;

    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((server.as_str(), port))
        .await
        .with_context(|| format!("cannot resolve the UDP upstream server '{server}:{port}'"))?
        .collect();
    let chosen = pick_upstream(&addrs).ok_or_else(|| {
        anyhow!("the UDP upstream server '{server}:{port}' resolves to no address")
    })?;
    if addrs.len() > 1 {
        info!(
            "UDP upstream '{server}' resolves to {}; using {chosen}",
            addrs
                .iter()
                .map(|a| a.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    let bind: SocketAddr = udp.downstream.bind.parse().with_context(|| {
        format!(
            "the UDP downstream bind '{}' is not an IP address and port",
            udp.downstream.bind
        )
    })?;
    proxy::udp::loopguard::check_startup(bind, &addrs).map_err(|e| anyhow!(e))?;

    Ok(chosen)
}

/// The address to forward to when the upstream name resolves to several.
///
/// The first IPv4 address wins.  Names such as `localhost` usually resolve to
/// `::1` first, but network servers and packet multiplexers commonly listen
/// on `0.0.0.0`, which is IPv4 only: forwarding to `::1` would then reach
/// nothing, silently.  A listener that accepts IPv6 almost always accepts
/// IPv4 as well, so IPv4 is the choice that works in both cases.  A name with
/// only IPv6 addresses uses its first one.
fn pick_upstream(addrs: &[SocketAddr]) -> Option<SocketAddr> {
    addrs
        .iter()
        .find(|a| a.is_ipv4())
        .or_else(|| addrs.first())
        .copied()
}

/// A warning for a configuration that has filter rules but cannot apply them:
/// a listening UDP sniffer forwards nothing, so it never blocks anything.
fn listen_filters_warning(config: &Configuration) -> Option<String> {
    let listening = config.gateway.protocol == config::Protocol::Udp
        && config.gateway.udp.mode == UdpMode::Listen;
    if !listening || config.filters.is_empty() {
        return None;
    }
    Some(format!(
        "{} filter rule(s) configured, but the UDP gateway is in listen mode: \
         nothing is forwarded, so no rule can block anything and every message \
         is recorded as captured",
        config.filters.len()
    ))
}

/// Compile the configured filter rules.
///
/// Fails on a gateway EUI that does not parse: a typo there would otherwise
/// leave the rule matching no gateway at all, and the traffic it was meant to
/// block would flow without any sign that the rule is dead.
fn build_filters(config: &Configuration) -> Result<Vec<FilterRule>> {
    config
        .filters
        .iter()
        .enumerate()
        .map(|(index, f)| {
            let gateways = f
                .gateways
                .iter()
                .map(|value| {
                    value.parse::<Eui64>().map_err(|e| {
                        anyhow!(
                            "filter rule #{} ('{}'): bad gateway: {}",
                            index + 1,
                            f.name,
                            e
                        )
                    })
                })
                .collect::<Result<Vec<_>>>()?;

            Ok(FilterRule {
                name: f.name.clone(),
                enabled: f.enabled,
                direction: f.direction.clone(),
                suppress_ack: f.suppress_ack,
                mtypes: f.mtype.clone(),
                gateways,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::json;
    use tokio::time::timeout;

    use super::*;
    use crate::core::labels::MAX_LABELS;

    fn config_from(toml_text: &str) -> Configuration {
        toml::from_str(toml_text).unwrap()
    }

    fn addrs(list: &[&str]) -> Vec<SocketAddr> {
        list.iter().map(|a| a.parse().unwrap()).collect()
    }

    #[test]
    fn upstream_prefers_ipv4() {
        // What `localhost` resolves to on a typical Debian host.
        assert_eq!(
            pick_upstream(&addrs(&["[::1]:1718", "127.0.0.1:1718"])),
            Some("127.0.0.1:1718".parse().unwrap())
        );
        assert_eq!(
            pick_upstream(&addrs(&[
                "[2001:db8::1]:1700",
                "192.0.2.1:1700",
                "192.0.2.2:1700"
            ])),
            Some("192.0.2.1:1700".parse().unwrap())
        );
    }

    #[test]
    fn upstream_falls_back_to_ipv6() {
        assert_eq!(
            pick_upstream(&addrs(&["[2001:db8::1]:1700", "[2001:db8::2]:1700"])),
            Some("[2001:db8::1]:1700".parse().unwrap())
        );
        assert_eq!(pick_upstream(&[]), None);
    }

    /// End to end through the resolver: `localhost` must end up on IPv4
    /// whatever order the system returns its addresses in.
    #[tokio::test]
    async fn localhost_upstream_resolves_to_ipv4() {
        let udp = config_from(
            r#"
[gateway.udp]
  [gateway.udp.downstream]
    bind = "0.0.0.0:1717"
  [gateway.udp.upstream]
    server = "localhost"
    port   = 1718
"#,
        )
        .gateway
        .udp;

        let chosen = resolve_upstream(&udp).await.unwrap();

        assert_eq!(chosen, "127.0.0.1:1718".parse::<SocketAddr>().unwrap());
    }

    #[test]
    fn gateway_euis_in_filters_are_parsed() {
        let config = config_from(
            r#"
            [[filters]]
              name = "only these"
              enabled = true
              gateways = ["AA:BB:CC:DD:00:11:22:33", "58a0cbfffe801234", "01-02-03-04-05-06-07-08"]
            "#,
        );

        let rules = build_filters(&config).unwrap();

        assert_eq!(rules.len(), 1);
        assert_eq!(
            rules[0].gateways,
            vec![
                Eui64([0xAA, 0xBB, 0xCC, 0xDD, 0x00, 0x11, 0x22, 0x33]),
                Eui64([0x58, 0xA0, 0xCB, 0xFF, 0xFE, 0x80, 0x12, 0x34]),
                Eui64([0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08]),
            ]
        );
    }

    #[test]
    fn filters_without_gateways_apply_to_all() {
        let config = config_from(
            r#"
            [[filters]]
              name = "everyone"
              enabled = true
              mtype = ["JoinRequest"]
            "#,
        );

        let rules = build_filters(&config).unwrap();

        assert!(rules[0].gateways.is_empty());
        assert_eq!(config.filters[0].gateways, Vec::<String>::new());
    }

    #[test]
    fn invalid_gateway_eui_stops_startup() {
        let config = config_from(
            r#"
            [[filters]]
              name = "fine"
              enabled = true
              gateways = ["AA:BB:CC:DD:00:11:22:33"]

            [[filters]]
              name = "typo"
              enabled = true
              gateways = ["AA:BB:CC:DD:00:11:22:33", "AA:BB:CC:DD:00:11:22"]
            "#,
        );

        let err = build_filters(&config).unwrap_err().to_string();

        // The message names both the rule and the value that did not parse.
        assert!(err.contains("typo"), "{err}");
        assert!(err.contains("AA:BB:CC:DD:00:11:22"), "{err}");
        assert!(err.contains("#2"), "{err}");
    }

    #[test]
    fn listen_filters_warning_when_rules_set() {
        let rules = r#"
            [[filters]]
              name = "join"
              enabled = true
              mtype = ["JoinRequest"]

            [[filters]]
              name = "other"
              enabled = false
        "#;
        let listen = "[gateway.udp]\n mode = \"listen\"\n";

        // Listen mode with rules: warn, and say how many rules are idle.
        let warning = listen_filters_warning(&config_from(&format!("{listen}{rules}")))
            .expect("rules in listen mode must warn");
        assert!(warning.contains("2 filter rule(s)"), "{warning}");
        assert!(warning.contains("listen mode"), "{warning}");

        // Listen mode without rules: nothing to warn about.
        assert_eq!(listen_filters_warning(&config_from(listen)), None);

        // Proxy mode, which is the default, applies its rules.
        assert_eq!(listen_filters_warning(&config_from(rules)), None);
        let proxy = format!("[gateway.udp]\n mode = \"proxy\"\n{rules}");
        assert_eq!(listen_filters_warning(&config_from(&proxy)), None);

        // The UDP mode means nothing to Concentratord.
        let concentratord = format!(
            "[gateway]\n protocol = \"concentratord\"\n[gateway.udp]\n mode = \"listen\"\n{rules}"
        );
        assert_eq!(listen_filters_warning(&config_from(&concentratord)), None);
    }

    #[test]
    fn invalid_gateway_eui_in_a_disabled_rule_still_stops_startup() {
        // Disabling a rule must not hide its typo until someone re-enables it.
        let config = config_from(
            r#"
            [[filters]]
              name = "off"
              enabled = false
              gateways = ["nonsense"]
            "#,
        );

        assert!(build_filters(&config).is_err());
    }

    // -- retention sweep ----------------------------------------------------

    #[test]
    fn sweep_interval_is_half_the_retention() {
        assert_eq!(
            sweep_interval(Duration::from_secs(4)),
            Duration::from_secs(2)
        );
        assert_eq!(
            sweep_interval(Duration::from_secs(120)),
            Duration::from_secs(60)
        );
        assert_eq!(
            sweep_interval(Duration::from_secs(9 * 60)),
            Duration::from_secs(270)
        );
    }

    #[test]
    fn sweep_interval_is_never_below_a_second() {
        assert_eq!(
            sweep_interval(Duration::from_secs(2)),
            Duration::from_secs(1)
        );
        assert_eq!(
            sweep_interval(Duration::from_secs(1)),
            Duration::from_secs(1)
        );
        assert_eq!(
            sweep_interval(Duration::from_millis(100)),
            Duration::from_secs(1)
        );
        // tokio's `interval` panics on a zero period.
        assert_eq!(sweep_interval(Duration::ZERO), Duration::from_secs(1));
    }

    #[test]
    fn sweep_interval_is_never_above_five_minutes() {
        let five_minutes = Duration::from_secs(300);
        assert_eq!(sweep_interval(Duration::from_secs(10 * 60)), five_minutes);
        assert_eq!(sweep_interval(Duration::from_secs(3600)), five_minutes);
        assert_eq!(
            sweep_interval(Duration::from_secs(30 * 24 * 3600)),
            five_minutes
        );
    }

    /// A scratch directory for a database file, removed afterwards.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "lorawan-sniffer-{}-{}",
                name,
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }

        fn db(&self) -> String {
            self.0.join("sniffer.db").display().to_string()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const RETENTION: Duration = Duration::from_secs(3600);
    const ORPHAN: &str = "00E8A80B";

    /// A sniffer that has just been restarted on a database whose only
    /// label belongs to a device with no stored record.
    struct Restarted {
        storage: Storage,
        seen: SeenDevices,
        events: broadcast::Receiver<Arc<String>>,
        writer: LabelWriter,
        _scratch: Scratch,
    }

    fn restarted(name: &str) -> Restarted {
        let scratch = Scratch::new(name);
        {
            // The previous run: the device was heard live, labelled, and
            // never stored.
            let storage = Storage::open(&scratch.db()).unwrap();
            storage
                .set_label(ORPHAN, "Havana", true, MAX_LABELS)
                .unwrap();
        }
        let storage = Storage::open(&scratch.db()).unwrap();
        assert_eq!(storage.labels().unwrap().len(), 1, "the label survived");
        let seen = SeenDevices::new();
        let (labels_tx, events) = broadcast::channel(8);
        let writer = LabelWriter::spawn(
            storage.clone(),
            seen.clone(),
            RETENTION,
            labels_tx,
            CancellationToken::new(),
        );
        Restarted {
            storage,
            seen,
            events,
            writer,
            _scratch: scratch,
        }
    }

    #[tokio::test]
    async fn a_restart_keeps_an_unheard_label_through_the_first_retention_period() {
        let mut run = restarted("restart-keep");

        // Not a single record since the start, but still inside the grace period.
        for uptime in [
            Duration::ZERO,
            RETENTION / 2,
            RETENTION - Duration::from_secs(1),
        ] {
            retention_tick(&run.writer, &run.seen, RETENTION, uptime).await;

            assert_eq!(run.storage.labels().unwrap().len(), 1, "{uptime:?}");
            assert!(run.events.try_recv().is_err(), "{uptime:?}");
        }
    }

    #[tokio::test]
    async fn after_the_first_period_a_label_that_was_never_heard_again_is_removed() {
        let mut run = restarted("restart-remove");

        retention_tick(
            &run.writer,
            &run.seen,
            RETENTION,
            RETENTION + Duration::from_secs(1),
        )
        .await;

        assert!(run.storage.labels().unwrap().is_empty());
        let removal: serde_json::Value =
            serde_json::from_str(&run.events.try_recv().unwrap()).unwrap();
        assert_eq!(
            removal,
            json!({"type": "label", "data": {"device": ORPHAN, "label": null}})
        );
        assert!(run.events.try_recv().is_err());
    }

    #[tokio::test]
    async fn after_the_first_period_a_label_heard_since_the_start_is_kept() {
        let mut run = restarted("restart-heard");
        run.seen.note(ORPHAN);

        retention_tick(&run.writer, &run.seen, RETENTION, RETENTION * 2).await;

        assert_eq!(run.storage.labels().unwrap().len(), 1);
        assert!(run.events.try_recv().is_err());
    }

    #[tokio::test]
    async fn the_sweep_forgets_devices_not_seen_within_the_retention_period() {
        let run = restarted("restart-forget");
        run.seen.note(ORPHAN);

        // A retention of zero makes every sighting too old.
        retention_tick(&run.writer, &run.seen, Duration::ZERO, Duration::ZERO).await;

        assert_eq!(run.seen.len(), 0);
    }

    // -- live device list ---------------------------------------------------

    fn envelope(dev_addr: Option<&str>, dev_eui: Option<&str>) -> String {
        json!({
            "type": "message",
            "data": {
                "id": "x", "mtype": "UnconfirmedDataUp", "rssi": -50,
                "dev_addr": dev_addr, "dev_eui": dev_eui, "fcnt": 1,
            }
        })
        .to_string()
    }

    #[test]
    fn a_data_frame_notes_its_dev_addr() {
        let seen = SeenDevices::new();

        note_sighting(&seen, &envelope(Some("01ABCDEF"), None));

        assert_eq!(seen.len(), 1);
        assert!(seen.seen_within("01ABCDEF", RETENTION));
    }

    #[test]
    fn a_join_request_notes_its_dev_eui() {
        let seen = SeenDevices::new();

        note_sighting(&seen, &envelope(None, Some("36:31:39:54:36:5D:05:39")));

        assert_eq!(seen.len(), 1);
        assert!(seen.seen_within("36:31:39:54:36:5D:05:39", RETENTION));
    }

    #[test]
    fn a_record_with_both_identifiers_notes_both() {
        let seen = SeenDevices::new();

        note_sighting(
            &seen,
            &envelope(Some("01ABCDEF"), Some("36:31:39:54:36:5D:05:39")),
        );

        assert_eq!(seen.len(), 2);
    }

    #[test]
    fn records_without_an_identifier_or_not_records_note_nothing() {
        let seen = SeenDevices::new();

        // A join accept: neither identifier.
        note_sighting(&seen, &envelope(None, None));
        // An envelope with no data, a record without the keys, and noise.
        note_sighting(&seen, r#"{"type":"message"}"#);
        note_sighting(&seen, r#"{"type":"message","data":{}}"#);
        note_sighting(&seen, r#"{"type":"message","data":{"dev_addr":5}}"#);
        note_sighting(&seen, "not json");
        note_sighting(&seen, "");

        assert_eq!(seen.len(), 0);
    }

    #[test]
    fn the_listener_understands_the_envelope_the_api_produces() {
        use crate::core::metadata::{CrcStatus, Direction, MType, Protocol, Status};
        let record = MessageRecord {
            id: "r".to_string(),
            timestamp: Utc::now(),
            direction: Direction::Up,
            status: Status::Captured,
            blocked_by: None,
            source_protocol: Protocol::Udp,
            freq_mhz: 868.1,
            datarate: "SF7BW125".to_string(),
            rssi: None,
            snr: None,
            crc_status: Some(CrcStatus::Ok),
            mtype: MType::UnconfirmedDataUp,
            dev_addr: Some("01ABCDEF".to_string()),
            dev_eui: None,
            fcnt: Some(1),
            fport: Some(1),
            payload_size: 4,
            toa_ms: None,
            gateway_eui: None,
            join_eui: None,
            dev_nonce: None,
            adr: None,
            adr_ack_req: None,
            ack: None,
            class_b: None,
            f_pending: None,
            f_opts_len: None,
            malformed: None,
        };
        let seen = SeenDevices::new();

        note_sighting(&seen, &api::make_ws_json(&record).unwrap());

        assert!(seen.seen_within("01ABCDEF", RETENTION));
    }

    /// A 15-byte UnconfirmedDataUp frame whose bytes read as DevAddr
    /// `6874FA22`: the kind of fake identifier a frame with a failed CRC used
    /// to put in the table.
    fn frame_of_noise_dev_addr() -> Vec<u8> {
        let mut frame = vec![0x40, 0x22, 0xFA, 0x74, 0x68]; // MHDR, DevAddr
        frame.push(0x00); // FCtrl
        frame.extend_from_slice(&[0x05, 0x00]); // FCnt
        frame.push(0x01); // FPort
        frame.extend_from_slice(&[0x11, 0x22]); // FRMPayload
        frame.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]); // MIC
        frame
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

    /// The record envelope the proxy broadcasts for `frame` received with CRC
    /// status `crc`: the fields come from the decoder, as they do in the
    /// backends.
    fn envelope_of_frame(frame: &[u8], crc: crate::core::metadata::CrcStatus) -> String {
        use crate::core::metadata::{Direction, Protocol, Status};

        let crc = Some(crc);
        let fields = crate::proxy::frame_fields(
            crate::core::decoder::decode_phy_bytes(frame, crc.as_ref()),
            frame.len() as u16,
            crc.as_ref(),
        );
        let record = MessageRecord {
            id: MessageRecord::new_id(),
            timestamp: Utc::now(),
            direction: Direction::Up,
            status: Status::Forwarded,
            blocked_by: None,
            source_protocol: Protocol::Udp,
            freq_mhz: 868.1,
            datarate: "SF7BW125".to_string(),
            rssi: Some(-50),
            snr: Some(7.5),
            crc_status: crc,
            mtype: fields.mtype,
            dev_addr: fields.dev_addr,
            dev_eui: fields.dev_eui,
            fcnt: fields.fcnt,
            fport: fields.fport,
            payload_size: fields.payload_size,
            toa_ms: None,
            gateway_eui: None,
            join_eui: fields.join_eui,
            dev_nonce: fields.dev_nonce,
            adr: fields.fctrl.map(|c| c.adr),
            adr_ack_req: fields.fctrl.and_then(|c| c.adr_ack_req),
            ack: fields.fctrl.map(|c| c.ack),
            class_b: fields.fctrl.and_then(|c| c.class_b),
            f_pending: fields.fctrl.and_then(|c| c.f_pending),
            f_opts_len: fields.fctrl.map(|c| c.f_opts_len),
            malformed: fields.malformed,
        };
        api::make_ws_json(&record).unwrap().to_string()
    }

    #[test]
    fn malformed_frames_note_no_identifier() {
        use crate::core::metadata::CrcStatus;

        let data = frame_of_noise_dev_addr();
        let join = join_request_frame();
        let mut bad_mhdr = data.clone();
        bad_mhdr[0] |= 0x04;

        // One frame for each reason, all of which would have been noted as a
        // device if their bytes had been taken at face value.
        let seen = SeenDevices::new();
        for envelope in [
            envelope_of_frame(&data, CrcStatus::Fail),
            envelope_of_frame(&bad_mhdr, CrcStatus::Ok),
            envelope_of_frame(&data[..9], CrcStatus::Ok),
            envelope_of_frame(&join[..20], CrcStatus::Ok),
        ] {
            assert!(envelope.contains(r#""malformed":""#), "{envelope}");
            note_sighting(&seen, &envelope);
        }
        assert_eq!(seen.len(), 0);

        // Control: the same bytes, valid, are the devices they look like.
        note_sighting(&seen, &envelope_of_frame(&data, CrcStatus::Ok));
        note_sighting(&seen, &envelope_of_frame(&join, CrcStatus::Ok));
        assert_eq!(seen.len(), 2);
        assert!(seen.seen_within("6874FA22", RETENTION));
    }

    #[tokio::test]
    async fn the_listener_carries_on_after_falling_behind() {
        let (tx, rx) = broadcast::channel::<Arc<String>>(2);
        let seen = SeenDevices::new();
        let cancel = CancellationToken::new();
        // Five records before the listener has run: it missed the first three.
        for i in 0..5 {
            tx.send(Arc::new(envelope(Some(&format!("0000000{i}")), None)))
                .unwrap();
        }
        let listener = tokio::spawn(run_seen_listener(rx, seen.clone(), cancel.clone()));

        timeout(Duration::from_secs(10), async {
            while seen.len() < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the listener never noted the records that were left");

        assert!(seen.seen_within("00000003", RETENTION));
        assert!(seen.seen_within("00000004", RETENTION));
        assert!(!seen.seen_within("00000000", RETENTION));
        // And it still works afterwards.
        tx.send(Arc::new(envelope(Some("0000000A"), None))).unwrap();
        timeout(Duration::from_secs(10), async {
            while !seen.seen_within("0000000A", RETENTION) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the listener stopped after falling behind");
        cancel.cancel();
        timeout(Duration::from_secs(10), listener)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn the_listener_stops_when_the_broadcast_closes() {
        let (tx, rx) = broadcast::channel::<Arc<String>>(4);
        let listener = tokio::spawn(run_seen_listener(
            rx,
            SeenDevices::new(),
            CancellationToken::new(),
        ));

        drop(tx);

        timeout(Duration::from_secs(10), listener)
            .await
            .unwrap()
            .unwrap();
    }
}
