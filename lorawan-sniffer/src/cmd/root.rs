use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use tokio::signal::unix::{signal, Signal, SignalKind};
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::core::eui::Eui64;
use crate::core::filter::FilterRule;
use crate::core::metadata::MessageRecord;
use crate::core::storage::Storage;

use crate::api;
use crate::config::{self, Configuration, UdpMode};
use crate::mqtt;
use crate::proxy;

/// How often the retention task sweeps the database for expired rows.
const RETENTION_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Buffered capacity of the record channel (records/messages).
/// LoRaWAN rates are << 1000/s, so 1024 is plenty.
const CHANNEL_CAPACITY: usize = 1024;

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

    // Channel: proxy tasks → storage writer
    let (record_tx, record_rx) = mpsc::channel::<MessageRecord>(CHANNEL_CAPACITY);

    // Broadcast hub: proxy tasks → WebSocket clients (pre-serialised JSON).
    // Capacity 512: at LoRaWAN rates this is never hit; slow WS clients lag.
    let (broadcast_tx, _broadcast_rx) = broadcast::channel::<Arc<String>>(512);

    // Everywhere a captured record goes, behind one non-blocking `emit`.
    let outputs = proxy::Outputs::new(record_tx, broadcast_tx.clone());

    let cancel = CancellationToken::new();

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
    let writer = tokio::spawn(async move {
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
    use super::*;

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
}
