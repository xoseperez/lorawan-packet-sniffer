//! Semtech UDP Packet Forwarder MITM proxy.
//!
//! Protocol reference: <https://github.com/Lora-net/packet_forwarder/blob/master/PROTOCOL.TXT>;
//! the wire format is in [`packet`].
//!
//! ## Gateways and sessions
//!
//! Any number of gateways can point at the proxy's one gateway-facing socket.
//! Each is identified by the EUI in the header of its PUSH_DATA, PULL_DATA
//! and TX_ACK datagrams and gets a [`session`] that remembers where to answer
//! it.  In proxy mode every session also has its own socket connected to the
//! LNS, so the LNS sees one source port per gateway and everything coming
//! back on a socket belongs to that gateway.  Nothing is ever rewritten:
//! datagrams are forwarded as the exact bytes received.
//!
//! ## Modes
//!
//! - **Proxy** (`upstream` is `Some`): uplinks, keepalives and TX_ACKs go to
//!   the LNS; ACKs and downlinks come back to the gateway that owns the
//!   socket they arrived on.
//! - **Listen** (`upstream` is `None`): there is no LNS.  Messages are only
//!   recorded, as captured, and the proxy optionally answers PUSH_DATA and
//!   PULL_DATA with the ACK the gateway expects so it considers its server
//!   alive.  It never sends a downlink.
//!
//! ## Blocking behaviour
//!
//! ```text
//! suppress_ack = false (default):
//!   Uplink blocked   → drop forward, send synthetic PUSH_ACK to GW
//!                       (GW believes the LNS received it, no retry)
//!   Downlink blocked → drop forward, send synthetic TX_ACK to LNS
//!                       (LNS believes it was transmitted, no retry)
//!
//! suppress_ack = true (stealth mode):
//!   Uplink blocked   → drop forward, NO PUSH_ACK
//!                       (GW will retry until timeout)
//!   Downlink blocked → drop forward, NO TX_ACK
//!                       (LNS will retry until timeout)
//! ```

pub mod loopguard;
mod packet;
mod session;
#[cfg(test)]
mod tests;

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use chrono::Utc;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::config::{UdpConfig, UdpMode};
use crate::core::decoder;
use crate::core::eui::Eui64;
use crate::core::filter::FilterRule;
use crate::core::metadata::{CrcStatus, Direction, MessageRecord, Protocol};
use crate::core::ratelimit::RateLimitedLog;
use crate::core::toa;

use super::{classify, frame_fields, FrameFields, Outputs, Verdict};
use loopguard::RecentForwards;
use packet::{
    build_pull_ack, build_push_ack, build_tx_ack, Header, PullRespPayload, PushDataPayload, RxPk,
    TxPk, EUI_LEN, HDR_LEN, PULL_ACK, PULL_DATA, PULL_RESP, PUSH_ACK, PUSH_DATA, TX_ACK,
};
use session::{LnsDatagram, SessionError, SessionTable};

/// Datagrams from the LNS sockets waiting for the main loop.  The readers
/// wait when it is full, which leaves excess datagrams in the kernel socket
/// buffers.
const LNS_QUEUE: usize = 1024;

/// Longest time between idle sweeps.
const MAX_SWEEP_INTERVAL: Duration = Duration::from_secs(30);

/// Shortest time between idle sweeps, so a tiny timeout cannot make the
/// sweep spin.
const MIN_SWEEP_INTERVAL: Duration = Duration::from_millis(10);

/// Pause after a failed receive on the gateway socket, so a socket that
/// keeps failing cannot turn the main loop into a busy loop.
const RECV_ERROR_PAUSE: Duration = Duration::from_millis(10);

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Run the UDP proxy until `cancel` is triggered.
///
/// Binds the gateway-facing socket from `config.downstream.bind`, then serves
/// as [`run_with_socket`] does.
pub async fn run(
    config: &UdpConfig,
    upstream: Option<SocketAddr>,
    filters: &[FilterRule],
    outputs: Outputs,
    cancel: CancellationToken,
) -> Result<()> {
    // Gateway-facing socket: the gateways send their packets here.
    let gw_sock = UdpSocket::bind(&config.downstream.bind)
        .await
        .with_context(|| format!("cannot bind UDP socket '{}'", config.downstream.bind))?;
    info!("UDP downstream bound to {}", config.downstream.bind);

    run_with_socket(gw_sock, config, upstream, filters, outputs, cancel).await
}

