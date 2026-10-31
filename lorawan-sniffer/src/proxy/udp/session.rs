//! Per-gateway state for the UDP proxy.
//!
//! Every datagram a gateway sends carries its EUI, so the proxy keeps one
//! [`GatewaySession`] per EUI.  A session remembers where to answer the
//! gateway and, in proxy mode, owns the socket that talks to the LNS on the
//! gateway's behalf.  Because each gateway has its own LNS-facing socket, a
//! datagram that comes back from the LNS belongs to the gateway whose socket
//! it arrived on, with no need to look inside it or to rewrite anything.
//!
//! A packet forwarder sends PUSH_DATA from one socket and PULL_DATA / TX_ACK
//! from another, so a session holds two gateway-side addresses: PUSH_ACK goes
//! back to the first, PULL_ACK and PULL_RESP to the second.
//!
//! The table is owned by the proxy's main loop alone, so it needs no locks.

use std::collections::{HashMap, HashSet};
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use thiserror::Error;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::core::eui::Eui64;
use crate::core::ratelimit::RateLimitedLog;

/// A datagram received from the LNS, with the gateway whose socket it
/// arrived on.
pub type LnsDatagram = (Eui64, Vec<u8>);

/// Largest possible UDP payload; a smaller buffer would silently truncate a
/// datagram that the proxy must forward byte for byte.
const MAX_DATAGRAM: usize = 65_536;

/// Pause after a failed receive, so a socket that keeps failing cannot turn
/// its reader task into a busy loop.
const RECV_ERROR_PAUSE: Duration = Duration::from_millis(50);

/// Why a session could not be provided.
#[derive(Debug, Error)]
pub enum SessionError {
    /// The table already holds `max` gateways and this EUI is not one of
    /// them.
    #[error("session limit of {max} gateways reached")]
    CapReached { max: usize },
    /// The LNS-facing socket could not be set up.
    #[error("cannot open the LNS-facing socket: {0}")]
    Io(#[from] io::Error),
}

/// What the proxy knows about one gateway.
pub struct GatewaySession {
    pub eui: Eui64,
    /// Source of the gateway's last PUSH_DATA: where PUSH_ACK goes.
    pub push_addr: Option<SocketAddr>,
    /// Source of the gateway's last PULL_DATA or TX_ACK: where PULL_ACK and
    /// PULL_RESP go.
    pub pull_addr: Option<SocketAddr>,
    /// Proxy mode: a socket bound to an ephemeral port and connected to the
    /// LNS.  `None` in listen mode.
    pub upstream: Option<Arc<UdpSocket>>,
    /// When the gateway last sent anything.
    pub last_seen: Instant,
    /// Task that reads `upstream` and feeds the main loop.  Aborted when the
    /// session is dropped, so a released session leaves no task or socket
    /// behind.
    reader: Option<JoinHandle<()>>,
    /// Local port of `upstream`, kept to release it from the table's set.
    upstream_port: Option<u16>,
}

impl GatewaySession {
    fn new(eui: Eui64, now: Instant) -> Self {
        Self {
            eui,
            push_addr: None,
            pull_addr: None,
            upstream: None,
            last_seen: now,
            reader: None,
            upstream_port: None,
        }
    }
}

impl Drop for GatewaySession {
    fn drop(&mut self) {
        if let Some(reader) = self.reader.take() {
            reader.abort();
        }
    }
}

/// All known gateways, keyed by EUI.
pub struct SessionTable {
    sessions: HashMap<Eui64, GatewaySession>,
    /// Most sessions the table will hold.
    max: usize,
    /// How long a session may stay silent before [`SessionTable::sweep_idle`]
    /// releases it.
    idle: Duration,
    /// Local ports of the upstream sockets, to recognise our own traffic
    /// coming back.
    own_ports: HashSet<u16>,
    /// Shared by every reader task, so a down LNS produces one log line, not
    /// one per gateway.
    recv_errors: Arc<RateLimitedLog>,
}

impl SessionTable {
    pub fn new(max: usize, idle: Duration) -> Self {
        Self {
            sessions: HashMap::new(),
            max,
            idle,
            own_ports: HashSet::new(),
            recv_errors: Arc::new(RateLimitedLog::new()),
        }
    }

