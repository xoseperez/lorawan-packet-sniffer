//! Loopback tests for the UDP proxy: fake gateways on one side, a fake LNS on
//! the other, and the real proxy loop in between.
//!
//! Everything runs on `127.0.0.1` with ephemeral ports.  Every receive is
//! wrapped in a timeout so a bug fails a test instead of hanging it.
//!
//! "Nothing arrives" assertions are only meaningful if the proxy has already
//! handled the datagram in question.  The gateway-facing socket is a single
//! queue handled in order, so the tests send the datagram that must be
//! swallowed first and a datagram that must get through second: once the
//! second shows up, the first has been dealt with, and a short quiet period
//! then catches anything stray.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use base64::prelude::*;
use tokio::net::UdpSocket;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use super::packet::{
    build_tx_ack, gateway_eui, RxPk, TxPk, PROTOCOL_VERSION, PULL_ACK, PULL_DATA, PULL_RESP,
    PUSH_ACK, PUSH_DATA, TX_ACK,
};
use super::{build_downlink_record, build_uplink_record, run_with_socket, UdpProxy};
use crate::config::{Configuration, MqttConfig, UdpConfig, UdpMode};
use crate::core::eui::Eui64;
use crate::core::filter::FilterRule;
use crate::core::metadata::{
    assert_body_cleared, CrcStatus, Direction, MType, Malformed, MessageRecord, Protocol, Status,
};
use crate::core::ratelimit::DEFAULT_INTERVAL;
use crate::proxy::{Outputs, Verdict};

/// Upper bound for anything that is supposed to happen.
const WAIT: Duration = Duration::from_secs(5);

/// How long to watch for something that is supposed not to happen.
const QUIET: Duration = Duration::from_millis(250);

// ---------------------------------------------------------------------------
// Datagram and frame builders
// ---------------------------------------------------------------------------

fn header(token: [u8; 2], kind: u8) -> Vec<u8> {
    vec![PROTOCOL_VERSION, token[0], token[1], kind]
}

/// An UnconfirmedDataUp frame from `dev_addr`.
fn data_up(dev_addr: u32, fcnt: u16) -> Vec<u8> {
    data_frame(0x40, dev_addr, fcnt)
}

/// An UnconfirmedDataDown frame for `dev_addr`.
fn data_down(dev_addr: u32, fcnt: u16) -> Vec<u8> {
    data_frame(0x60, dev_addr, fcnt)
}

fn data_frame(mhdr: u8, dev_addr: u32, fcnt: u16) -> Vec<u8> {
    let mut frame = vec![mhdr];
    frame.extend_from_slice(&dev_addr.to_le_bytes());
    frame.push(0x00); // FCtrl
    frame.extend_from_slice(&fcnt.to_le_bytes());
    frame.push(1); // FPort
    frame.extend_from_slice(&[0x11, 0x22]); // FRMPayload
    frame.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]); // MIC
    frame
}

fn join_request(dev_eui_last: u8) -> Vec<u8> {
    let mut frame = vec![0x00];
    frame.extend_from_slice(&[0x01, 0, 0, 0, 0, 0, 0, 0]); // JoinEUI
    frame.extend_from_slice(&[dev_eui_last, 7, 6, 5, 4, 3, 2, 1]); // DevEUI
    frame.extend_from_slice(&[0x34, 0x12]); // DevNonce
    frame.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]); // MIC
    frame
}

fn join_accept() -> Vec<u8> {
    let mut frame = vec![0x20];
    frame.extend_from_slice(&[0x55; 16]); // encrypted body
    frame
}

/// A PUSH_DATA datagram from `eui` carrying one `rxpk` per frame, each with a
/// passed CRC check.
fn push_data(token: [u8; 2], eui: &Eui64, frames: &[Vec<u8>]) -> Vec<u8> {
    let frames: Vec<(Vec<u8>, i8)> = frames.iter().map(|f| (f.clone(), 1)).collect();
    push_data_with(token, eui, &frames)
}