/// Run the UDP proxy on an already bound gateway-facing socket.
///
/// `upstream` is the LNS address in proxy mode.  It is ignored in listen mode
/// and required in proxy mode.
///
/// `outputs` receives every captured `MessageRecord` (storage, WebSocket feed,
/// and MQTT when configured).  It only does non-blocking sends, so a slow
/// consumer never stalls packet forwarding.
///
/// Separate from [`run`] so that tests can bind `127.0.0.1:0` first and learn
/// the port.
pub async fn run_with_socket(
    gw_sock: UdpSocket,
    config: &UdpConfig,
    upstream: Option<SocketAddr>,
    filters: &[FilterRule],
    outputs: Outputs,
    cancel: CancellationToken,
) -> Result<()> {
    check_config(config)?;
    let listen = config.mode == UdpMode::Listen;
    let upstream = match (listen, upstream) {
        (true, _) => None,
        (false, Some(addr)) => Some(addr),
        (false, None) => bail!("UDP proxy mode needs an upstream address"),
    };

    match upstream {
        Some(addr) => info!("UDP upstream: one socket per gateway, each connected to {addr}"),
        None => info!(
            "UDP listen mode: nothing is forwarded, PUSH_ACK / PULL_ACK replies are {}",
            if config.listen_acks { "on" } else { "off" }
        ),
    }

    let (mut proxy, mut lns_rx) = UdpProxy::new(gw_sock, config, upstream, filters, outputs);

    let sweep_every = (config.session_timeout / 4)
        .min(MAX_SWEEP_INTERVAL)
        .max(MIN_SWEEP_INTERVAL);
    let mut sweep = tokio::time::interval(sweep_every);
    sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut gw_buf = vec![0u8; 65_536];

    loop {
        tokio::select! {
            // ----------------------------------------------------------------
            // Datagram from a gateway
            // ----------------------------------------------------------------
            result = proxy.gw_sock.recv_from(&mut gw_buf) => match result {
                Ok((len, src)) => proxy.handle_from_gateway(&gw_buf[..len], src).await,
                Err(e) => {
                    // A receive error says nothing about the other gateways;
                    // ending the proxy over it would cut all of them off.
                    proxy.logs.recv_errors.error(format_args!(
                        "Receive error on the gateway socket: {e}"
                    ));
                    tokio::time::sleep(RECV_ERROR_PAUSE).await;
                }
            },

            // ----------------------------------------------------------------
            // Datagram from the LNS, on some gateway's socket
            // ----------------------------------------------------------------
            Some((eui, datagram)) = lns_rx.recv() => {
                proxy.handle_from_lns(eui, &datagram).await;
            }

            // ----------------------------------------------------------------
            // Release gateways that went quiet
            // ----------------------------------------------------------------
            _ = sweep.tick() => {
                if proxy.sessions.sweep_idle(Instant::now()) > 0 {
                    debug!("{} gateway session(s) remain", proxy.sessions.len());
                }
            }

            // ----------------------------------------------------------------
            // Shutdown
            // ----------------------------------------------------------------
            _ = cancel.cancelled() => {
                info!("UDP proxy shutting down");
                break;
            }
        }
    }

    Ok(())
}

