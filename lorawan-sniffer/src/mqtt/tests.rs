//! Tests for the MQTT publisher against an in-process fake broker.
//!
//! The broker is a bare `TcpListener` that speaks just enough MQTT 3.1.1,
//! using `rumqttc`'s own packet codec: it answers CONNECT with CONNACK,
//! records every PUBLISH, and can be told to hang up after a number of
//! publishes to imitate a broker that keeps failing.  Records are offered
//! through [`Outputs`] exactly as the proxy does.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use bytes::BytesMut;
use chrono::Utc;
use rumqttc::mqttbytes::v4::{ConnAck, Connect, ConnectReturnCode, Packet, PubAck, Publish};
use rumqttc::mqttbytes::{Error as PacketError, QoS};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::core::metadata::{Direction, MType, Protocol, Status};
use crate::proxy::Outputs;

/// Backoff short enough that tests reconnect in milliseconds.
const FAST: Tuning = Tuning {
    min_backoff: Duration::from_millis(5),
    max_backoff: Duration::from_millis(40),
};

/// Longest packet the fake broker accepts.
const MAX_PACKET: usize = 1024 * 1024;

// ---------------------------------------------------------------------------
// Fake broker
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Behaviour {
    /// Hang up on a connection right after this many publishes arrived on it.
    drop_after: Option<usize>,
    /// How many connections get hung up on; later ones stay open.
    drop_connections: usize,
    /// Answer QoS 1 publishes with PUBACK.
    ack_qos1: bool,
}

impl Behaviour {
    /// A broker that behaves.
    const HEALTHY: Behaviour = Behaviour {
        drop_after: None,
        drop_connections: 0,
        ack_qos1: true,
    };
}

#[derive(Default)]
struct BrokerState {
    publishes: Mutex<Vec<Publish>>,
    connects: Mutex<Vec<Connect>>,
    /// Connections accepted so far, including the one being served.
    connections: AtomicUsize,
}

struct FakeBroker {
    addr: SocketAddr,
    state: Arc<BrokerState>,
    task: JoinHandle<()>,
}

impl FakeBroker {
    async fn start(behaviour: Behaviour) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = Arc::new(BrokerState::default());
        let task = tokio::spawn(serve(listener, behaviour, Arc::clone(&state)));
        FakeBroker { addr, state, task }
    }

    /// Client settings that point at this broker.
    fn config(&self, qos: u8) -> MqttConfig {
        MqttConfig {
            enabled: true,
            server: self.addr.ip().to_string(),
            port: self.addr.port(),
            qos,
            ..MqttConfig::default()
        }
    }

    fn publishes(&self) -> Vec<Publish> {
        self.state.publishes.lock().unwrap().clone()
    }

    fn publish_count(&self) -> usize {
        self.state.publishes.lock().unwrap().len()
    }

    fn connections(&self) -> usize {
        self.state.connections.load(Ordering::SeqCst)
    }
}

impl Drop for FakeBroker {
    fn drop(&mut self) {
        // Aborting drops the listener and the connection being served.
        self.task.abort();
    }
}

async fn serve(listener: TcpListener, behaviour: Behaviour, state: Arc<BrokerState>) {
    let mut hung_up = 0;
    loop {
        let Ok((socket, _)) = listener.accept().await else {
            return;
        };
        state.connections.fetch_add(1, Ordering::SeqCst);

        let drop_after = if hung_up < behaviour.drop_connections {
            behaviour.drop_after
        } else {
            None
        };
        if serve_connection(socket, behaviour.ack_qos1, drop_after, &state).await {
            hung_up += 1;
        }
    }
}