/// Like [`push_data`], with the CRC status `stat` of each frame: 1 for a passed
/// check, -1 for a failed one and 0 for none.
fn push_data_with(token: [u8; 2], eui: &Eui64, frames: &[(Vec<u8>, i8)]) -> Vec<u8> {
    let rxpk: Vec<String> = frames
        .iter()
        .map(|(f, stat)| {
            format!(
                r#"{{"freq":868.1,"datr":"SF7BW125","codr":"4/5","rssi":-50,"lsnr":7.5,"stat":{},"size":{},"data":"{}"}}"#,
                stat,
                f.len(),
                BASE64_STANDARD.encode(f)
            )
        })
        .collect();
    let mut pkt = header(token, PUSH_DATA);
    pkt.extend_from_slice(&eui.0);
    pkt.extend_from_slice(format!(r#"{{"rxpk":[{}]}}"#, rxpk.join(",")).as_bytes());
    pkt
}

/// Three frames as a gateway that forwards frames with a failed CRC delivers
/// them: a one-byte "ConfirmedDataDown", a 22-byte "Proprietary" and a 238-byte
/// "UnconfirmedDataUp" whose bytes read as DevAddr `6874FA22` and FPort 127.
fn lns01_noise() -> Vec<Vec<u8>> {
    let one_byte = vec![0xA0];

    let mut proprietary = vec![0xE0];
    proprietary.extend((1..22).map(|i| (i * 7) as u8));

    let mut uplink = vec![0x40, 0x22, 0xFA, 0x74, 0x68, 0x00, 0x05, 0x00, 127];
    uplink.resize(238, 0x5A);

    vec![one_byte, proprietary, uplink]
}

/// One of every kind of uplink a noisy gateway delivers in a single PUSH_DATA,
/// with the reason each one is expected to be recorded under: the noise (CRC
/// failed), a valid frame with a reserved MHDR bit, a nine-byte data frame and
/// a valid frame.
fn mixed_uplinks() -> Vec<(Vec<u8>, i8, Option<Malformed>)> {
    let mut frames: Vec<_> = lns01_noise()
        .into_iter()
        .map(|f| (f, -1, Some(Malformed::Crc)))
        .collect();
    let mut bad_mhdr = data_up(0x1001, 2);
    bad_mhdr[0] |= 0x04;
    frames.push((bad_mhdr, 1, Some(Malformed::Mhdr)));
    frames.push((data_up(0x1001, 3)[..9].to_vec(), 1, Some(Malformed::Length)));
    frames.push((data_up(0x1001, 4), 1, None));
    frames
}

/// The PUSH_DATA datagram of [`mixed_uplinks`].
fn mixed_push_data(token: [u8; 2], eui: &Eui64) -> Vec<u8> {
    let frames: Vec<(Vec<u8>, i8)> = mixed_uplinks()
        .into_iter()
        .map(|(f, stat, _)| (f, stat))
        .collect();
    push_data_with(token, eui, &frames)
}

fn pull_data(token: [u8; 2], eui: &Eui64) -> Vec<u8> {
    let mut pkt = header(token, PULL_DATA);
    pkt.extend_from_slice(&eui.0);
    pkt
}

fn tx_ack(token: [u8; 2], eui: &Eui64) -> Vec<u8> {
    let mut pkt = header(token, TX_ACK);
    pkt.extend_from_slice(&eui.0);
    pkt.extend_from_slice(br#"{"txpk_ack":{"error":"NONE"}}"#);
    pkt
}

fn push_ack(token: [u8; 2]) -> Vec<u8> {
    header(token, PUSH_ACK)
}

fn pull_ack(token: [u8; 2]) -> Vec<u8> {
    header(token, PULL_ACK)
}

/// A PULL_RESP datagram carrying `frame` as its `txpk`.
fn pull_resp(token: [u8; 2], frame: &[u8]) -> Vec<u8> {
    let mut pkt = header(token, PULL_RESP);
    pkt.extend_from_slice(
        format!(
            r#"{{"txpk":{{"imme":true,"freq":869.525,"rfch":0,"powe":27,"modu":"LORA","datr":"SF9BW125","codr":"4/5","ipol":true,"size":{},"data":"{}"}}}}"#,
            frame.len(),
            BASE64_STANDARD.encode(frame)
        )
        .as_bytes(),
    );
    pkt
}

// ---------------------------------------------------------------------------
// Fake gateway
// ---------------------------------------------------------------------------

/// A packet forwarder: one EUI and, like the real thing, separate sockets for
/// PUSH_DATA and for PULL_DATA / TX_ACK.
struct FakeGateway {
    eui: Eui64,
    target: SocketAddr,
    push: UdpSocket,
    pull: UdpSocket,
}

impl FakeGateway {
    async fn new(n: u8, target: SocketAddr) -> Self {
        Self {
            eui: Eui64([0xAA, 0xBB, 0xCC, 0xDD, 0x00, 0x00, 0x00, n]),
            target,
            push: UdpSocket::bind("127.0.0.1:0").await.unwrap(),
            pull: UdpSocket::bind("127.0.0.1:0").await.unwrap(),
        }
    }

    async fn send_push(&self, datagram: &[u8]) {
        self.push.send_to(datagram, self.target).await.unwrap();
    }

    async fn send_pull(&self, datagram: &[u8]) {
        self.pull.send_to(datagram, self.target).await.unwrap();
    }

    async fn recv_push(&self) -> Vec<u8> {
        recv(&self.push).await
    }

    async fn recv_pull(&self) -> Vec<u8> {
        recv(&self.pull).await
    }

    /// Neither socket receives anything.
    async fn assert_quiet(&self) {
        assert_quiet(&self.push).await;
        assert_quiet(&self.pull).await;
    }

    /// Replace the PULL_DATA socket by a new one, as a restarted forwarder or
    /// a changed NAT mapping would, and return the old one.
    async fn rebind_pull(&mut self) -> UdpSocket {
        let new = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        std::mem::replace(&mut self.pull, new)
    }

    /// Likewise for the PUSH_DATA socket.
    async fn rebind_push(&mut self) -> UdpSocket {
        let new = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        std::mem::replace(&mut self.push, new)
    }
}

async fn recv(sock: &UdpSocket) -> Vec<u8> {
    let mut buf = vec![0u8; 65_536];
    let (len, _) = timeout(WAIT, sock.recv_from(&mut buf))
        .await
        .expect("timed out waiting for a datagram")
        .unwrap();
    buf.truncate(len);
    buf
}

async fn assert_quiet(sock: &UdpSocket) {
    let mut buf = vec![0u8; 65_536];
    if let Ok(Ok((len, from))) = timeout(QUIET, sock.recv_from(&mut buf)).await {
        panic!(
            "unexpected datagram from {from} at {}: {:02X?}",
            sock.local_addr().unwrap(),
            &buf[..len]
        );
    }
}

// ---------------------------------------------------------------------------
// Fake LNS
// ---------------------------------------------------------------------------

/// A datagram the fake LNS received.
struct Received {
    datagram: Vec<u8>,
    from: SocketAddr,
}

impl Received {
    fn kind(&self) -> u8 {
        self.datagram[3]
    }

    fn token(&self) -> [u8; 2] {
        [self.datagram[1], self.datagram[2]]
    }

    fn eui(&self) -> Eui64 {
        gateway_eui(&self.datagram).expect("datagram carries no gateway EUI")
    }
}

/// A network server: one socket, which notes the source ports each gateway
/// shows up from and can answer any of them.
struct FakeLns {
    sock: UdpSocket,
    /// Every source port seen per gateway EUI.
    ports: HashMap<Eui64, HashSet<u16>>,
    /// Latest source address seen per gateway EUI: where to answer it.
    peers: HashMap<Eui64, SocketAddr>,
}

impl FakeLns {
    async fn new() -> Self {
        Self {
            sock: UdpSocket::bind("127.0.0.1:0").await.unwrap(),
            ports: HashMap::new(),
            peers: HashMap::new(),
        }
    }

    fn addr(&self) -> SocketAddr {
        self.sock.local_addr().unwrap()
    }

    async fn recv(&mut self) -> Received {
        let mut buf = vec![0u8; 65_536];
        let (len, from) = timeout(WAIT, self.sock.recv_from(&mut buf))
            .await
            .expect("timed out waiting for the LNS to receive a datagram")
            .unwrap();
        buf.truncate(len);
        // ACKs carry no EUI; every datagram a gateway sends does.
        if let Some(eui) = gateway_eui(&buf) {
            self.ports.entry(eui).or_default().insert(from.port());
            self.peers.insert(eui, from);
        }
        Received {
            datagram: buf,
            from,
        }
    }

    async fn recv_n(&mut self, n: usize) -> Vec<Received> {
        let mut all = Vec::new();
        for _ in 0..n {
            all.push(self.recv().await);
        }
        all
    }

    /// Answer the gateway `eui` on the socket it last used.
    async fn reply(&self, eui: &Eui64, datagram: &[u8]) {
        let peer = *self
            .peers
            .get(eui)
            .expect("no datagram seen from that gateway");
        self.sock.send_to(datagram, peer).await.unwrap();
    }

    async fn assert_quiet(&self) {
        assert_quiet(&self.sock).await;
    }
}

// ---------------------------------------------------------------------------
// The proxy under test
// ---------------------------------------------------------------------------

/// Where the proxy under test forwards to.
enum Upstream {
    /// Listen mode.
    None,
    Lns(SocketAddr),
    /// The proxy's own gateway-facing socket: a misconfiguration the runtime
    /// loop guard must cut.
    Itself,
}

/// A proxy running on a loopback socket, with its records readable.
struct Sniffer {
    addr: SocketAddr,
    records: mpsc::Receiver<MessageRecord>,
    cancel: CancellationToken,
    task: JoinHandle<Result<()>>,
}

impl Sniffer {
    async fn start(config: UdpConfig, upstream: Upstream, filters: Vec<FilterRule>) -> Self {
        let gw_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = gw_sock.local_addr().unwrap();
        let upstream = match upstream {
            Upstream::None => None,
            Upstream::Lns(lns) => Some(lns),
            Upstream::Itself => Some(addr),
        };

        let (storage_tx, records) = mpsc::channel(1024);
        let (live_tx, _) = broadcast::channel(16);
        let outputs = Outputs::new(storage_tx, live_tx);
        let cancel = CancellationToken::new();
        let task = tokio::spawn({
            let cancel = cancel.clone();
            async move { run_with_socket(gw_sock, &config, upstream, &filters, outputs, cancel).await }
        });

        Self {
            addr,
            records,
            cancel,
            task,
        }
    }

    /// A proxy in front of `lns`, with default settings and no filters.
    async fn proxy(lns: &FakeLns) -> Self {
        Self::start(UdpConfig::default(), Upstream::Lns(lns.addr()), vec![]).await
    }

    async fn gateway(&self, n: u8) -> FakeGateway {
        FakeGateway::new(n, self.addr).await
    }

    async fn record(&mut self) -> MessageRecord {
        timeout(WAIT, self.records.recv())
            .await
            .expect("timed out waiting for a record")
            .expect("record channel closed")
    }

    async fn records(&mut self, n: usize) -> Vec<MessageRecord> {
        let mut all = Vec::new();
        for _ in 0..n {
            all.push(self.record().await);
        }
        all
    }

    async fn assert_no_record(&mut self) {
        if let Ok(Some(record)) = timeout(QUIET, self.records.recv()).await {
            panic!("unexpected record: {record:?}");
        }
    }

    async fn stop(self) {
        self.cancel.cancel();
        timeout(WAIT, self.task)
            .await
            .expect("the proxy did not stop")
            .unwrap()
            .unwrap();
    }
}

fn config_with(edit: impl FnOnce(&mut UdpConfig)) -> UdpConfig {
    let mut config = UdpConfig::default();
    edit(&mut config);
    config
}

fn rule(
    name: &str,
    direction: Option<Direction>,
    mtypes: Vec<MType>,
    suppress_ack: bool,
    gateways: Vec<Eui64>,
) -> FilterRule {
    FilterRule {
        name: name.to_string(),
        enabled: true,
        direction,
        suppress_ack,
        mtypes,
        gateways,
    }
}

/// Rule blocking join requests from every gateway.
fn block_joins() -> FilterRule {
    rule(
        "no-joins",
        Some(Direction::Up),
        vec![MType::JoinRequest],
        false,
        vec![],
    )
}

// ---------------------------------------------------------------------------
// Proxy mode: routing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn multi_gateway_traffic_reaches_lns() {
    let mut lns = FakeLns::new().await;
    let mut sniffer = Sniffer::proxy(&lns).await;
    let gateways = [
        sniffer.gateway(1).await,
        sniffer.gateway(2).await,
        sniffer.gateway(3).await,
    ];

    // Three rounds of all three datagram types from every gateway.
    let mut sent: HashMap<Eui64, Vec<Vec<u8>>> = HashMap::new();
    for round in 0u8..3 {
        for (i, gw) in gateways.iter().enumerate() {
            let tag = [round, i as u8];
            let datagrams = [
                push_data(tag, &gw.eui, &[data_up(0x1000 + i as u32, round as u16)]),
                pull_data(tag, &gw.eui),
                tx_ack(tag, &gw.eui),
            ];
            gw.send_push(&datagrams[0]).await;
            gw.send_pull(&datagrams[1]).await;
            gw.send_pull(&datagrams[2]).await;
            sent.entry(gw.eui).or_default().extend(datagrams);
        }
    }

    // Everything arrives, unmodified, and is attributed to the right gateway.
    let mut got: HashMap<Eui64, Vec<Vec<u8>>> = HashMap::new();
    for received in lns.recv_n(27).await {
        got.entry(received.eui())
            .or_default()
            .push(received.datagram);
    }
    assert_eq!(got.len(), 3);
    for gw in &gateways {
        let mut expected = sent[&gw.eui].clone();
        let mut actual = got[&gw.eui].clone();
        expected.sort();
        actual.sort();
        assert_eq!(actual, expected, "datagrams of gateway {}", gw.eui);
    }

    // Each gateway reaches the LNS from one port of its own, whatever it
    // sent and however often.
    for gw in &gateways {
        assert_eq!(
            lns.ports[&gw.eui].len(),
            1,
            "gateway {} changed port",
            gw.eui
        );
    }
    let all_ports: HashSet<u16> = lns.ports.values().flatten().copied().collect();
    assert_eq!(all_ports.len(), 3, "gateways share an LNS-facing port");

    // One record per uplink, three per gateway.
    let records = sniffer.records(9).await;
    for gw in &gateways {
        let own = records
            .iter()
            .filter(|r| r.gateway_eui.as_deref() == Some(gw.eui.to_string().as_str()))
            .count();
        assert_eq!(own, 3);
    }
    sniffer.stop().await;
}

#[tokio::test]
async fn same_token_acks_reach_their_own_gateway() {
    let mut lns = FakeLns::new().await;
    let sniffer = Sniffer::proxy(&lns).await;
    let (g1, g2) = (sniffer.gateway(1).await, sniffer.gateway(2).await);

    // Both gateways happen to pick the same token for everything.
    let token = [0x12, 0x34];
    g1.send_push(&push_data(token, &g1.eui, &[data_up(0x1001, 1)]))
        .await;
    g2.send_push(&push_data(token, &g2.eui, &[data_up(0x1002, 1)]))
        .await;
    g1.send_pull(&pull_data(token, &g1.eui)).await;
    g2.send_pull(&pull_data(token, &g2.eui)).await;
    lns.recv_n(4).await;

    // The LNS answers gateway 1 only: a token map would be ambiguous here.
    lns.reply(&g1.eui, &push_ack(token)).await;
    assert_eq!(g1.recv_push().await, push_ack(token));
    g2.assert_quiet().await;

    lns.reply(&g2.eui, &push_ack(token)).await;
    assert_eq!(g2.recv_push().await, push_ack(token));
    g1.assert_quiet().await;

    // PULL_ACK goes to the socket PULL_DATA came from, not the PUSH_DATA one.
    lns.reply(&g2.eui, &pull_ack(token)).await;
    assert_eq!(g2.recv_pull().await, pull_ack(token));
    g1.assert_quiet().await;
    assert_quiet(&g2.push).await;

    sniffer.stop().await;
}

#[tokio::test]
async fn pull_resp_reaches_only_addressed_gateway() {
    let mut lns = FakeLns::new().await;
    let mut sniffer = Sniffer::proxy(&lns).await;
    let (g1, g2) = (sniffer.gateway(1).await, sniffer.gateway(2).await);

    g1.send_pull(&pull_data([0, 1], &g1.eui)).await;
    g2.send_pull(&pull_data([0, 2], &g2.eui)).await;
    lns.recv_n(2).await;

    // A downlink for gateway 2.  Its token is the LNS's own, unrelated to
    // any token either gateway used.
    let downlink = pull_resp([0xBE, 0xEF], &data_down(0x2002, 7));
    lns.reply(&g2.eui, &downlink).await;
    assert_eq!(g2.recv_pull().await, downlink);
    g1.assert_quiet().await;
    assert_quiet(&g2.push).await;

    let record = sniffer.record().await;
    assert_eq!(record.direction, Direction::Down);
    assert_eq!(record.status, Status::Forwarded);
    assert_eq!(record.gateway_eui, Some(g2.eui.to_string()));

    // And one for gateway 1.
    let downlink = pull_resp([0xCA, 0xFE], &data_down(0x2001, 8));
    lns.reply(&g1.eui, &downlink).await;
    assert_eq!(g1.recv_pull().await, downlink);
    g2.assert_quiet().await;
    assert_eq!(sniffer.record().await.gateway_eui, Some(g1.eui.to_string()));

    sniffer.stop().await;
}

#[tokio::test]
async fn single_gateway_proxy_unchanged() {
    let mut lns = FakeLns::new().await;
    // A rule that does not match anything the gateway sends.
    let no_match = rule("never", None, vec![MType::RejoinRequest], false, vec![]);
    let mut sniffer = Sniffer::start(
        UdpConfig::default(),
        Upstream::Lns(lns.addr()),
        vec![no_match],
    )
    .await;
    let gw = sniffer.gateway(1).await;

    // Gateway to LNS: the exact bytes, all three types.
    let uplink = push_data([1, 1], &gw.eui, &[data_up(0x1001, 5)]);
    let keepalive = pull_data([2, 2], &gw.eui);
    let confirmation = tx_ack([3, 3], &gw.eui);
    gw.send_push(&uplink).await;
    gw.send_pull(&keepalive).await;
    gw.send_pull(&confirmation).await;
    let mut got = lns.recv_n(3).await;
    got.sort_by_key(|r| r.kind());
    assert_eq!(got[0].datagram, uplink);
    assert_eq!(got[1].datagram, keepalive);
    assert_eq!(got[2].datagram, confirmation);

    // LNS to gateway: the exact bytes, each on the right socket.
    lns.reply(&gw.eui, &push_ack([1, 1])).await;
    assert_eq!(gw.recv_push().await, push_ack([1, 1]));
    lns.reply(&gw.eui, &pull_ack([2, 2])).await;
    assert_eq!(gw.recv_pull().await, pull_ack([2, 2]));
    let downlink = pull_resp([9, 9], &data_down(0x2001, 6));
    lns.reply(&gw.eui, &downlink).await;
    assert_eq!(gw.recv_pull().await, downlink);

    // Both messages recorded as forwarded, nothing blocked.
    let records = sniffer.records(2).await;
    assert_eq!(records[0].direction, Direction::Up);
    assert_eq!(records[1].direction, Direction::Down);
    for record in &records {
        assert_eq!(record.status, Status::Forwarded);
        assert_eq!(record.blocked_by, None);
        assert_eq!(record.source_protocol, Protocol::Udp);
    }
    assert_eq!(records[0].dev_addr.as_deref(), Some("00001001"));
    assert_eq!(records[0].fcnt, Some(5));
    gw.assert_quiet().await;
    lns.assert_quiet().await;
    sniffer.stop().await;
}

#[tokio::test]
async fn udp_records_carry_gateway_eui() {
    let mut lns = FakeLns::new().await;
    let mut sniffer = Sniffer::proxy(&lns).await;
    let (g1, g2) = (sniffer.gateway(1).await, sniffer.gateway(2).await);

    // Each gateway has its own device, so a record's DevAddr says whose
    // traffic it was.
    g1.send_pull(&pull_data([0, 1], &g1.eui)).await;
    g2.send_pull(&pull_data([0, 2], &g2.eui)).await;
    g1.send_push(&push_data([1, 1], &g1.eui, &[data_up(0x1001, 1)]))
        .await;
    g2.send_push(&push_data([1, 2], &g2.eui, &[data_up(0x1002, 1)]))
        .await;
    lns.recv_n(4).await;
    lns.reply(&g1.eui, &pull_resp([5, 1], &data_down(0x2001, 1)))
        .await;
    lns.reply(&g2.eui, &pull_resp([5, 2], &data_down(0x2002, 1)))
        .await;

    let expected: HashMap<&str, String> = HashMap::from([
        ("00001001", g1.eui.to_string()),
        ("00001002", g2.eui.to_string()),
        ("00002001", g1.eui.to_string()),
        ("00002002", g2.eui.to_string()),
    ]);
    for record in sniffer.records(4).await {
        let dev_addr = record.dev_addr.clone().unwrap();
        assert_eq!(
            record.gateway_eui.as_ref(),
            Some(&expected[dev_addr.as_str()]),
            "record for {dev_addr}"
        );
    }
    // The format is the Display form of the EUI.
    assert_eq!(g1.eui.to_string(), "AA:BB:CC:DD:00:00:00:01");
    sniffer.stop().await;
}

// ---------------------------------------------------------------------------
// Proxy mode: blocking
// ---------------------------------------------------------------------------

#[tokio::test]
async fn blocked_messages_ack_through_their_gateway() {
    let mut lns = FakeLns::new().await;
    let downlink_rule = rule(
        "no-accepts",
        Some(Direction::Down),
        vec![MType::JoinAccept],
        false,
        vec![],
    );
    let mut sniffer = Sniffer::start(
        UdpConfig::default(),
        Upstream::Lns(lns.addr()),
        vec![block_joins(), downlink_rule],
    )
    .await;
    let (g1, g2) = (sniffer.gateway(1).await, sniffer.gateway(2).await);

    g1.send_pull(&pull_data([0, 1], &g1.eui)).await;
    g2.send_pull(&pull_data([0, 2], &g2.eui)).await;
    lns.recv_n(2).await;

    // Uplink: gateway 1's join request is blocked and gateway 1, not 2,
    // gets the synthetic PUSH_ACK, with its own token.
    g1.send_push(&push_data([0xA1, 0xA2], &g1.eui, &[join_request(1)]))
        .await;
    assert_eq!(g1.recv_push().await, push_ack([0xA1, 0xA2]));
    g2.assert_quiet().await;
    lns.assert_quiet().await;
    let record = sniffer.record().await;
    assert_eq!(record.status, Status::Blocked);
    assert_eq!(record.blocked_by.as_deref(), Some("no-joins"));
    assert_eq!(record.gateway_eui, Some(g1.eui.to_string()));

    // Downlink: a join accept for gateway 2 is blocked, never reaches it,
    // and the LNS receives a TX_ACK that looks like gateway 2's, on the
    // socket it uses for gateway 2.
    let g2_peer = lns.peers[&g2.eui];
    lns.reply(&g2.eui, &pull_resp([0xB1, 0xB2], &join_accept()))
        .await;
    let ack = lns.recv().await;
    assert_eq!(ack.kind(), TX_ACK);
    assert_eq!(ack.token(), [0xB1, 0xB2]);
    assert_eq!(ack.eui(), g2.eui);
    assert_eq!(ack.from, g2_peer);
    assert_eq!(ack.datagram, build_tx_ack([0xB1, 0xB2], &g2.eui));
    g1.assert_quiet().await;
    g2.assert_quiet().await;
    let record = sniffer.record().await;
    assert_eq!(record.status, Status::Blocked);
    assert_eq!(record.blocked_by.as_deref(), Some("no-accepts"));
    assert_eq!(record.gateway_eui, Some(g2.eui.to_string()));

    sniffer.stop().await;
}

#[tokio::test]
async fn stealth_blocks_send_no_acks() {
    let mut lns = FakeLns::new().await;
    let stealth_up = rule(
        "quiet-joins",
        Some(Direction::Up),
        vec![MType::JoinRequest],
        true,
        vec![],
    );
    let stealth_down = rule(
        "quiet-accepts",
        Some(Direction::Down),
        vec![MType::JoinAccept],
        true,
        vec![],
    );
    let mut sniffer = Sniffer::start(
        UdpConfig::default(),
        Upstream::Lns(lns.addr()),
        vec![stealth_up, stealth_down],
    )
    .await;
    let gw = sniffer.gateway(1).await;
    gw.send_pull(&pull_data([0, 1], &gw.eui)).await;
    lns.recv().await;

    gw.send_push(&push_data([1, 1], &gw.eui, &[join_request(1)]))
        .await;
    assert_eq!(sniffer.record().await.status, Status::Blocked);
    lns.reply(&gw.eui, &pull_resp([2, 2], &join_accept())).await;
    assert_eq!(sniffer.record().await.status, Status::Blocked);

    // No PUSH_ACK for the gateway, no TX_ACK for the LNS, nothing forwarded.
    gw.assert_quiet().await;
    lns.assert_quiet().await;
    sniffer.stop().await;
}

#[tokio::test]
async fn one_blocked_message_blocks_the_whole_datagram() {
    let lns = FakeLns::new().await;
    let mut sniffer = Sniffer::start(
        UdpConfig::default(),
        Upstream::Lns(lns.addr()),
        vec![block_joins()],
    )
    .await;
    let gw = sniffer.gateway(1).await;

    // One PUSH_DATA with a harmless uplink and a join request.
    gw.send_push(&push_data(
        [7, 7],
        &gw.eui,
        &[data_up(0x1001, 1), join_request(1)],
    ))
    .await;

    assert_eq!(gw.recv_push().await, push_ack([7, 7]));
    let records = sniffer.records(2).await;
    assert_eq!(records.len(), 2);
    assert_eq!(
        records
            .iter()
            .filter(|r| r.status == Status::Blocked)
            .count(),
        1
    );
    lns.assert_quiet().await;
    sniffer.stop().await;
}

#[tokio::test]
async fn gateway_rule_blocks_only_that_gateway() {
    let mut lns = FakeLns::new().await;
    let g1_eui = Eui64([0xAA, 0xBB, 0xCC, 0xDD, 0x00, 0x00, 0x00, 1]);
    let only_g1 = rule(
        "no-joins-on-g1",
        Some(Direction::Up),
        vec![MType::JoinRequest],
        false,
        vec![g1_eui],
    );
    let mut sniffer = Sniffer::start(
        UdpConfig::default(),
        Upstream::Lns(lns.addr()),
        vec![only_g1],
    )
    .await;
    let (g1, g2) = (sniffer.gateway(1).await, sniffer.gateway(2).await);
    assert_eq!(g1.eui, g1_eui);

    // The same join request through both gateways; gateway 1's goes first.
    g1.send_push(&push_data([1, 1], &g1.eui, &[join_request(9)]))
        .await;
    g2.send_push(&push_data([2, 2], &g2.eui, &[join_request(9)]))
        .await;

    // Gateway 1's is blocked and acknowledged, gateway 2's reaches the LNS.
    assert_eq!(g1.recv_push().await, push_ack([1, 1]));
    let forwarded = lns.recv().await;
    assert_eq!(forwarded.eui(), g2.eui);
    assert_eq!(forwarded.token(), [2, 2]);
    lns.assert_quiet().await;
    g2.assert_quiet().await;

    let records = sniffer.records(2).await;
    let of = |gw: &FakeGateway| {
        records
            .iter()
            .find(|r| r.gateway_eui.as_deref() == Some(gw.eui.to_string().as_str()))
            .unwrap()
    };
    assert_eq!(of(&g1).status, Status::Blocked);
    assert_eq!(of(&g1).blocked_by.as_deref(), Some("no-joins-on-g1"));
    assert_eq!(of(&g2).status, Status::Forwarded);
    assert_eq!(of(&g2).blocked_by, None);
    sniffer.stop().await;
}

// ---------------------------------------------------------------------------
// Proxy mode: sessions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn idle_session_released_and_recreated() {
    let mut lns = FakeLns::new().await;
    let config = config_with(|c| c.session_timeout = Duration::from_millis(200));
    let sniffer = Sniffer::start(config, Upstream::Lns(lns.addr()), vec![]).await;
    let gw = sniffer.gateway(1).await;

    gw.send_push(&push_data([1, 1], &gw.eui, &[data_up(0x1001, 1)]))
        .await;
    let first_port = lns.recv().await.from.port();
    lns.reply(&gw.eui, &push_ack([1, 1])).await;
    assert_eq!(gw.recv_push().await, push_ack([1, 1]));

    // Fall silent for several timeouts and sweep intervals.  A new socket
    // may by chance be given the old port number, so go around again in that
    // (very unlikely) case rather than fail.
    let mut old_port = first_port;
    let mut new_port = old_port;
    for attempt in 0u8..3 {
        tokio::time::sleep(Duration::from_millis(700)).await;
        let token = [2, attempt];
        gw.send_push(&push_data(token, &gw.eui, &[data_up(0x1001, 2)]))
            .await;
        new_port = lns.recv().await.from.port();
        if new_port != old_port {
            break;
        }
        old_port = new_port;
    }
    assert_ne!(new_port, old_port, "the upstream socket was never replaced");

    // Traffic flows through the new socket, and the old one is gone.
    let token = [3, 3];
    gw.send_push(&push_data(token, &gw.eui, &[data_up(0x1001, 3)]))
        .await;
    assert_eq!(lns.recv().await.from.port(), new_port);
    lns.reply(&gw.eui, &push_ack(token)).await;
    assert_eq!(gw.recv_push().await, push_ack(token));
    let old_peer = SocketAddr::from(([127, 0, 0, 1], old_port));
    lns.sock.send_to(&push_ack([4, 4]), old_peer).await.unwrap();
    gw.assert_quiet().await;

    sniffer.stop().await;
}

#[tokio::test]
async fn released_session_makes_room_for_a_new_gateway() {
    let mut lns = FakeLns::new().await;
    let config = config_with(|c| {
        c.session_timeout = Duration::from_millis(200);
        c.max_gateways = 1;
    });
    let sniffer = Sniffer::start(config, Upstream::Lns(lns.addr()), vec![]).await;
    let (g1, g2) = (sniffer.gateway(1).await, sniffer.gateway(2).await);

    g1.send_pull(&pull_data([1, 1], &g1.eui)).await;
    assert_eq!(lns.recv().await.eui(), g1.eui);

    // While gateway 1 is active the table is full ...
    g2.send_pull(&pull_data([2, 2], &g2.eui)).await;
    g1.send_pull(&pull_data([1, 2], &g1.eui)).await;
    assert_eq!(lns.recv().await.eui(), g1.eui);
    lns.assert_quiet().await;

    // ... and once it has been silent for long enough, gateway 2 gets in.
    tokio::time::sleep(Duration::from_millis(700)).await;
    g2.send_pull(&pull_data([2, 3], &g2.eui)).await;
    assert_eq!(lns.recv().await.eui(), g2.eui);
    sniffer.stop().await;
}

#[tokio::test]
async fn address_change_follows_eui() {
    let mut lns = FakeLns::new().await;
    let sniffer = Sniffer::proxy(&lns).await;
    let mut gw = sniffer.gateway(1).await;

    gw.send_push(&push_data([1, 1], &gw.eui, &[data_up(0x1001, 1)]))
        .await;
    gw.send_pull(&pull_data([1, 2], &gw.eui)).await;
    lns.recv_n(2).await;
    let peer = lns.peers[&gw.eui];

    // The forwarder restarts: both its sockets have new ports.
    let old_pull = gw.rebind_pull().await;
    let old_push = gw.rebind_push().await;
    gw.send_push(&push_data([2, 1], &gw.eui, &[data_up(0x1001, 2)]))
        .await;
    gw.send_pull(&pull_data([2, 2], &gw.eui)).await;
    let again = lns.recv_n(2).await;

    // The LNS sees the same peer: the session followed the EUI.
    assert!(again.iter().all(|r| r.from == peer));
    assert_eq!(lns.ports[&gw.eui].len(), 1);

    // ACKs and downlinks go to the new ports only.
    lns.reply(&gw.eui, &pull_ack([2, 2])).await;
    assert_eq!(gw.recv_pull().await, pull_ack([2, 2]));
    let downlink = pull_resp([8, 8], &data_down(0x2001, 1));
    lns.reply(&gw.eui, &downlink).await;
    assert_eq!(gw.recv_pull().await, downlink);
    lns.reply(&gw.eui, &push_ack([2, 1])).await;
    assert_eq!(gw.recv_push().await, push_ack([2, 1]));
    assert_quiet(&old_pull).await;
    assert_quiet(&old_push).await;

    sniffer.stop().await;
}

#[tokio::test]
async fn session_cap_refuses_new_gateways() {
    let mut lns = FakeLns::new().await;
    let config = config_with(|c| c.max_gateways = 2);
    let mut sniffer = Sniffer::start(config, Upstream::Lns(lns.addr()), vec![]).await;
    let (g1, g2, g3) = (
        sniffer.gateway(1).await,
        sniffer.gateway(2).await,
        sniffer.gateway(3).await,
    );

    g1.send_pull(&pull_data([1, 1], &g1.eui)).await;
    g2.send_pull(&pull_data([2, 2], &g2.eui)).await;
    lns.recv_n(2).await;

    // The third gateway is refused: neither its keepalive nor its uplink is
    // forwarded or recorded.  Gateway 1's uplink behind them proves they
    // were handled.
    g3.send_pull(&pull_data([3, 3], &g3.eui)).await;
    g3.send_push(&push_data([3, 4], &g3.eui, &[data_up(0x1003, 1)]))
        .await;
    g1.send_push(&push_data([1, 2], &g1.eui, &[data_up(0x1001, 1)]))
        .await;
    let forwarded = lns.recv().await;
    assert_eq!(forwarded.eui(), g1.eui);
    lns.assert_quiet().await;
    assert!(!lns.peers.contains_key(&g3.eui));
    g3.assert_quiet().await;
    let record = sniffer.record().await;
    assert_eq!(record.gateway_eui, Some(g1.eui.to_string()));
    sniffer.assert_no_record().await;

    // The two gateways that were in keep working in both directions.
    for gw in [&g1, &g2] {
        gw.send_push(&push_data([5, 5], &gw.eui, &[data_up(0x1000, 2)]))
            .await;
        gw.send_pull(&pull_data([5, 6], &gw.eui)).await;
        let arrived = lns.recv_n(2).await;
        assert!(arrived.iter().all(|r| r.eui() == gw.eui));
        lns.reply(&gw.eui, &push_ack([5, 5])).await;
        assert_eq!(gw.recv_push().await, push_ack([5, 5]));
        lns.reply(&gw.eui, &pull_ack([5, 6])).await;
        assert_eq!(gw.recv_pull().await, pull_ack([5, 6]));
    }
    sniffer.stop().await;
}

#[tokio::test]
async fn refused_upstream_does_not_stop_the_proxy() {
    // An LNS port with nobody listening: every datagram sent to it makes the
    // kernel report "connection refused" on the sending socket.
    let closed = {
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        sock.local_addr().unwrap()
    };
    let mut sniffer = Sniffer::start(UdpConfig::default(), Upstream::Lns(closed), vec![]).await;
    let gw = sniffer.gateway(1).await;

    // The proxy keeps recording uplinks the whole time, so neither the send
    // nor the receive errors ended it.
    for fcnt in 0u16..6 {
        gw.send_push(&push_data(
            [0, fcnt as u8],
            &gw.eui,
            &[data_up(0x1001, fcnt)],
        ))
        .await;
        assert_eq!(sniffer.record().await.fcnt, Some(fcnt as u32));
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    sniffer.stop().await;
}

// ---------------------------------------------------------------------------
// Listen mode
// ---------------------------------------------------------------------------

fn listen_config(listen_acks: bool) -> UdpConfig {
    config_with(|c| {
        c.mode = UdpMode::Listen;
        c.listen_acks = listen_acks;
    })
}

#[tokio::test]
async fn listen_mode_captures_and_sends_nothing_upstream() {
    // A network server that the configuration still points at: a listening
    // sniffer must leave it alone.
    let lns = FakeLns::new().await;
    let mut config = listen_config(true);
    config.upstream.server = "127.0.0.1".to_string();
    config.upstream.port = lns.addr().port();
    // A rule that would block everything it can; listening ignores rules.
    let block_all = rule("block-all", None, vec![], false, vec![]);
    let mut sniffer = Sniffer::start(config, Upstream::None, vec![block_all]).await;
    let gw = sniffer.gateway(1).await;

    gw.send_pull(&pull_data([1, 1], &gw.eui)).await;
    gw.send_pull(&tx_ack([1, 2], &gw.eui)).await;
    gw.send_push(&push_data(
        [1, 3],
        &gw.eui,
        &[data_up(0x1001, 4), join_request(2)],
    ))
    .await;

    let records = sniffer.records(2).await;
    assert_eq!(records[0].mtype, MType::UnconfirmedDataUp);
    assert_eq!(records[1].mtype, MType::JoinRequest);
    for record in &records {
        assert_eq!(record.status, Status::Captured);
        assert_eq!(record.blocked_by, None);
        assert_eq!(record.gateway_eui, Some(gw.eui.to_string()));
    }
    lns.assert_quiet().await;
    sniffer.assert_no_record().await;
    sniffer.stop().await;
}

#[tokio::test]
async fn listen_acks_on_and_off() {
    // On: each ACK echoes the token and goes to the socket that asked.
    let mut sniffer = Sniffer::start(listen_config(true), Upstream::None, vec![]).await;
    let gw = sniffer.gateway(1).await;
    gw.send_pull(&pull_data([0x0A, 0x0B], &gw.eui)).await;
    gw.send_pull(&tx_ack([0x0C, 0x0D], &gw.eui)).await;
    gw.send_push(&push_data([0x01, 0x02], &gw.eui, &[data_up(0x1001, 1)]))
        .await;
    assert_eq!(gw.recv_push().await, push_ack([0x01, 0x02]));
    assert_eq!(gw.recv_pull().await, pull_ack([0x0A, 0x0B]));
    // Nothing else: no reply to the TX_ACK and no downlink of any kind.
    gw.assert_quiet().await;
    assert_eq!(sniffer.record().await.status, Status::Captured);
    sniffer.stop().await;

    // Off: the same traffic is recorded but nobody answers, for as long as it
    // takes a gateway to notice.
    let mut sniffer = Sniffer::start(listen_config(false), Upstream::None, vec![]).await;
    let gw = sniffer.gateway(1).await;
    gw.send_pull(&pull_data([0x0A, 0x0B], &gw.eui)).await;
    gw.send_push(&push_data([0x01, 0x02], &gw.eui, &[data_up(0x1001, 1)]))
        .await;
    // The record proves both datagrams have been handled.
    assert_eq!(sniffer.record().await.status, Status::Captured);
    let (mut push_buf, mut pull_buf) = ([0u8; 64], [0u8; 64]);
    timeout(Duration::from_millis(500), async {
        tokio::select! {
            r = gw.push.recv_from(&mut push_buf) => r.map(|(n, _)| push_buf[..n].to_vec()),
            r = gw.pull.recv_from(&mut pull_buf) => r.map(|(n, _)| pull_buf[..n].to_vec()),
        }
    })
    .await
    .expect_err("a gateway got an answer although listen_acks is off");
    sniffer.stop().await;
}

// ---------------------------------------------------------------------------
// Malformed frames
// ---------------------------------------------------------------------------

/// The EUI of the gateway `FakeGateway::new(1, ..)` plays.
const GATEWAY_1: Eui64 = Eui64([0xAA, 0xBB, 0xCC, 0xDD, 0x00, 0x00, 0x00, 1]);

/// A JoinAccept of 10 bytes; a real one has 17 or 33.
fn short_join_accept() -> Vec<u8> {
    let mut frame = vec![0x20];
    frame.extend_from_slice(&[0x55; 9]);
    frame
}

/// Noise that reads as a JoinRequest: a CRC-failed 20-byte frame starting with
/// the JoinRequest MHDR.
fn noise_reading_as_a_join_request() -> Vec<u8> {
    let mut frame = vec![0x00];
    frame.extend((1..20).map(|i| (i * 13) as u8));
    frame
}

fn rxpk_of(frame: &[u8], stat: i8) -> RxPk {
    RxPk {
        freq: 868.1,
        datr: "SF7BW125".to_string(),
        codr: "4/5".to_string(),
        rssi: Some(-50),
        lsnr: Some(7.5),
        stat,
        size: frame.len() as u16,
        data: BASE64_STANDARD.encode(frame),
    }
}

fn txpk_of(frame: &[u8]) -> TxPk {
    TxPk {
        freq: 869.525,
        datr: "SF9BW125".to_string(),
        codr: "4/5".to_string(),
        size: frame.len() as u16,
        data: BASE64_STANDARD.encode(frame),
    }
}

#[test]
fn a_failed_crc_makes_the_uplink_record_malformed_and_clears_its_fields() {
    // The bytes read as a data frame from DevAddr 6874FA22, which is what a
    // failed CRC would otherwise have put in the table.
    let frame = data_up(0x6874FA22, 5);
    let (rec, verdict) = build_uplink_record(&rxpk_of(&frame, -1), GATEWAY_1, false, &[]);

    assert_eq!(rec.malformed, Some(Malformed::Crc));
    assert_body_cleared(&rec);

    // What was measured about the reception stays.
    assert_eq!(rec.direction, Direction::Up);
    assert_eq!(rec.mtype, MType::UnconfirmedDataUp);
    assert_eq!(rec.freq_mhz, 868.1);
    assert_eq!(rec.datarate, "SF7BW125");
    assert_eq!(rec.rssi, Some(-50));
    assert_eq!(rec.snr, Some(7.5));
    assert_eq!(rec.crc_status, Some(CrcStatus::Fail));
    assert_eq!(rec.payload_size, 15);
    assert!(rec.toa_ms.is_some_and(|t| t > 0.0), "{:?}", rec.toa_ms);
    assert_eq!(rec.gateway_eui.as_deref(), Some("AA:BB:CC:DD:00:00:00:01"));
    assert_eq!(rec.status, Status::Forwarded);
    assert_eq!(verdict, Verdict::Forward);
}

#[test]
fn an_uplink_without_a_failed_crc_keeps_its_fields() {
    // The same bytes, with every other status a gateway can report.
    let frame = data_up(0x6874FA22, 5);
    for (stat, crc) in [
        (1, Some(CrcStatus::Ok)),
        (0, Some(CrcStatus::NoCrc)),
        (7, None),
    ] {
        let (rec, verdict) = build_uplink_record(&rxpk_of(&frame, stat), GATEWAY_1, false, &[]);

        assert_eq!(rec.malformed, None, "stat {stat}");
        assert_eq!(rec.crc_status, crc, "stat {stat}");
        assert_eq!(rec.dev_addr.as_deref(), Some("6874FA22"), "stat {stat}");
        assert_eq!(rec.fcnt, Some(5), "stat {stat}");
        assert_eq!(rec.fport, Some(1), "stat {stat}");
        assert_eq!(rec.f_opts_len, Some(0), "stat {stat}");
        assert_eq!(verdict, Verdict::Forward, "stat {stat}");
    }
}

#[test]
fn a_downlink_of_the_wrong_length_is_a_malformed_record() {
    let frame = short_join_accept();
    let (rec, verdict) = build_downlink_record(&txpk_of(&frame), GATEWAY_1, &[]);

    assert_eq!(rec.malformed, Some(Malformed::Length));
    assert_body_cleared(&rec);
    assert_eq!(rec.direction, Direction::Down);
    assert_eq!(rec.mtype, MType::JoinAccept);
    // A downlink has no CRC to report.
    assert_eq!(rec.crc_status, None);
    assert_eq!(rec.payload_size, 10);
    assert_eq!(rec.freq_mhz, 869.525);
    assert_eq!(rec.datarate, "SF9BW125");
    assert_eq!(rec.gateway_eui.as_deref(), Some("AA:BB:CC:DD:00:00:00:01"));
    assert_eq!(rec.status, Status::Forwarded);
    assert_eq!(verdict, Verdict::Forward);
}

#[tokio::test]
async fn malformed_frames_are_forwarded_byte_for_byte() {
    let mut lns = FakeLns::new().await;
    let mut sniffer = Sniffer::proxy(&lns).await;
    let gw = sniffer.gateway(1).await;
    assert_eq!(gw.eui, GATEWAY_1);

    // Noise, a bad header, a short frame and a valid frame in one PUSH_DATA
    // reach the LNS as they were sent, and its answer reaches the gateway.
    let uplink = mixed_push_data([1, 1], &gw.eui);
    gw.send_push(&uplink).await;
    assert_eq!(lns.recv().await.datagram, uplink);
    lns.reply(&gw.eui, &push_ack([1, 1])).await;
    assert_eq!(gw.recv_push().await, push_ack([1, 1]));

    // A malformed downlink reaches the gateway as it was sent, and the
    // gateway's TX_ACK reaches the LNS.
    gw.send_pull(&pull_data([2, 2], &gw.eui)).await;
    assert_eq!(lns.recv().await.kind(), PULL_DATA);
    let downlink = pull_resp([3, 3], &short_join_accept());
    lns.reply(&gw.eui, &downlink).await;
    assert_eq!(gw.recv_pull().await, downlink);
    let confirmation = tx_ack([3, 3], &gw.eui);
    gw.send_pull(&confirmation).await;
    assert_eq!(lns.recv().await.datagram, confirmation);

    let expected = mixed_uplinks();
    let records = sniffer.records(expected.len() + 1).await;
    for (record, (_, _, reason)) in records.iter().zip(&expected) {
        assert_eq!(record.direction, Direction::Up);
        assert_eq!(record.status, Status::Forwarded);
        assert_eq!(record.blocked_by, None);
        assert_eq!(record.malformed, *reason);
        match reason {
            Some(_) => assert_body_cleared(record),
            None => assert_eq!(record.dev_addr.as_deref(), Some("00001001")),
        }
        // The noise's fake DevAddr is in no record.
        assert_ne!(record.dev_addr.as_deref(), Some("6874FA22"));
    }
    let record = records.last().unwrap();
    assert_eq!(record.direction, Direction::Down);
    assert_eq!(record.status, Status::Forwarded);
    assert_eq!(record.malformed, Some(Malformed::Length));
    assert_body_cleared(record);

    gw.assert_quiet().await;
    lns.assert_quiet().await;
    sniffer.stop().await;
}

/// A rule listing no types that blocks everything coming from gateway 1.
fn block_gateway_1() -> FilterRule {
    rule("only-g1", None, vec![], false, vec![GATEWAY_1])
}

/// A rule listing no types that blocks every uplink.
fn block_uplinks() -> FilterRule {
    rule("all-up", Some(Direction::Up), vec![], false, vec![])
}

#[tokio::test]
async fn type_rules_do_not_match_malformed_frames() {
    // CRC-failed noise whose first byte reads as an UnconfirmedDataUp.
    let noise = lns01_noise().pop().unwrap();
    assert_eq!(noise[0], 0x40);
    let no_unconfirmed = rule(
        "no-unconfirmed",
        Some(Direction::Up),
        vec![MType::UnconfirmedDataUp],
        false,
        vec![],
    );

    // A rule on the type it reads as lets the noise through untouched, and
    // the LNS's own PUSH_ACK reaches the gateway.
    let mut lns = FakeLns::new().await;
    let mut sniffer = Sniffer::start(
        UdpConfig::default(),
        Upstream::Lns(lns.addr()),
        vec![no_unconfirmed],
    )
    .await;
    let gw = sniffer.gateway(1).await;
    let datagram = push_data_with([1, 1], &gw.eui, &[(noise.clone(), -1)]);
    gw.send_push(&datagram).await;
    assert_eq!(lns.recv().await.datagram, datagram);
    lns.reply(&gw.eui, &push_ack([1, 1])).await;
    assert_eq!(gw.recv_push().await, push_ack([1, 1]));
    let record = sniffer.record().await;
    assert_eq!(record.status, Status::Forwarded);
    assert_eq!(record.blocked_by, None);
    assert_eq!(record.malformed, Some(Malformed::Crc));
    assert_eq!(record.mtype, MType::UnconfirmedDataUp);

    // The rule does work on a valid frame of that type: it is held back and
    // the gateway gets the proxy's own PUSH_ACK.
    gw.send_push(&push_data([2, 2], &gw.eui, &[data_up(0x1001, 1)]))
        .await;
    assert_eq!(gw.recv_push().await, push_ack([2, 2]));
    lns.assert_quiet().await;
    let record = sniffer.record().await;
    assert_eq!(record.status, Status::Blocked);
    assert_eq!(record.blocked_by.as_deref(), Some("no-unconfirmed"));
    sniffer.stop().await;

    // A rule on the type a malformed downlink reads as lets it through, too.
    let no_accepts = rule(
        "no-accepts",
        Some(Direction::Down),
        vec![MType::JoinAccept],
        false,
        vec![],
    );
    let mut lns = FakeLns::new().await;
    let mut sniffer = Sniffer::start(
        UdpConfig::default(),
        Upstream::Lns(lns.addr()),
        vec![no_accepts],
    )
    .await;
    let gw = sniffer.gateway(1).await;
    gw.send_pull(&pull_data([3, 3], &gw.eui)).await;
    lns.recv().await;
    let downlink = pull_resp([4, 4], &short_join_accept());
    lns.reply(&gw.eui, &downlink).await;
    assert_eq!(gw.recv_pull().await, downlink);
    let record = sniffer.record().await;
    assert_eq!(record.status, Status::Forwarded);
    assert_eq!(record.malformed, Some(Malformed::Length));
    // A valid JoinAccept is blocked, and the LNS is told so.
    lns.reply(&gw.eui, &pull_resp([5, 5], &join_accept())).await;
    assert_eq!(lns.recv().await.datagram, build_tx_ack([5, 5], &gw.eui));
    gw.assert_quiet().await;
    assert_eq!(sniffer.record().await.status, Status::Blocked);
    sniffer.stop().await;

    // Rules that name no type still match the noise: the frame is held back
    // and the proxy answers the gateway itself.
    for blocking in [block_uplinks(), block_gateway_1()] {
        let name = blocking.name.clone();
        let lns = FakeLns::new().await;
        let mut sniffer = Sniffer::start(
            UdpConfig::default(),
            Upstream::Lns(lns.addr()),
            vec![blocking],
        )
        .await;
        let gw = sniffer.gateway(1).await;
        gw.send_push(&push_data_with([6, 6], &gw.eui, &[(noise.clone(), -1)]))
            .await;
        assert_eq!(gw.recv_push().await, push_ack([6, 6]), "{name}");
        lns.assert_quiet().await;
        let record = sniffer.record().await;
        assert_eq!(record.status, Status::Blocked, "{name}");
        assert_eq!(record.blocked_by.as_deref(), Some(name.as_str()));
        assert_eq!(record.malformed, Some(Malformed::Crc), "{name}");
        sniffer.stop().await;
    }
}

#[tokio::test]
async fn a_malformed_frame_does_not_hold_back_its_datagram() {
    let noise = noise_reading_as_a_join_request();
    let valid = data_up(0x1001, 1);
    let mixed = |token: [u8; 2], eui: &Eui64| {
        push_data_with(token, eui, &[(noise.clone(), -1), (valid.clone(), 1)])
    };

    // Under a rule on JoinRequests, noise that reads as one does not stop
    // the valid frame next to it: the datagram reaches the LNS as sent and
    // the LNS answers it.
    let mut lns = FakeLns::new().await;
    let mut sniffer = Sniffer::start(
        UdpConfig::default(),
        Upstream::Lns(lns.addr()),
        vec![block_joins()],
    )
    .await;
    let gw = sniffer.gateway(1).await;
    let datagram = mixed([1, 1], &gw.eui);
    gw.send_push(&datagram).await;
    assert_eq!(lns.recv().await.datagram, datagram);
    lns.reply(&gw.eui, &push_ack([1, 1])).await;
    assert_eq!(gw.recv_push().await, push_ack([1, 1]));
    let records = sniffer.records(2).await;
    for record in &records {
        assert_eq!(record.status, Status::Forwarded);
        assert_eq!(record.blocked_by, None);
    }
    assert_eq!(records[0].mtype, MType::JoinRequest);
    assert_eq!(records[0].malformed, Some(Malformed::Crc));
    assert_eq!(records[1].malformed, None);
    assert_eq!(records[1].dev_addr.as_deref(), Some("00001001"));

    // Control: a valid JoinRequest next to a valid frame holds the whole
    // datagram back, and the proxy answers the gateway itself.  The valid
    // frame's own record says it was forwarded.
    gw.send_push(&push_data(
        [2, 2],
        &gw.eui,
        &[join_request(1), valid.clone()],
    ))
    .await;
    assert_eq!(gw.recv_push().await, push_ack([2, 2]));
    lns.assert_quiet().await;
    let records = sniffer.records(2).await;
    assert_eq!(records[0].status, Status::Blocked);
    assert_eq!(records[0].blocked_by.as_deref(), Some("no-joins"));
    assert_eq!(records[1].status, Status::Forwarded);
    assert_eq!(records[1].blocked_by, None);
    sniffer.stop().await;

    // Controls: rules that name no type match the noise, so they still hold
    // back the whole datagram, and both frames are recorded as blocked.
    for blocking in [block_uplinks(), block_gateway_1()] {
        let name = blocking.name.clone();
        let lns = FakeLns::new().await;
        let mut sniffer = Sniffer::start(
            UdpConfig::default(),
            Upstream::Lns(lns.addr()),
            vec![blocking],
        )
        .await;
        let gw = sniffer.gateway(1).await;
        gw.send_push(&mixed([3, 3], &gw.eui)).await;
        assert_eq!(gw.recv_push().await, push_ack([3, 3]), "{name}");
        lns.assert_quiet().await;
        let records = sniffer.records(2).await;
        for record in &records {
            assert_eq!(record.status, Status::Blocked, "{name}");
            assert_eq!(record.blocked_by.as_deref(), Some(name.as_str()));
        }
        assert_eq!(records[0].malformed, Some(Malformed::Crc), "{name}");
        sniffer.stop().await;
    }
}

#[tokio::test]
async fn listen_mode_captures_malformed_frames() {
    let mut sniffer = Sniffer::start(listen_config(true), Upstream::None, vec![]).await;
    let gw = sniffer.gateway(1).await;

    gw.send_pull(&pull_data([1, 1], &gw.eui)).await;
    gw.send_push(&mixed_push_data([1, 2], &gw.eui)).await;

    // The listen ACKs are the same as for any other traffic.
    assert_eq!(gw.recv_push().await, push_ack([1, 2]));
    assert_eq!(gw.recv_pull().await, pull_ack([1, 1]));

    let expected = mixed_uplinks();
    let records = sniffer.records(expected.len()).await;
    for (record, (_, _, reason)) in records.iter().zip(&expected) {
        assert_eq!(record.status, Status::Captured);
        assert_eq!(record.blocked_by, None);
        assert_eq!(record.malformed, *reason);
        if reason.is_some() {
            assert_body_cleared(record);
        }
    }
    gw.assert_quiet().await;
    sniffer.assert_no_record().await;
    sniffer.stop().await;
}

// ---------------------------------------------------------------------------
// Self-forwarding
// ---------------------------------------------------------------------------

#[tokio::test]
async fn self_loop_is_cut_at_runtime() {
    // The upstream is the proxy's own gateway-facing socket, which the
    // startup check would have refused: whatever is forwarded comes straight
    // back as a datagram from one of the proxy's own LNS sockets.
    let mut sniffer = Sniffer::start(UdpConfig::default(), Upstream::Itself, vec![]).await;
    let gw = sniffer.gateway(1).await;

    gw.send_push(&push_data([1, 1], &gw.eui, &[data_up(0x1001, 1)]))
        .await;
    gw.send_pull(&pull_data([1, 2], &gw.eui)).await;

    // The original uplink is recorded once; its looped copy never is.
    let record = sniffer.record().await;
    assert_eq!(record.dev_addr.as_deref(), Some("00001001"));
    assert_eq!(record.status, Status::Forwarded);
    sniffer.assert_no_record().await;

    // The guard has not shut the gateway out: new traffic still gets through.
    gw.send_push(&push_data([2, 1], &gw.eui, &[data_up(0x1001, 2)]))
        .await;
    assert_eq!(sniffer.record().await.fcnt, Some(2));
    sniffer.assert_no_record().await;
    sniffer.stop().await;
}

#[tokio::test]
async fn datagram_from_own_lns_socket_is_dropped_without_being_an_echo() {
    // Drive the handler directly, so the datagram can come from a source the
    // proxy really owns while being nothing it ever sent upstream.
    let mut lns = FakeLns::new().await;
    let gw_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let (storage_tx, mut records) = mpsc::channel(8);
    let (live_tx, _) = broadcast::channel(1);
    let (mut proxy, _lns_rx) = UdpProxy::new(
        gw_sock,
        &UdpConfig::default(),
        Some(lns.addr()),
        &[],
        Outputs::new(storage_tx, live_tx),
    );
    let eui = Eui64([0xAA, 0xBB, 0xCC, 0xDD, 0x00, 0x00, 0x00, 1]);
    let gateway_addr: SocketAddr = "127.0.0.1:50000".parse().unwrap();

    // A keepalive creates the gateway's session and its LNS socket.
    proxy
        .handle_from_gateway(&pull_data([1, 1], &eui), gateway_addr)
        .await;
    lns.recv().await;
    let own_port = proxy
        .sessions
        .get(&eui)
        .unwrap()
        .upstream
        .as_ref()
        .unwrap()
        .local_addr()
        .unwrap()
        .port();

    // An uplink "from" that LNS socket: not a repeat of anything forwarded,
    // but it cannot have come from a gateway.
    let uplink = push_data([2, 2], &eui, &[data_up(0x1001, 1)]);
    proxy
        .handle_from_gateway(&uplink, SocketAddr::from(([127, 0, 0, 1], own_port)))
        .await;
    assert!(
        records.try_recv().is_err(),
        "a record was made from our own traffic"
    );
    lns.assert_quiet().await;

    // The same bytes from an ordinary source are an ordinary uplink.
    proxy.handle_from_gateway(&uplink, gateway_addr).await;
    assert!(records.try_recv().is_ok());
    assert_eq!(lns.recv().await.datagram, uplink);
}

#[tokio::test]
async fn address_rewriting_loop_is_cut_by_the_echo_check() {
    // The "upstream" hands every datagram back to the proxy from a different
    // socket, as Docker port forwarding or NAT hairpinning would: the source
    // is not one of the proxy's ports, but the bytes are the same.
    let relay_in = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let relay_out = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut sniffer = Sniffer::start(
        UdpConfig::default(),
        Upstream::Lns(relay_in.local_addr().unwrap()),
        vec![],
    )
    .await;
    let gw = sniffer.gateway(1).await;

    let uplink = push_data([1, 1], &gw.eui, &[data_up(0x1001, 1)]);
    gw.send_push(&uplink).await;
    let forwarded = recv(&relay_in).await;
    assert_eq!(forwarded, uplink);
    relay_out.send_to(&forwarded, sniffer.addr).await.unwrap();

    // One record for the original, none for the copy, no second forward.
    assert_eq!(sniffer.record().await.fcnt, Some(1));
    sniffer.assert_no_record().await;
    assert_quiet(&relay_in).await;

    // A different datagram from the same gateway is unaffected.
    let next = push_data([1, 2], &gw.eui, &[data_up(0x1001, 2)]);
    gw.send_push(&next).await;
    assert_eq!(recv(&relay_in).await, next);
    assert_eq!(sniffer.record().await.fcnt, Some(2));
    sniffer.stop().await;
}

// ---------------------------------------------------------------------------
// Outputs that cannot keep up
// ---------------------------------------------------------------------------

/// PUSH_DATA datagrams sent in the flood tests.
const FLOOD: usize = 5_000;

/// Gateways the flood is spread over.
const FLOOD_GATEWAYS: usize = 4;

/// Datagrams sent before the test waits for them to come out the other side.
/// The proxy's socket is an ordinary kernel receive buffer that holds a couple
/// of hundred small datagrams; a sender that gets further ahead would lose
/// datagrams in the kernel, which says nothing about the proxy.
const FLOOD_WINDOW: usize = 50;

/// How long the whole flood may take to reach the LNS.
const FLOOD_DEADLINE: Duration = Duration::from_secs(20);

/// Outputs whose storage queue is emptied as fast as it fills, plus the number
/// of records that reached it.
fn outputs_with_storage_drain() -> (Outputs, Arc<AtomicUsize>) {
    let (storage_tx, mut storage_rx) = mpsc::channel(1024);
    let (live_tx, _) = broadcast::channel(16);
    let stored = Arc::new(AtomicUsize::new(0));
    tokio::spawn({
        let stored = Arc::clone(&stored);
        async move {
            while storage_rx.recv().await.is_some() {
                stored.fetch_add(1, Ordering::SeqCst);
            }
        }
    });
    (Outputs::new(storage_tx, live_tx), stored)
}

/// Poll `condition` until it holds, failing the test after `WAIT`.
async fn eventually(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Run a proxy in front of a fake LNS on `outputs`, send it [`FLOOD`] distinct
/// uplinks from several gateways as fast as the kernel buffers allow, and
/// return how long it took for every one of them to reach the LNS.
///
/// Fails the test if any uplink is missing, repeated or altered on the way.
async fn flood_through_proxy(outputs: Outputs) -> Duration {
    let mut lns = FakeLns::new().await;
    let gw_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = gw_sock.local_addr().unwrap();
    let lns_addr = lns.addr();
    let cancel = CancellationToken::new();
    let task = tokio::spawn({
        let cancel = cancel.clone();
        async move {
            run_with_socket(
                gw_sock,
                &UdpConfig::default(),
                Some(lns_addr),
                &[],
                outputs,
                cancel,
            )
            .await
        }
    });

    let mut gateways = Vec::new();
    for n in 0..FLOOD_GATEWAYS {
        gateways.push(FakeGateway::new(n as u8 + 1, proxy_addr).await);
    }

    // Every datagram is different, by token and by frame counter, so a lost
    // one cannot hide behind a duplicate.
    let datagram = |i: usize| {
        let gw = &gateways[i % FLOOD_GATEWAYS];
        let frame = data_up(0x1000 + (i % FLOOD_GATEWAYS) as u32, i as u16);
        push_data((i as u16).to_be_bytes(), &gw.eui, &[frame])
    };

    let started = Instant::now();
    let sent_and_received = async {
        let mut seen = HashSet::new();
        for first in (0..FLOOD).step_by(FLOOD_WINDOW) {
            let end = (first + FLOOD_WINDOW).min(FLOOD);
            for i in first..end {
                gateways[i % FLOOD_GATEWAYS].send_push(&datagram(i)).await;
            }
            for _ in first..end {
                let received = lns.recv().await;
                assert_eq!(received.kind(), PUSH_DATA);
                let i = u16::from_be_bytes(received.token()) as usize;
                assert!(i < FLOOD && seen.insert(i), "datagram {i} arrived twice");
                assert_eq!(received.datagram, datagram(i), "datagram {i} was altered");
            }
        }
        seen.len()
    };
    let delivered = timeout(FLOOD_DEADLINE, sent_and_received)
        .await
        .expect("the flood did not reach the LNS in time");
    let elapsed = started.elapsed();
    assert_eq!(delivered, FLOOD);

    cancel.cancel();
    timeout(WAIT, task)
        .await
        .expect("the proxy did not stop")
        .unwrap()
        .unwrap();
    elapsed
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn broker_down_does_not_slow_forwarding() {
    // How long the same flood takes with MQTT not involved at all.
    let (outputs, stored) = outputs_with_storage_drain();
    let baseline = flood_through_proxy(outputs).await;
    eventually("the baseline records to be stored", || {
        stored.load(Ordering::SeqCst) == FLOOD
    })
    .await;

    // A TCP port nothing listens on: bind to find a free one, then let go.
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
    let mqtt_cancel = CancellationToken::new();
    let (mqtt_tx, stats) = crate::mqtt::start(&cfg, mqtt_cancel.clone()).unwrap();

    let (outputs, stored) = outputs_with_storage_drain();
    let outputs = outputs.with_mqtt(mqtt_tx, Arc::clone(&stats));
    // Shares the queues and rate limiters of the copy the proxy owns, so the
    // drop warnings can be inspected afterwards.
    let observer = outputs.clone();

    let started = Instant::now();
    let with_broker_down = flood_through_proxy(outputs).await;

    // Forwarding was not held up by the missing broker...
    assert!(
        with_broker_down < baseline * 3 + Duration::from_secs(1),
        "forwarding took {with_broker_down:?} with the broker down, \
         against {baseline:?} without MQTT"
    );
    // ...and neither was the database.
    eventually("every record to be stored", || {
        stored.load(Ordering::SeqCst) == FLOOD
    })
    .await;

    // Nothing could be published, so every record was dropped and counted.
    eventually("every record to be counted as dropped", || {
        stats.dropped.load(Ordering::SeqCst) >= FLOOD as u64
    })
    .await;
    let run_time = started.elapsed();
    assert_eq!(stats.published.load(Ordering::SeqCst), 0);
    let dropped = stats.dropped.load(Ordering::SeqCst);
    assert!(dropped > 0);
    assert_eq!(dropped, FLOOD as u64);

    // Every drop went through a warning limiter, and each limiter let through
    // no more than one line per interval: a handful of lines for thousands of
    // drops instead of one line per drop.
    let queue_full = &observer.mqtt.as_ref().unwrap().drops;
    let limiters = [&stats.offline_drops, &stats.refused_drops, &**queue_full];
    let logged: u64 = limiters.iter().map(|l| l.logged()).sum();
    let suppressed: u64 = limiters.iter().map(|l| l.suppressed()).sum();
    assert_eq!(logged + suppressed, dropped, "a drop escaped its limiter");
    assert!(logged >= 1, "the outage was never reported");
    let per_limiter = 1 + run_time.as_secs() / DEFAULT_INTERVAL.as_secs();
    assert!(
        logged <= limiters.len() as u64 * per_limiter,
        "{logged} warnings for {dropped} drops in {run_time:?}"
    );
    assert!(
        logged * 100 <= dropped,
        "{logged} warnings for {dropped} drops"
    );

    mqtt_cancel.cancel();
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

#[test]
fn udp_config_defaults_and_keys() {
    let defaults = UdpConfig::default();
    assert_eq!(defaults.mode, UdpMode::Proxy);
    assert!(defaults.listen_acks);
    assert_eq!(defaults.session_timeout, Duration::from_secs(300));
    assert_eq!(defaults.max_gateways, 100);

    // An existing configuration without the new keys gets the defaults.
    let config: Configuration =
        toml::from_str("[gateway.udp.downstream]\nbind = \"0.0.0.0:1701\"\n").unwrap();
    assert_eq!(config.gateway.udp.mode, UdpMode::Proxy);
    assert!(config.gateway.udp.listen_acks);
    assert_eq!(config.gateway.udp.session_timeout, Duration::from_secs(300));
    assert_eq!(config.gateway.udp.max_gateways, 100);

    let config: Configuration = toml::from_str(
        r#"
        [gateway.udp]
          mode = "listen"
          listen_acks = false
          session_timeout = "45s"
          max_gateways = 7
        "#,
    )
    .unwrap();
    assert_eq!(config.gateway.udp.mode, UdpMode::Listen);
    assert!(!config.gateway.udp.listen_acks);
    assert_eq!(config.gateway.udp.session_timeout, Duration::from_secs(45));
    assert_eq!(config.gateway.udp.max_gateways, 7);
    assert_eq!(config.gateway.udp.downstream.bind, "0.0.0.0:1700");

    // The mode is spelled in lowercase and nothing else is accepted.
    assert!(toml::from_str::<Configuration>("[gateway.udp]\nmode = \"Listen\"\n").is_err());
    assert!(toml::from_str::<Configuration>("[gateway.udp]\nmode = \"spy\"\n").is_err());
}

#[tokio::test]
async fn unusable_settings_stop_the_proxy_at_startup() {
    async fn start_error(config: UdpConfig, upstream: Option<SocketAddr>) -> String {
        let gw_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let (storage_tx, _storage_rx) = mpsc::channel(1);
        let (live_tx, _) = broadcast::channel(1);
        run_with_socket(
            gw_sock,
            &config,
            upstream,
            &[],
            Outputs::new(storage_tx, live_tx),
            CancellationToken::new(),
        )
        .await
        .expect_err("the proxy started")
        .to_string()
    }
    let lns = Some("127.0.0.1:1".parse().unwrap());

    let err = start_error(config_with(|c| c.max_gateways = 0), lns).await;
    assert!(err.contains("max_gateways"), "{err}");

    let err = start_error(config_with(|c| c.session_timeout = Duration::ZERO), lns).await;
    assert!(err.contains("session_timeout"), "{err}");

    // Proxy mode cannot run without a server to proxy to.
    let err = start_error(UdpConfig::default(), None).await;
    assert!(err.contains("upstream"), "{err}");
}
