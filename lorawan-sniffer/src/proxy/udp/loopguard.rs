//! Protection against the proxy forwarding to itself.
//!
//! If the configured upstream address leads back to the proxy's own
//! gateway-facing port, every datagram it forwards arrives again as if a
//! gateway had sent it, is forwarded again, and so on, flooding the log, the
//! database and the network.  Two layers prevent that:
//!
//! - [`check_startup`] refuses to start when the upstream address is
//!   recognisably the proxy's own.
//! - [`RecentForwards`] (together with `SessionTable::is_own_upstream`) cuts
//!   a loop at runtime that the startup check could not see, such as one
//!   going through Docker port forwarding or NAT.

use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

/// How many forwarded datagrams [`RecentForwards`] remembers.
const RING_SIZE: usize = 256;

/// How long a forwarded datagram counts as recent.
const ECHO_WINDOW: Duration = Duration::from_secs(2);

/// Check that none of the `upstream` addresses is the proxy's own
/// gateway-facing socket.
///
/// An upstream address is the proxy's own when it has the bind port and
/// either the same IP as the bind address, or the proxy is bound to the
/// wildcard address and the upstream IP is any address of this machine
/// (loopback included).  The error message says what is wrong and what to
/// change.
pub fn check_startup(bind: SocketAddr, upstream: &[SocketAddr]) -> Result<(), String> {
    for u in upstream {
        if u.port() != bind.port() {
            continue;
        }
        let u_ip = u.ip().to_canonical();
        let bind_ip = bind.ip().to_canonical();
        // A socket bound to 0.0.0.0 only ever receives IPv4, so an IPv6
        // upstream cannot lead back to it.
        let reachable_by_bind = bind_ip.is_ipv6() || u_ip.is_ipv4();
        let loops =
            u_ip == bind_ip || (bind_ip.is_unspecified() && reachable_by_bind && is_local_ip(u_ip));
        if loops {
            return Err(format!(
                "the upstream address {u} is this sniffer's own gateway-facing address \
                 (bound to {bind}): everything it forwarded would come straight back to \
                 itself. Point the upstream at the real network server, or give the \
                 sniffer a different downstream port"
            ));
        }
    }
    Ok(())
}

/// Whether `ip` is an address of this machine.
///
/// Loopback always is.  Any other address is local exactly when a socket can
/// be bound to it: the kernel refuses with EADDRNOTAVAIL for an address no
/// interface holds.  Hosts that set `net.ipv4.ip_nonlocal_bind` accept
/// foreign addresses too, which would make every address look local, so the
/// probe is calibrated first against a documentation address that no
/// machine owns.  When that bind succeeds the test is meaningless and only
/// loopback counts; the runtime checks cover the rest.
fn is_local_ip(ip: IpAddr) -> bool {
    let ip = ip.to_canonical();
    if ip.is_loopback() {
        return true;
    }
    if allows_nonlocal_bind(ip.is_ipv4()) {
        return false;
    }
    UdpSocket::bind((ip, 0)).is_ok()
}

/// Whether this host lets sockets bind to addresses it does not own, tested
/// with an address from a documentation range (RFC 5737 / RFC 3849).
fn allows_nonlocal_bind(ipv4: bool) -> bool {
    let probe: IpAddr = if ipv4 {
        Ipv4Addr::new(192, 0, 2, 1).into()
    } else {
        Ipv6Addr::new(0x2001, 0x0db8, 0, 0, 0, 0, 0, 1).into()
    };
    UdpSocket::bind((probe, 0)).is_ok()
}

/// A short memory of what the proxy sent upstream, to recognise it coming
/// back.
///
/// Keeps a hash and a timestamp for the last 256 datagrams.  The hash is
/// keyed randomly per instance, so nobody can craft a datagram that collides
/// with a forwarded one to get it dropped.
pub struct RecentForwards {
    hasher: RandomState,
    ring: Vec<(u64, Instant)>,
    /// Slot the next entry goes into once the ring is full.
    next: usize,
}