/// Serve one client connection.  Returns `true` when the broker ended it by
/// hanging up after `drop_after` publishes, `false` when the client left.
async fn serve_connection(
    mut socket: TcpStream,
    ack_qos1: bool,
    drop_after: Option<usize>,
    state: &BrokerState,
) -> bool {
    let mut buf = BytesMut::with_capacity(16 * 1024);
    let mut seen = 0;

    loop {
        // Answer every complete packet already read.
        loop {
            let mut reply = BytesMut::new();
            match Packet::read(&mut buf, MAX_PACKET) {
                Ok(Packet::Connect(connect)) => {
                    state.connects.lock().unwrap().push(connect);
                    ConnAck::new(ConnectReturnCode::Success, false)
                        .write(&mut reply)
                        .unwrap();
                }
                Ok(Packet::Publish(publish)) => {
                    seen += 1;
                    if ack_qos1 && publish.qos == QoS::AtLeastOnce {
                        PubAck::new(publish.pkid).write(&mut reply).unwrap();
                    }
                    state.publishes.lock().unwrap().push(publish);
                    if Some(seen) == drop_after {
                        // Dropping the socket closes it with the client's
                        // later packets unread, as a crashing broker would.
                        return true;
                    }
                }
                Ok(Packet::PingReq) => reply.extend_from_slice(&[0xD0, 0x00]),
                Ok(Packet::Disconnect) => return false,
                Ok(_) => {}
                Err(PacketError::InsufficientBytes(_)) => break,
                Err(_) => return false,
            }
            if !reply.is_empty() && socket.write_all(&reply).await.is_err() {
                return false;
            }
        }

        match socket.read_buf(&mut buf).await {
            Ok(0) | Err(_) => return false,
            Ok(_) => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// The publisher wired to an [`Outputs`], as `cmd/root.rs` does it.
struct Harness {
    outputs: Outputs,
    stats: Arc<MqttStats>,
    connected: Arc<AtomicBool>,
    cancel: CancellationToken,
    /// Calls to [`Harness::emit`].
    emitted: usize,
    _storage_drain: JoinHandle<()>,
}

impl Harness {
    fn start(cfg: &MqttConfig) -> Self {
        let cancel = CancellationToken::new();
        let (mqtt_tx, stats, connected) = start_with(cfg, cancel.clone(), FAST).unwrap();

        // The storage side is not under test, but its queue must not fill up
        // and log warnings of its own.
        let (storage_tx, mut storage_rx) = mpsc::channel(RECORD_QUEUE_CAP);
        let drain = tokio::spawn(async move { while storage_rx.recv().await.is_some() {} });
        let (live_tx, _live_rx) = broadcast::channel(16);

        Harness {
            outputs: Outputs::new(storage_tx, live_tx).with_mqtt(mqtt_tx, Arc::clone(&stats)),
            stats,
            connected,
            cancel,
            emitted: 0,
            _storage_drain: drain,
        }
    }

    fn emit(&mut self, record: MessageRecord) {
        self.outputs.emit(record);
        self.emitted += 1;
    }

    async fn wait_connected(&self) {
        wait_until("the client to connect", || {
            self.connected.load(Ordering::SeqCst)
        })
        .await;
    }

    fn published(&self) -> u64 {
        self.stats.published.load(Ordering::SeqCst)
    }

    fn dropped(&self) -> u64 {
        self.stats.dropped.load(Ordering::SeqCst)
    }

    fn high_water(&self) -> u64 {
        self.stats.pending_high_water.load(Ordering::SeqCst)
    }

    /// Wait for traffic to settle: every emitted record accounted for as
    /// published or dropped.  Counters that overshoot are caught by the
    /// caller, which compares for equality after this returns.
    async fn settle(&self) {
        let emitted = self.emitted as u64;
        wait_until("every record to be published or dropped", || {
            self.published() + self.dropped() >= emitted
        })
        .await;
        // Anything that was going to be counted twice has had time to be.
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

async fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn test_record() -> MessageRecord {
    MessageRecord {
        id: MessageRecord::new_id(),
        timestamp: Utc::now(),
        direction: Direction::Up,
        status: Status::Forwarded,
        blocked_by: None,
        source_protocol: Protocol::Udp,
        freq_mhz: 868.1,
        datarate: "SF7BW125".to_string(),
        rssi: Some(-80),
        snr: Some(7.5),
        crc_status: None,
        mtype: MType::UnconfirmedDataUp,
        dev_addr: Some("01ABCDEF".to_string()),
        dev_eui: None,
        fcnt: Some(1),
        fport: Some(1),
        payload_size: 17,
        toa_ms: Some(56.6),
        gateway_eui: Some("AA:BB:CC:DD:00:11:22:33".to_string()),
        join_eui: None,
        dev_nonce: None,
        adr: None,
        adr_ack_req: None,
        ack: None,
        class_b: None,
        f_pending: None,
        f_opts_len: None,
    }
}

/// One record for every direction, status and protocol, with the optional
/// fields present on some and absent on others.
fn records_of_every_kind() -> Vec<MessageRecord> {
    let mut records = Vec::new();
    for direction in [Direction::Up, Direction::Down] {
        for status in [Status::Forwarded, Status::Blocked, Status::Captured] {
            for protocol in [Protocol::Udp, Protocol::Concentratord] {
                let n = records.len();
                let mut record = test_record();
                record.direction = direction.clone();
                record.status = status.clone();
                record.source_protocol = protocol;
                record.mtype = match (&direction, n % 3) {
                    (Direction::Up, 0) => MType::JoinRequest,
                    (Direction::Up, 1) => MType::ConfirmedDataUp,
                    (Direction::Up, _) => MType::UnconfirmedDataUp,
                    (Direction::Down, 0) => MType::JoinAccept,
                    (Direction::Down, 1) => MType::ConfirmedDataDown,
                    (Direction::Down, _) => MType::UnconfirmedDataDown,
                };
                record.gateway_eui = (n % 4 != 0).then(|| format!("58:A0:CB:FF:FE:80:00:{n:02X}"));
                record.dev_addr = (n % 2 == 0).then(|| format!("01AB{n:04X}"));
                record.dev_eui = (n % 5 == 0).then(|| "01:02:03:04:05:06:07:08".to_string());
                record.blocked_by = (status == Status::Blocked).then(|| "block rule".to_string());
                records.push(record);
            }
        }
    }
    records
}

/// The topic the default template must produce, written out by hand so the
/// test does not depend on the code it checks.
fn expected_topic(record: &MessageRecord) -> String {
    let direction = match record.direction {
        Direction::Up => "up",
        Direction::Down => "down",
    };
    format!(
        "lorawan-sniffer/{}/{}/{:?}",
        record.gateway_eui.as_deref().unwrap_or("unknown"),
        direction,
        record.mtype
    )
}

// ---------------------------------------------------------------------------
// Publishing while the broker is healthy
// ---------------------------------------------------------------------------

async fn every_record_arrives_exactly_once(qos: u8, expected_qos: QoS) {
    let broker = FakeBroker::start(Behaviour::HEALTHY).await;
    let mut harness = Harness::start(&broker.config(qos));
    harness.wait_connected().await;

    let records = records_of_every_kind();
    for record in &records {
        harness.emit(record.clone());
    }

    wait_until("the broker to receive every record", || {
        broker.publish_count() >= records.len()
    })
    .await;
    // A duplicate would show up shortly after the last expected record.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let received = broker.publishes();
    assert_eq!(received.len(), records.len(), "each record exactly once");
    for record in &records {
        let matching: Vec<&Publish> = received
            .iter()
            .filter(|p| {
                serde_json::from_slice::<serde_json::Value>(&p.payload).unwrap()["id"]
                    == record.id.as_str()
            })
            .collect();
        assert_eq!(
            matching.len(),
            1,
            "record {} arrived {} times",
            record.id,
            matching.len()
        );

        let publish = matching[0];
        assert_eq!(publish.topic, expected_topic(record));
        assert_eq!(publish.qos, expected_qos);
        assert!(!publish.retain, "records must not be retained");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&publish.payload).unwrap(),
            serde_json::to_value(record).unwrap(),
            "payload of {}",
            record.id
        );
    }

    assert_eq!(harness.published(), records.len() as u64);
    assert_eq!(harness.dropped(), 0);
}

#[tokio::test]
async fn healthy_broker_receives_each_record_exactly_once() {
    every_record_arrives_exactly_once(0, QoS::AtMostOnce).await;
}

#[tokio::test]
async fn healthy_broker_receives_each_record_exactly_once_at_qos1() {
    every_record_arrives_exactly_once(1, QoS::AtLeastOnce).await;
}

#[tokio::test]
async fn topic_template_from_the_configuration_is_used() {
    let broker = FakeBroker::start(Behaviour::HEALTHY).await;
    let cfg = MqttConfig {
        topic: "lora/{status}/{dev_addr}".to_string(),
        ..broker.config(0)
    };
    let mut harness = Harness::start(&cfg);
    harness.wait_connected().await;

    let mut record = test_record();
    record.status = Status::Blocked;
    harness.emit(record);
    // A join request has no DevAddr.
    let mut join = test_record();
    join.dev_addr = None;
    harness.emit(join);

    wait_until("two publishes", || broker.publish_count() >= 2).await;
    let topics: Vec<String> = broker.publishes().into_iter().map(|p| p.topic).collect();
    assert_eq!(topics, ["lora/Blocked/01ABCDEF", "lora/Forwarded/unknown"]);
}

#[tokio::test]
async fn connect_packet_carries_identity_and_a_clean_session() {
    let broker = FakeBroker::start(Behaviour::HEALTHY).await;
    let cfg = MqttConfig {
        client_id: "sniffer-under-test".to_string(),
        username: "alice".to_string(),
        password: "pa55word".to_string(),
        ..broker.config(0)
    };
    let harness = Harness::start(&cfg);
    harness.wait_connected().await;

    let connects = broker.state.connects.lock().unwrap();
    assert_eq!(connects.len(), 1);
    assert_eq!(connects[0].client_id, "sniffer-under-test");
    assert!(connects[0].clean_session, "sessions must be clean");
    let login = connects[0].login.as_ref().expect("credentials were sent");
    assert_eq!(login.username, "alice");
    assert_eq!(login.password, "pa55word");
}

#[tokio::test]
async fn no_credentials_are_sent_when_no_username_is_set() {
    let broker = FakeBroker::start(Behaviour::HEALTHY).await;
    let harness = Harness::start(&broker.config(0));
    harness.wait_connected().await;

    assert!(broker.state.connects.lock().unwrap()[0].login.is_none());
}

// ---------------------------------------------------------------------------
// Broker failures
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn flapping_broker_stays_bounded_and_accounted() {
    const CYCLES: usize = 20;
    const PUBLISHES_PER_CONNECTION: usize = 50;

    let broker = FakeBroker::start(Behaviour {
        drop_after: Some(PUBLISHES_PER_CONNECTION),
        drop_connections: CYCLES,
        ack_qos1: false,
    })
    .await;
    let mut harness = Harness::start(&broker.config(0));
    harness.wait_connected().await;

    // Emit continuously, in bursts of different sizes (the biggest is larger
    // than the client's request channel), until the broker has hung up CYCLES
    // times and the next connection is the stable one.
    let deadline = Instant::now() + Duration::from_secs(60);
    let bursts = [5, 40, 300, 12];
    let mut round = 0;
    while broker.connections() <= CYCLES {
        assert!(
            Instant::now() < deadline,
            "the broker never finished flapping"
        );
        for _ in 0..bursts[round % bursts.len()] {
            harness.emit(test_record());
        }
        round += 1;
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    // Some traffic on the stable connection as well.
    harness.wait_connected().await;
    for _ in 0..200 {
        harness.emit(test_record());
    }

    harness.settle().await;

    assert!(
        harness.high_water() <= (REQUEST_CAP + 100) as u64,
        "the client held {} messages",
        harness.high_water()
    );
    assert_eq!(
        harness.published() + harness.dropped(),
        harness.emitted as u64,
        "published {} + dropped {} != emitted {}",
        harness.published(),
        harness.dropped(),
        harness.emitted
    );
    // The test would prove nothing if the broker had not really flapped or
    // if every record had been lost.
    assert!(broker.connections() > CYCLES);
    assert!(harness.published() > 0);
    assert!(harness.dropped() > 0);
    assert!(broker.publish_count() as u64 <= harness.published());
}

#[tokio::test]
async fn unacknowledged_messages_are_counted_and_not_resent_after_a_reconnect() {
    const UNACKED: usize = 10;

    // At QoS 1 the client remembers every publish until it is acknowledged
    // and would normally send it again on the next connection.  This broker
    // acknowledges nothing and hangs up after the tenth.
    let broker = FakeBroker::start(Behaviour {
        drop_after: Some(UNACKED),
        drop_connections: 1,
        ack_qos1: false,
    })
    .await;
    let mut harness = Harness::start(&broker.config(1));
    harness.wait_connected().await;

    for _ in 0..UNACKED {
        harness.emit(test_record());
    }

    wait_until("the lost messages to be counted", || {
        harness.dropped() >= UNACKED as u64
    })
    .await;
    assert_eq!(harness.high_water(), UNACKED as u64);
    assert_eq!(harness.dropped(), UNACKED as u64);

    // Reconnected: only new traffic reaches the broker.
    wait_until("the client to reconnect", || {
        broker.connections() == 2 && harness.connected.load(Ordering::SeqCst)
    })
    .await;
    harness.emit(test_record());
    wait_until("the new record", || broker.publish_count() > UNACKED).await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let received = broker.publishes();
    assert_eq!(received.len(), UNACKED + 1, "nothing may be sent twice");
    assert!(received.iter().all(|p| !p.dup), "no redelivery flags");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unreachable_broker_drops_without_growth() {
    // A port nothing listens on: bind to find a free one, then let go of it.
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let cfg = MqttConfig {
        enabled: true,
        server: "127.0.0.1".to_string(),
        port,
        ..MqttConfig::default()
    };
    let mut harness = Harness::start(&cfg);

    // `emit` never waits, so the whole run takes far less than the limit.
    let started = Instant::now();
    for _ in 0..50 {
        for _ in 0..100 {
            harness.emit(test_record());
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert!(started.elapsed() < Duration::from_secs(10));

    harness.settle().await;

    assert_eq!(harness.emitted, 5000);
    assert_eq!(harness.published(), 0);
    assert_eq!(harness.dropped(), harness.emitted as u64);
    // Nothing was ever handed to the client, so it never held anything.
    assert_eq!(harness.high_water(), 0);
    assert!(!harness.connected.load(Ordering::SeqCst));
}

#[tokio::test]
async fn client_reconnects_when_the_broker_comes_back() {
    // Reserve a port, start the client against it with nothing listening, then
    // bring a broker up on that port.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let cfg = MqttConfig {
        enabled: true,
        server: addr.ip().to_string(),
        port: addr.port(),
        ..MqttConfig::default()
    };
    let mut harness = Harness::start(&cfg);

    harness.emit(test_record());
    wait_until("the first record to be dropped", || harness.dropped() == 1).await;

    let listener = TcpListener::bind(addr).await.unwrap();
    let state = Arc::new(BrokerState::default());
    let task = tokio::spawn(serve(listener, Behaviour::HEALTHY, Arc::clone(&state)));

    harness.wait_connected().await;
    harness.emit(test_record());
    wait_until("the record to reach the revived broker", || {
        !state.publishes.lock().unwrap().is_empty()
    })
    .await;
    task.abort();

    assert_eq!(harness.dropped(), 1);
}

#[tokio::test]
async fn tls_is_not_silently_skipped() {
    // This broker speaks plain MQTT.  A client configured for TLS must fail
    // the handshake instead of falling back to an unencrypted connection.
    let broker = FakeBroker::start(Behaviour::HEALTHY).await;
    let cfg = MqttConfig {
        tls: true,
        ..broker.config(0)
    };
    let mut harness = Harness::start(&cfg);

    wait_until("the client to try the broker", || broker.connections() >= 1).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    harness.emit(test_record());
    harness.settle().await;

    assert!(!harness.connected.load(Ordering::SeqCst));
    assert_eq!(broker.state.connects.lock().unwrap().len(), 0);
    assert_eq!(broker.publish_count(), 0);
    assert_eq!(harness.dropped(), 1);
}

#[tokio::test]
async fn cancelling_stops_both_tasks() {
    let broker = FakeBroker::start(Behaviour::HEALTHY).await;
    let cancel = CancellationToken::new();
    let (tx, _stats, connected) = start_with(&broker.config(0), cancel.clone(), FAST).unwrap();
    wait_until("the client to connect", || connected.load(Ordering::SeqCst)).await;

    cancel.cancel();

    // The publisher dropped its queue and the event loop closed the connection.
    wait_until("the publisher to stop", || tx.is_closed()).await;
    wait_until("the connection flag to clear", || {
        !connected.load(Ordering::SeqCst)
    })
    .await;
}

// ---------------------------------------------------------------------------
// Startup
// ---------------------------------------------------------------------------

#[test]
fn start_rejects_invalid_settings() {
    // Every one of these fails before anything is spawned, so no runtime is
    // needed.
    let cancel = CancellationToken::new();

    for qos in [3, 255] {
        let cfg = MqttConfig {
            qos,
            ..MqttConfig::default()
        };
        assert!(start(&cfg, cancel.clone()).is_err(), "qos {qos}");
    }

    for topic in ["", "a/{unknown}", "a/{mtype", "a/+"] {
        let cfg = MqttConfig {
            topic: topic.to_string(),
            ..MqttConfig::default()
        };
        assert!(start(&cfg, cancel.clone()).is_err(), "topic '{topic}'");
    }

    let cfg = MqttConfig {
        tls: true,
        ca_file: "/nonexistent/lorawan-sniffer/ca.pem".to_string(),
        ..MqttConfig::default()
    };
    let err = format!("{:#}", start(&cfg, cancel).err().unwrap());
    assert!(err.contains("/nonexistent/lorawan-sniffer/ca.pem"), "{err}");
}

// ---------------------------------------------------------------------------
// Secrets
// ---------------------------------------------------------------------------

/// Collects every log line, at every level, from the whole test process.
struct CaptureLogger {
    lines: Mutex<Vec<String>>,
}

impl log::Log for CaptureLogger {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }

    fn log(&self, record: &log::Record) {
        self.lines.lock().unwrap().push(format!(
            "{} {}: {}",
            record.level(),
            record.target(),
            record.args()
        ));
    }

    fn flush(&self) {}
}

fn capture_logger() -> &'static CaptureLogger {
    static LOGGER: OnceLock<CaptureLogger> = OnceLock::new();
    let logger = LOGGER.get_or_init(|| CaptureLogger {
        lines: Mutex::new(Vec::new()),
    });
    // Ignore the error from a second test installing it again.
    let _ = log::set_logger(logger);
    log::set_max_level(log::LevelFilter::Trace);
    logger
}

#[tokio::test]
async fn logs_never_contain_the_password() {
    const CANARY: &str = "canary-7f3e-password";
    let logger = capture_logger();

    // Once against a broker that comes and goes, once against nothing.
    let broker = FakeBroker::start(Behaviour {
        drop_after: Some(3),
        drop_connections: 2,
        ack_qos1: true,
    })
    .await;
    let reachable = MqttConfig {
        username: "alice".to_string(),
        password: CANARY.to_string(),
        ..broker.config(1)
    };
    let mut harness = Harness::start(&reachable);
    harness.wait_connected().await;
    for _ in 0..30 {
        harness.emit(test_record());
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    harness.settle().await;
    drop(harness);

    let unreachable = MqttConfig {
        username: "alice".to_string(),
        password: CANARY.to_string(),
        server: "127.0.0.1".to_string(),
        port: 1,
        ..MqttConfig::default()
    };
    let mut harness = Harness::start(&unreachable);
    for _ in 0..10 {
        harness.emit(test_record());
    }
    harness.settle().await;
    drop(harness);

    let lines = logger.lines.lock().unwrap();
    assert!(
        lines.iter().any(|l| l.contains("MQTT")),
        "the publisher logged nothing, so the check below proves nothing"
    );
    let leaked: Vec<&String> = lines.iter().filter(|l| l.contains(CANARY)).collect();
    assert!(leaked.is_empty(), "the password was logged: {leaked:?}");
}