    /// Number of live sessions.
    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    /// The session for `eui`, created if it does not exist yet.
    ///
    /// With `upstream` set (proxy mode) a new session also gets its own
    /// socket connected to that address, and a reader task that sends every
    /// datagram arriving on it, tagged with the gateway's EUI, to `lns_tx`.
    ///
    /// Refuses with [`SessionError::CapReached`] when the table is full and
    /// `eui` is new; sessions that exist are never evicted to make room.
    pub async fn get_or_create(
        &mut self,
        eui: Eui64,
        upstream: Option<SocketAddr>,
        lns_tx: &mpsc::Sender<LnsDatagram>,
    ) -> Result<&mut GatewaySession, SessionError> {
        if !self.sessions.contains_key(&eui) {
            if self.sessions.len() >= self.max {
                return Err(SessionError::CapReached { max: self.max });
            }

            let mut session = GatewaySession::new(eui, Instant::now());
            if let Some(lns) = upstream {
                let (socket, port) = open_upstream(lns).await?;
                session.reader = Some(tokio::spawn(read_upstream(
                    eui,
                    Arc::clone(&socket),
                    lns_tx.clone(),
                    Arc::clone(&self.recv_errors),
                )));
                session.upstream = Some(socket);
                session.upstream_port = Some(port);
                self.own_ports.insert(port);
            }
            info!("New gateway {} ({} active)", eui, self.sessions.len() + 1);
            self.sessions.insert(eui, session);
        }

        // The entry exists: it was either found above or just inserted.
        Ok(self
            .sessions
            .entry(eui)
            .or_insert_with(|| GatewaySession::new(eui, Instant::now())))
    }

    pub fn get(&self, eui: &Eui64) -> Option<&GatewaySession> {
        self.sessions.get(eui)
    }

    /// Release every session that has been silent for longer than the idle
    /// timeout, together with its socket and reader task, and return how
    /// many went.
    pub fn sweep_idle(&mut self, now: Instant) -> usize {
        let before = self.sessions.len();
        let idle = self.idle;
        let own_ports = &mut self.own_ports;
        self.sessions.retain(|_, session| {
            let keep = now.saturating_duration_since(session.last_seen) < idle;
            if !keep {
                if let Some(port) = session.upstream_port {
                    own_ports.remove(&port);
                }
                info!(
                    "Gateway {} released after {} s without traffic",
                    session.eui,
                    idle.as_secs()
                );
            }
            keep
        });
        before - self.sessions.len()
    }

    /// Whether a datagram from `src` came from one of our own upstream
    /// sockets rather than from a gateway.
    ///
    /// That only happens when the upstream address leads back to the
    /// gateway-facing socket.  The source then has one of our ports, on the
    /// upstream's IP or on loopback (the kernel picks the source address of a
    /// connected socket from the route to its destination).
    pub fn is_own_upstream(&self, src: SocketAddr, upstream_ip: IpAddr) -> bool {
        self.own_ports.contains(&src.port())
            && (src.ip().to_canonical() == upstream_ip.to_canonical() || src.ip().is_loopback())
    }
}

/// Bind a socket to an ephemeral port of the same address family as `lns` and
/// connect it there.  Returns the socket and its local port.
async fn open_upstream(lns: SocketAddr) -> io::Result<(Arc<UdpSocket>, u16)> {
    let local = match lns {
        SocketAddr::V4(_) => SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)),
        SocketAddr::V6(_) => SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0)),
    };
    let socket = UdpSocket::bind(local).await?;
    socket.connect(lns).await?;
    let port = socket.local_addr()?.port();
    Ok((Arc::new(socket), port))
}