impl RecentForwards {
    pub fn new() -> Self {
        Self {
            hasher: RandomState::new(),
            ring: Vec::with_capacity(RING_SIZE),
            next: 0,
        }
    }

    /// Record that `datagram` was just sent upstream.  Call it for every
    /// datagram that goes to the LNS, synthetic ones included.
    pub fn remember(&mut self, datagram: &[u8], now: Instant) {
        let entry = (self.hash(datagram), now);
        if self.ring.len() < RING_SIZE {
            self.ring.push(entry);
        } else {
            self.ring[self.next] = entry;
            self.next = (self.next + 1) % RING_SIZE;
        }
    }

    /// Whether a byte-identical datagram was sent upstream within the last
    /// two seconds.
    ///
    /// The proxy never alters a datagram, so a loop through some
    /// address-rewriting hop returns exact copies.  Packet forwarders put a
    /// fresh random token into every datagram, so a legitimate sender does
    /// not repeat itself byte for byte; at most it retransmits, and dropping
    /// a retransmission is harmless because the original has already been
    /// forwarded.
    pub fn is_echo(&self, datagram: &[u8], now: Instant) -> bool {
        let hash = self.hash(datagram);
        self.ring
            .iter()
            .any(|&(h, at)| h == hash && now.saturating_duration_since(at) <= ECHO_WINDOW)
    }

    fn hash(&self, datagram: &[u8]) -> u64 {
        self.hasher.hash_one(datagram)
    }
}

impl Default for RecentForwards {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn startup_check_rejects_same_ip_and_port() {
        let err = check_startup(addr("192.0.2.5:1700"), &[addr("192.0.2.5:1700")]).unwrap_err();
        assert!(err.contains("192.0.2.5:1700"), "{err}");
        assert!(err.contains("own gateway-facing address"), "{err}");

        // IPv6 and the IPv4-mapped spelling of the same address.
        assert!(check_startup(addr("[2001:db8::5]:1700"), &[addr("[2001:db8::5]:1700")]).is_err());
        assert!(check_startup(addr("192.0.2.5:1700"), &[addr("[::ffff:192.0.2.5]:1700")]).is_err());
    }

    #[test]
    fn startup_check_rejects_wildcard_bind_with_loopback_upstream() {
        assert!(check_startup(addr("0.0.0.0:1700"), &[addr("127.0.0.1:1700")]).is_err());
        assert!(check_startup(addr("[::]:1700"), &[addr("[::1]:1700")]).is_err());
        // A dual-stack IPv6 wildcard also receives IPv4.
        assert!(check_startup(addr("[::]:1700"), &[addr("127.0.0.1:1700")]).is_err());
        // The wildcard itself as the upstream is this machine, too.
        assert!(check_startup(addr("0.0.0.0:1700"), &[addr("0.0.0.0:1700")]).is_err());
    }