/// Reject settings that would make the proxy misbehave rather than fail.
fn check_config(config: &UdpConfig) -> Result<()> {
    if config.max_gateways == 0 {
        bail!("gateway.udp.max_gateways must be at least 1");
    }
    if config.session_timeout.is_zero() {
        bail!("gateway.udp.session_timeout must be longer than zero");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Proxy state and per-direction handlers
// ---------------------------------------------------------------------------

/// Rate limiters for log lines that unauthenticated datagrams, or a failing
/// network, can trigger at packet rate.  Each condition has its own, so a
/// noisy one cannot hide a rare one.
#[derive(Default)]
struct DropLogs {
    /// Datagrams too short or of a type that makes no sense.
    malformed: RateLimitedLog,
    /// Datagrams from gateways beyond the session limit.
    cap_reached: RateLimitedLog,
    /// Datagrams dropped as the proxy's own traffic coming back.
    looped: RateLimitedLog,
    /// Failures to receive on the gateway socket.
    recv_errors: RateLimitedLog,
    /// Failures to send to a gateway or to the LNS.
    send_errors: RateLimitedLog,
    /// Failures to set up a gateway's LNS socket.
    session_errors: RateLimitedLog,
}

/// Everything the main loop owns.
struct UdpProxy<'a> {
    /// Gateway-facing socket.
    gw_sock: UdpSocket,
    /// Listen mode: no LNS, nothing forwarded.
    listen: bool,
    /// The LNS, in proxy mode.
    upstream: Option<SocketAddr>,
    /// Listen mode: answer PUSH_DATA and PULL_DATA with an ACK.
    listen_acks: bool,
    filters: &'a [FilterRule],
    outputs: Outputs,
    sessions: SessionTable,
    /// What was sent upstream lately, to recognise it coming back.
    recent: RecentForwards,
    /// Handed to each session's reader task.
    lns_tx: mpsc::Sender<LnsDatagram>,
    logs: DropLogs,
}

/// What the filters say about a datagram after its messages were recorded.
enum Disposition {
    /// Forward it.
    Pass,
    /// Do not forward it; `suppress_ack` withholds the protocol ACK too.
    Blocked { suppress_ack: bool },
}

impl<'a> UdpProxy<'a> {
    /// A proxy with no sessions yet, and the receiving end of the channel its
    /// sessions' LNS readers will feed.  `upstream` is `None` in listen mode.
    fn new(
        gw_sock: UdpSocket,
        config: &UdpConfig,
        upstream: Option<SocketAddr>,
        filters: &'a [FilterRule],
        outputs: Outputs,
    ) -> (Self, mpsc::Receiver<LnsDatagram>) {
        let (lns_tx, lns_rx) = mpsc::channel(LNS_QUEUE);
        let proxy = Self {
            gw_sock,
            listen: upstream.is_none(),
            upstream,
            listen_acks: config.listen_acks,
            filters,
            outputs,
            sessions: SessionTable::new(config.max_gateways, config.session_timeout),
            recent: RecentForwards::new(),
            lns_tx,
            logs: DropLogs::default(),
        };
        (proxy, lns_rx)
    }

    async fn handle_from_gateway(&mut self, buf: &[u8], src: SocketAddr) {
        let now = Instant::now();

        // A datagram that comes from our own LNS-facing sockets, or that is a
        // byte-for-byte copy of one we just sent upstream, can only be our
        // own traffic coming back: the upstream address leads to this very
        // socket, directly or through a hop that rewrites addresses (Docker
        // port forwarding, NAT hairpinning).  Gateways put a fresh random
        // token in every datagram, so a genuine exact repeat within seconds
        // would be a mere retransmission, and dropping it loses nothing
        // because the original was already forwarded.  Either way nothing is
        // recorded or forwarded, which is what keeps a loop from feeding on
        // itself.
        if self.is_own_traffic(buf, src, now) {
            return;
        }

        let Some(header) = Header::parse(buf) else {
            self.logs.malformed.warn(format_args!(
                "Received undersized UDP packet from {} ({} bytes), dropping",
                src,
                buf.len()
            ));
            return;
        };

        if !matches!(header.kind, PUSH_DATA | PULL_DATA | TX_ACK) {
            self.logs.malformed.warn(format_args!(
                "Unknown message type 0x{:02X} from {} ({} bytes), dropping",
                header.kind,
                src,
                buf.len()
            ));
            return;
        }

        // All three carry the gateway EUI right after the header.
        let Some(eui) = packet::gateway_eui(buf) else {
            self.logs.malformed.warn(format_args!(
                "Message type 0x{:02X} from {} too short for a gateway EUI ({} bytes), dropping",
                header.kind,
                src,
                buf.len()
            ));
            return;
        };

        let session = match self
            .sessions
            .get_or_create(eui, self.upstream, &self.lns_tx)
            .await
        {
            Ok(session) => session,
            Err(SessionError::CapReached { max }) => {
                self.logs.cap_reached.warn(format_args!(
                    "Gateway limit of {max} reached: ignoring new gateway {eui} at {src}. \
                     Raise gateway.udp.max_gateways to serve more gateways"
                ));
                return;
            }
            Err(SessionError::Io(e)) => {
                self.logs
                    .session_errors
                    .error(format_args!("Cannot set up gateway {eui} at {src}: {e}"));
                return;
            }
        };

        // A session follows the EUI, not the address: a forwarder that
        // restarts, or whose NAT mapping changes, simply moves it.
        session.last_seen = now;
        match header.kind {
            PUSH_DATA => session.push_addr = Some(src),
            _ => session.pull_addr = Some(src),
        }
        let upstream = session.upstream.clone();

        match header.kind {
            PUSH_DATA => {
                let payload = &buf[HDR_LEN + EUI_LEN..];
                debug!(
                    "PUSH_DATA  token={:02X}{:02X}  gw={} eui={}  {} B payload",
                    header.token[0],
                    header.token[1],
                    src,
                    eui,
                    payload.len()
                );

                let disposition =
                    process_push_data(payload, eui, self.listen, self.filters, &self.outputs);

                let Some(upstream) = upstream else {
                    // Listen mode.
                    if self.listen_acks {
                        let ack = build_push_ack(header.token);
                        self.send_to_gateway(&ack, src, "PUSH_ACK").await;
                    }
                    return;
                };

                match disposition {
                    Disposition::Pass => self.send_upstream(&upstream, buf, "PUSH_DATA").await,
                    Disposition::Blocked {
                        suppress_ack: false,
                    } => {
                        // Blocked, non-stealth: gateway needs an ACK or it will retry.
                        debug!(
                            "PUSH_DATA blocked (non-stealth) — sending synthetic PUSH_ACK to {}",
                            src
                        );
                        let ack = build_push_ack(header.token);
                        self.send_to_gateway(&ack, src, "synthetic PUSH_ACK").await;
                    }
                    Disposition::Blocked { suppress_ack: true } => {
                        // Stealth: gateway is unaware the packet was intercepted.
                        debug!("PUSH_DATA blocked (stealth) — no ACK sent");
                    }
                }
            }

            PULL_DATA => {
                debug!(
                    "PULL_DATA  token={:02X}{:02X}  gw={} eui={}",
                    header.token[0], header.token[1], src, eui
                );
                match upstream {
                    // Always forward PULL_DATA so the LNS knows the gateway is alive.
                    Some(upstream) => self.send_upstream(&upstream, buf, "PULL_DATA").await,
                    None if self.listen_acks => {
                        let ack = build_pull_ack(header.token);
                        self.send_to_gateway(&ack, src, "PULL_ACK").await;
                    }
                    None => {}
                }
            }

            // TX_ACK: the gateway confirms, or reports an error for, a downlink.
            _ => {
                debug!(
                    "TX_ACK  token={:02X}{:02X}  gw={} eui={}",
                    header.token[0], header.token[1], src, eui
                );
                match upstream {
                    Some(upstream) => self.send_upstream(&upstream, buf, "TX_ACK").await,
                    // Nobody to tell; a listening proxy never sent a downlink.
                    None => debug!("TX_ACK from {eui} ignored in listen mode"),
                }
            }
        }
    }

    async fn handle_from_lns(&mut self, eui: Eui64, buf: &[u8]) {
        let Some(header) = Header::parse(buf) else {
            self.logs.malformed.warn(format_args!(
                "Received undersized UDP packet from the LNS for gateway {} ({} bytes), dropping",
                eui,
                buf.len()
            ));
            return;
        };

        // The gateway may have been released while the datagram was queued.
        let Some(session) = self.sessions.get(&eui) else {
            debug!("Datagram from the LNS for released gateway {eui}, dropping");
            return;
        };
        let (push_addr, pull_addr, upstream) = (
            session.push_addr,
            session.pull_addr,
            session.upstream.clone(),
        );
        let token = header.token;

        match header.kind {
            // The LNS acknowledges an uplink batch: back to where PUSH_DATA came from.
            PUSH_ACK => match push_addr {
                Some(addr) => {
                    debug!(
                        "PUSH_ACK  token={:02X}{:02X}  {} → {}",
                        token[0], token[1], eui, addr
                    );
                    self.send_to_gateway(buf, addr, "PUSH_ACK").await;
                }
                None => debug!("PUSH_ACK for gateway {eui} but no PUSH_DATA seen yet, dropping"),
            },

            // The LNS acknowledges a keepalive: back to where PULL_DATA came from.
            PULL_ACK => match pull_addr {
                Some(addr) => {
                    debug!(
                        "PULL_ACK  token={:02X}{:02X}  {} → {}",
                        token[0], token[1], eui, addr
                    );
                    self.send_to_gateway(buf, addr, "PULL_ACK").await;
                }
                None => debug!("PULL_ACK for gateway {eui} but no PULL_DATA seen yet, dropping"),
            },

            // The LNS sends a downlink to the gateway.  It carries a token of
            // its own choosing, so the socket it arrived on is what says
            // which gateway it is for.
            PULL_RESP => {
                let Some(addr) = pull_addr else {
                    // Without PULL_DATA the gateway has told us nowhere to
                    // reach it, so nothing was or could be forwarded and
                    // there is nothing honest to record.
                    debug!("PULL_RESP for gateway {eui} but no PULL_DATA seen yet, dropping");
                    return;
                };
                let payload = &buf[HDR_LEN..];
                if payload.is_empty() {
                    self.logs
                        .malformed
                        .warn(format_args!("PULL_RESP too short ({} bytes)", buf.len()));
                    return;
                }
                debug!(
                    "PULL_RESP  token={:02X}{:02X}  {} B payload  {} → {}",
                    token[0],
                    token[1],
                    payload.len(),
                    eui,
                    addr
                );

                match process_pull_resp(payload, eui, self.filters, &self.outputs) {
                    Disposition::Pass => self.send_to_gateway(buf, addr, "PULL_RESP").await,
                    Disposition::Blocked {
                        suppress_ack: false,
                    } => {
                        // Blocked, non-stealth: the LNS needs a TX_ACK or it may retry.
                        debug!("PULL_RESP blocked (non-stealth) — sending synthetic TX_ACK to LNS");
                        if let Some(upstream) = upstream {
                            let ack = build_tx_ack(token, &eui);
                            self.send_upstream(&upstream, &ack, "synthetic TX_ACK")
                                .await;
                        }
                    }
                    Disposition::Blocked { suppress_ack: true } => {
                        debug!("PULL_RESP blocked (stealth) — no TX_ACK sent");
                    }
                }
            }

            other => {
                self.logs.malformed.warn(format_args!(
                    "Unknown message type 0x{:02X} from the LNS for gateway {} ({} bytes), dropping",
                    other,
                    eui,
                    buf.len()
                ));
            }
        }
    }

    /// Whether `buf` is the proxy's own upstream traffic coming back; logs
    /// the reason when it is.  Always `false` in listen mode, which sends
    /// nothing upstream.
    fn is_own_traffic(&self, buf: &[u8], src: SocketAddr, now: Instant) -> bool {
        let Some(upstream) = self.upstream else {
            return false;
        };

        if self.sessions.is_own_upstream(src, upstream.ip()) {
            self.logs.looped.error(format_args!(
                "Dropped a datagram from {src}: it was sent by this sniffer's own LNS socket, \
                 so the upstream address {upstream} leads back to the sniffer. \
                 Set the upstream to the real network server"
            ));
            return true;
        }

        if self.recent.is_echo(buf, now) {
            self.logs.looped.error(format_args!(
                "Dropped a datagram from {src}: it is identical to one just forwarded to \
                 {upstream}, so something between the sniffer and the LNS is sending it back. \
                 Check the upstream address and any port forwarding in between"
            ));
            return true;
        }

        false
    }

    /// Send a datagram to a gateway, logging a failure without giving up.
    async fn send_to_gateway(&self, datagram: &[u8], to: SocketAddr, what: &str) {
        if let Err(e) = self.gw_sock.send_to(datagram, to).await {
            self.logs
                .send_errors
                .error(format_args!("{what} send error to {to}: {e}"));
        }
    }

    /// Send a datagram to the LNS on a gateway's own socket, and remember it
    /// for the loop check.
    async fn send_upstream(&mut self, socket: &UdpSocket, datagram: &[u8], what: &str) {
        let mut result = socket.send(datagram).await;

        // After the LNS answered an earlier datagram with "port unreachable",
        // the next call on the connected socket fails with ECONNREFUSED
        // instead of sending.  That error belongs to the earlier datagram and
        // has now been consumed, so try this one again.
        if matches!(&result, Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused) {
            result = socket.send(datagram).await;
        }

        match result {
            Ok(_) => self.recent.remember(datagram, Instant::now()),
            Err(e) => self
                .logs
                .send_errors
                .error(format_args!("{what} forward error: {e}")),
        }
    }
}

// ---------------------------------------------------------------------------
// Payload processors — decode, evaluate filters, record, return the decision
// ---------------------------------------------------------------------------

/// Decode and record all `rxpk` entries in a PUSH_DATA JSON payload.
///
/// If any entry is blocked the whole PUSH_DATA is blocked.
/// ACK is suppressed only when **all** blocked entries request stealth.
fn process_push_data(
    payload: &[u8],
    gateway_eui: Eui64,
    listen: bool,
    filters: &[FilterRule],
    outputs: &Outputs,
) -> Disposition {
    let json: PushDataPayload = match serde_json::from_slice(payload) {
        Ok(v) => v,
        Err(e) => {
            debug!("PUSH_DATA JSON parse error: {}", e);
            return Disposition::Pass;
        }
    };

    if let Some(stat) = &json.stat {
        debug!(
            "PUSH_DATA stat  rxnb={} rxok={} rxfw={} txnb={}",
            stat.rxnb, stat.rxok, stat.rxfw, stat.txnb
        );
    }

    let mut any_blocked = false;
    let mut all_suppress = true; // pessimistic: only true if every blocked entry wants stealth

    for rxpk in &json.rxpk {
        let (record, verdict) = build_uplink_record(rxpk, gateway_eui, listen, filters);

        if let Verdict::Block { suppress_ack, .. } = verdict {
            any_blocked = true;
            if !suppress_ack {
                all_suppress = false;
            }
        }

        outputs.emit(record);
    }

    if any_blocked {
        Disposition::Blocked {
            suppress_ack: all_suppress,
        }
    } else {
        Disposition::Pass
    }
}

/// Decode and record the `txpk` in a PULL_RESP JSON payload.
fn process_pull_resp(
    payload: &[u8],
    gateway_eui: Eui64,
    filters: &[FilterRule],
    outputs: &Outputs,
) -> Disposition {
    let json: PullRespPayload = match serde_json::from_slice(payload) {
        Ok(v) => v,
        Err(e) => {
            debug!("PULL_RESP JSON parse error: {}", e);
            return Disposition::Pass;
        }
    };

    let (record, verdict) = build_downlink_record(&json.txpk, gateway_eui, filters);

    let disposition = match verdict {
        Verdict::Block { suppress_ack, .. } => Disposition::Blocked { suppress_ack },
        Verdict::Forward | Verdict::Capture => Disposition::Pass,
    };

    outputs.emit(record);
    disposition
}

// ---------------------------------------------------------------------------
// MessageRecord builders
// ---------------------------------------------------------------------------

/// The CRC status a Semtech `stat` value stands for: 1 is a passed check, -1 a
/// failed one and 0 no check at all.  Anything else is unknown.
fn crc_status(stat: i8) -> Option<CrcStatus> {
    match stat {
        1 => Some(CrcStatus::Ok),
        -1 => Some(CrcStatus::Fail),
        0 => Some(CrcStatus::NoCrc),
        _ => None,
    }
}

fn build_uplink_record(
    rxpk: &RxPk,
    gateway_eui: Eui64,
    listen: bool,
    filters: &[FilterRule],
) -> (MessageRecord, Verdict) {
    let crc = crc_status(rxpk.stat);
    let frame = decoder::decode_phy(&rxpk.data, crc.as_ref());
    let fields = frame_fields(frame, rxpk.size, crc.as_ref());
    let verdict = classify(
        listen,
        filters,
        &Direction::Up,
        fields.rule_mtype(),
        Some(&gateway_eui),
    );
    let FrameFields {
        mtype,
        dev_addr,
        dev_eui,
        join_eui,
        dev_nonce,
        fcnt,
        fport,
        fctrl,
        payload_size,
        malformed,
    } = fields;

    (
        MessageRecord {
            id: MessageRecord::new_id(),
            timestamp: Utc::now(),
            direction: Direction::Up,
            status: verdict.status(),
            blocked_by: verdict.blocked_by().map(str::to_owned),
            source_protocol: Protocol::Udp,
            freq_mhz: rxpk.freq,
            datarate: rxpk.datr.clone(),
            rssi: rxpk.rssi,
            snr: rxpk.lsnr,
            crc_status: crc,
            mtype,
            dev_addr,
            dev_eui,
            fcnt,
            fport,
            payload_size,
            toa_ms: toa::compute_lora_toa(payload_size, &rxpk.datr, &rxpk.codr),
            gateway_eui: Some(gateway_eui.to_string()),
            join_eui,
            dev_nonce,
            adr: fctrl.map(|c| c.adr),
            adr_ack_req: fctrl.and_then(|c| c.adr_ack_req),
            ack: fctrl.map(|c| c.ack),
            class_b: fctrl.and_then(|c| c.class_b),
            f_pending: fctrl.and_then(|c| c.f_pending),
            f_opts_len: fctrl.map(|c| c.f_opts_len),
            malformed,
        },
        verdict,
    )
}

fn build_downlink_record(
    txpk: &TxPk,
    gateway_eui: Eui64,
    filters: &[FilterRule],
) -> (MessageRecord, Verdict) {
    // A downlink has no CRC to report.
    let frame = decoder::decode_phy(&txpk.data, None);
    let fields = frame_fields(frame, txpk.size, None);
    let verdict = classify(
        false,
        filters,
        &Direction::Down,
        fields.rule_mtype(),
        Some(&gateway_eui),
    );
    let FrameFields {
        mtype,
        dev_addr,
        dev_eui,
        join_eui,
        dev_nonce,
        fcnt,
        fport,
        fctrl,
        payload_size,
        malformed,
    } = fields;

    (
        MessageRecord {
            id: MessageRecord::new_id(),
            timestamp: Utc::now(),
            direction: Direction::Down,
            status: verdict.status(),
            blocked_by: verdict.blocked_by().map(str::to_owned),
            source_protocol: Protocol::Udp,
            freq_mhz: txpk.freq,
            datarate: txpk.datr.clone(),
            rssi: None,
            snr: None,
            crc_status: None,
            mtype,
            dev_addr,
            dev_eui,
            fcnt,
            fport,
            payload_size,
            toa_ms: toa::compute_lora_toa(payload_size, &txpk.datr, &txpk.codr),
            gateway_eui: Some(gateway_eui.to_string()),
            join_eui,
            dev_nonce,
            adr: fctrl.map(|c| c.adr),
            adr_ack_req: fctrl.and_then(|c| c.adr_ack_req),
            ack: fctrl.map(|c| c.ack),
            class_b: fctrl.and_then(|c| c.class_b),
            f_pending: fctrl.and_then(|c| c.f_pending),
            f_opts_len: fctrl.map(|c| c.f_opts_len),
            malformed,
        },
        verdict,
    )
}