/// Forward everything arriving on a session's upstream socket to the main
/// loop.
///
/// A receive error is logged and the loop carries on.  On a connected UDP
/// socket the kernel reports an ICMP "port unreachable" as ECONNREFUSED on
/// the next call, which just means the LNS was not listening for a moment;
/// ending the task, or the proxy, over it would cut the gateway off for good.
async fn read_upstream(
    eui: Eui64,
    socket: Arc<UdpSocket>,
    tx: mpsc::Sender<LnsDatagram>,
    errors: Arc<RateLimitedLog>,
) {
    let mut buf = vec![0u8; MAX_DATAGRAM];
    loop {
        match socket.recv(&mut buf).await {
            Ok(len) => {
                // Only fails once the main loop has gone away.
                if tx.send((eui, buf[..len].to_vec())).await.is_err() {
                    break;
                }
            }
            Err(e) => {
                errors.warn(format_args!(
                    "Receive error on the LNS socket of gateway {}: {}",
                    eui, e
                ));
                tokio::time::sleep(RECV_ERROR_PAUSE).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GW_A: Eui64 = Eui64([0xA0, 0, 0, 0, 0, 0, 0, 1]);
    const GW_B: Eui64 = Eui64([0xB0, 0, 0, 0, 0, 0, 0, 2]);
    const GW_C: Eui64 = Eui64([0xC0, 0, 0, 0, 0, 0, 0, 3]);

    fn channel() -> (mpsc::Sender<LnsDatagram>, mpsc::Receiver<LnsDatagram>) {
        mpsc::channel(16)
    }

    /// A socket standing in for the LNS.
    async fn fake_lns() -> (UdpSocket, SocketAddr) {
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = sock.local_addr().unwrap();
        (sock, addr)
    }

    #[tokio::test]
    async fn listen_sessions_have_no_upstream_socket() {
        let (tx, _rx) = channel();
        let mut table = SessionTable::new(4, Duration::from_secs(60));

        let session = table.get_or_create(GW_A, None, &tx).await.unwrap();

        assert_eq!(session.eui, GW_A);
        assert!(session.upstream.is_none());
        assert!(session.push_addr.is_none() && session.pull_addr.is_none());
        assert!(table.own_ports.is_empty());
    }

    #[tokio::test]
    async fn each_proxy_session_gets_its_own_upstream_socket() {
        let (_lns, lns_addr) = fake_lns().await;
        let (tx, _rx) = channel();
        let mut table = SessionTable::new(4, Duration::from_secs(60));

        let a = table
            .get_or_create(GW_A, Some(lns_addr), &tx)
            .await
            .unwrap()
            .upstream
            .clone()
            .unwrap();
        let b = table
            .get_or_create(GW_B, Some(lns_addr), &tx)
            .await
            .unwrap()
            .upstream
            .clone()
            .unwrap();
        // Asking again returns the same session, not a new socket.
        let a_again = table
            .get_or_create(GW_A, Some(lns_addr), &tx)
            .await
            .unwrap()
            .upstream
            .clone()
            .unwrap();

        let (port_a, port_b) = (
            a.local_addr().unwrap().port(),
            b.local_addr().unwrap().port(),
        );
        assert_ne!(port_a, port_b);
        assert!(Arc::ptr_eq(&a, &a_again));
        assert_eq!(table.len(), 2);
        assert_eq!(table.own_ports, HashSet::from([port_a, port_b]));
    }

    #[tokio::test]
    async fn reader_tags_datagrams_with_the_session_eui() {
        let (lns, lns_addr) = fake_lns().await;
        let (tx, mut rx) = channel();
        let mut table = SessionTable::new(4, Duration::from_secs(60));

        let a = table
            .get_or_create(GW_A, Some(lns_addr), &tx)
            .await
            .unwrap()
            .upstream
            .clone()
            .unwrap();
        a.send(b"hello").await.unwrap();
        let mut buf = [0u8; 16];
        let (len, peer) = lns.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[..len], b"hello");

        lns.send_to(b"reply", peer).await.unwrap();
        let (eui, data) = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("reader task did not deliver")
            .unwrap();
        assert_eq!(eui, GW_A);
        assert_eq!(data, b"reply");
    }

    #[tokio::test]
    async fn a_full_table_refuses_new_gateways_but_keeps_the_old_ones() {
        let (tx, _rx) = channel();
        let mut table = SessionTable::new(2, Duration::from_secs(60));
        table.get_or_create(GW_A, None, &tx).await.unwrap();
        table.get_or_create(GW_B, None, &tx).await.unwrap();

        let err = table.get_or_create(GW_C, None, &tx).await.err().unwrap();
        assert!(matches!(err, SessionError::CapReached { max: 2 }));
        assert_eq!(table.len(), 2);
        assert!(table.get(&GW_C).is_none());

        // The gateways already in the table are still served.
        assert!(table.get_or_create(GW_A, None, &tx).await.is_ok());
        assert!(table.get_or_create(GW_B, None, &tx).await.is_ok());
    }

    #[tokio::test]
    async fn sweep_releases_idle_sessions_and_their_ports() {
        let (_lns, lns_addr) = fake_lns().await;
        let (tx, _rx) = channel();
        let mut table = SessionTable::new(4, Duration::from_secs(60));
        let t0 = Instant::now();

        table
            .get_or_create(GW_A, Some(lns_addr), &tx)
            .await
            .unwrap();
        table
            .get_or_create(GW_B, Some(lns_addr), &tx)
            .await
            .unwrap();
        let port_a = table.sessions[&GW_A].upstream_port.unwrap();
        let port_b = table.sessions[&GW_B].upstream_port.unwrap();
        table.sessions.get_mut(&GW_A).unwrap().last_seen = t0;
        table.sessions.get_mut(&GW_B).unwrap().last_seen = t0 + Duration::from_secs(50);

        // Nothing has been silent for a full minute yet.
        assert_eq!(table.sweep_idle(t0 + Duration::from_secs(59)), 0);
        assert_eq!(table.len(), 2);

        // 70 s in, A has been silent for 70 s and B for 20 s.
        assert_eq!(table.sweep_idle(t0 + Duration::from_secs(70)), 1);
        assert!(table.get(&GW_A).is_none());
        assert!(table.get(&GW_B).is_some());
        assert_eq!(table.own_ports, HashSet::from([port_b]));
        assert!(!table.own_ports.contains(&port_a));

        // B goes too once it has been silent long enough.
        assert_eq!(table.sweep_idle(t0 + Duration::from_secs(200)), 1);
        assert_eq!(table.len(), 0);
        assert!(table.own_ports.is_empty());
    }

    #[tokio::test]
    async fn dropping_a_session_stops_its_reader() {
        let (_lns, lns_addr) = fake_lns().await;
        let (tx, _rx) = channel();
        let mut table = SessionTable::new(4, Duration::from_secs(60));
        let session = table
            .get_or_create(GW_A, Some(lns_addr), &tx)
            .await
            .unwrap();
        let socket = session.upstream.clone().unwrap();
        let reader = session.reader.as_ref().unwrap().abort_handle();
        assert!(!reader.is_finished());

        assert_eq!(
            table.sweep_idle(Instant::now() + Duration::from_secs(3600)),
            1
        );

        // The abort lands at the reader's next poll; wait for it, then the
        // reader's clone of the socket is gone and ours is the only one.
        tokio::time::timeout(Duration::from_secs(5), async {
            while !reader.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("reader task was not aborted");
        assert_eq!(Arc::strong_count(&socket), 1);
    }

    #[test]
    fn own_upstream_means_one_of_our_ports_on_the_upstream_ip_or_loopback() {
        let mut table = SessionTable::new(4, Duration::from_secs(60));
        table.own_ports.insert(40_000);
        let lns: IpAddr = "192.0.2.10".parse().unwrap();

        // Our port, reached through the upstream's address or loopback.
        assert!(table.is_own_upstream("192.0.2.10:40000".parse().unwrap(), lns));
        assert!(table.is_own_upstream("127.0.0.1:40000".parse().unwrap(), lns));
        // An IPv4-mapped IPv6 source is the same address.
        assert!(table.is_own_upstream("[::ffff:192.0.2.10]:40000".parse().unwrap(), lns));
        // Another port on the same address is just another sender.
        assert!(!table.is_own_upstream("192.0.2.10:40001".parse().unwrap(), lns));
        // Our port number on some unrelated host is not us.
        assert!(!table.is_own_upstream("198.51.100.7:40000".parse().unwrap(), lns));
    }
}