    /// An address of this machine that is not loopback, found by asking the
    /// kernel which source address it would use to reach the internet.  No
    /// packet is sent: connecting a UDP socket only selects a route.
    fn own_non_loopback_ip() -> Option<IpAddr> {
        let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
        sock.connect("8.8.8.8:80").ok()?;
        let ip = sock.local_addr().ok()?.ip();
        (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
    }

    #[test]
    fn startup_check_rejects_wildcard_bind_with_a_local_non_loopback_upstream() {
        let Some(ip) = own_non_loopback_ip() else {
            eprintln!("skipped: this machine has no non-loopback address");
            return;
        };
        if allows_nonlocal_bind(ip.is_ipv4()) {
            eprintln!("skipped: this host binds to addresses it does not own");
            return;
        }

        let err = check_startup(addr("0.0.0.0:1700"), &[SocketAddr::new(ip, 1700)]);
        assert!(err.is_err(), "{ip}:1700 is this machine's own address");
    }

    #[test]
    fn startup_check_allows_a_different_port() {
        assert!(check_startup(addr("0.0.0.0:1700"), &[addr("127.0.0.1:1701")]).is_ok());
        assert!(check_startup(addr("127.0.0.1:1700"), &[addr("127.0.0.1:1701")]).is_ok());
        assert!(check_startup(addr("192.0.2.5:1700"), &[addr("192.0.2.5:1")]).is_ok());
    }

    #[test]
    fn startup_check_allows_a_non_local_address_on_the_same_port() {
        // 192.0.2.0/24 and 198.51.100.0/24 are documentation ranges that no
        // machine owns, so these stand for a remote LNS on the default port.
        assert!(check_startup(addr("0.0.0.0:1700"), &[addr("192.0.2.77:1700")]).is_ok());
        assert!(check_startup(addr("[::]:1700"), &[addr("198.51.100.7:1700")]).is_ok());
        assert!(check_startup(addr("[::]:1700"), &[addr("[2001:db8::77]:1700")]).is_ok());
        // Bound to one specific address, a different address is not us.
        assert!(check_startup(addr("192.0.2.5:1700"), &[addr("192.0.2.6:1700")]).is_ok());
    }

    #[test]
    fn startup_check_ignores_an_ipv6_upstream_for_an_ipv4_only_bind() {
        // Nothing listens on ::1 when the sniffer is bound to 0.0.0.0 only.
        assert!(check_startup(addr("0.0.0.0:1700"), &[addr("[::1]:1700")]).is_ok());
    }

    #[test]
    fn startup_check_looks_at_every_resolved_address() {
        let upstream = [addr("192.0.2.77:1700"), addr("127.0.0.1:1700")];
        assert!(check_startup(addr("0.0.0.0:1700"), &upstream).is_err());
        assert!(check_startup(addr("0.0.0.0:1700"), &[]).is_ok());
    }

    #[test]
    fn echo_detected_only_within_window() {
        let mut recent = RecentForwards::new();
        let t0 = Instant::now();
        let sent = b"\x02\x12\x34\x00 forwarded";
        recent.remember(sent, t0);

        assert!(recent.is_echo(sent, t0));
        assert!(recent.is_echo(sent, t0 + Duration::from_millis(1_999)));
        assert!(recent.is_echo(sent, t0 + Duration::from_secs(2)));
        assert!(!recent.is_echo(sent, t0 + Duration::from_millis(2_001)));
        assert!(!recent.is_echo(sent, t0 + Duration::from_secs(60)));

        // A datagram that differs in a single byte is not an echo.
        assert!(!recent.is_echo(b"\x02\x12\x35\x00 forwarded", t0));
        assert!(!recent.is_echo(b"", t0));
    }

    #[test]
    fn the_ring_forgets_its_oldest_entries() {
        let mut recent = RecentForwards::new();
        let t0 = Instant::now();
        let datagram = |n: usize| n.to_be_bytes().to_vec();

        for n in 0..RING_SIZE {
            recent.remember(&datagram(n), t0);
        }
        assert!((0..RING_SIZE).all(|n| recent.is_echo(&datagram(n), t0)));

        // One more evicts the oldest and nothing else.
        recent.remember(&datagram(RING_SIZE), t0);
        assert!(!recent.is_echo(&datagram(0), t0));
        assert!((1..=RING_SIZE).all(|n| recent.is_echo(&datagram(n), t0)));
        assert_eq!(recent.ring.len(), RING_SIZE);
    }

    #[test]
    fn calibration_probe_is_not_a_local_address() {
        // Whatever the host allows, loopback is always local, and the
        // documentation address never counts as local.
        assert!(is_local_ip("127.0.0.1".parse().unwrap()));
        assert!(is_local_ip("::1".parse().unwrap()));
        assert!(!is_local_ip("192.0.2.1".parse().unwrap()));
        assert!(!is_local_ip("2001:db8::1".parse().unwrap()));
    }
}
