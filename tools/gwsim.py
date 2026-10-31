#!/usr/bin/env python3
"""gwsim - Semtech UDP packet forwarder simulator and fake LNS.

Standard library only (Python 3.7+). Single thread, non-blocking sockets and a
`selectors` loop, so a few hundred simulated gateways fit in one process.

Two roles, usable separately or together:

  gateways  N simulated gateways. Each one has its own EUI and, like the real
            forwarder, two UDP sockets: a "push" socket that sends PUSH_DATA
            (and receives PUSH_ACK) and a "pull" socket that sends PULL_DATA
            and TX_ACK (and receives PULL_ACK and PULL_RESP). Everything is
            sent to --target. Every gateway sends data uplinks from one fixed
            DevAddr; --random-devaddr gives each uplink a new one, and
            --joins N adds a JoinRequest (fixed DevEUI per gateway) every N
            uplinks.
  lns       A fake network server. It answers PUSH_DATA with PUSH_ACK and
            PULL_DATA with PULL_ACK, sends PULL_RESP downlinks for a fraction
            of the uplinks to the source address of the gateway's latest
            PULL_DATA, and counts TX_ACKs and the source ports used per EUI.
            Its summary also counts the uplink frames received by message
            type and the distinct DevAddrs and DevEUIs in them.
  all       Both in the same process and event loop.

Typical setup: gateways -> sniffer -> fake LNS, so the sniffer's upstream
points at the fake LNS and the gateways point at the sniffer.

Examples
--------

Self-test on loopback, no sniffer (gateways talk straight to the fake LNS):

    tools/gwsim.py all --lns-bind 127.0.0.1:17700 --target 127.0.0.1:17700 \\
        --count 20 --duration 25 --keepalive 5 --uplink-interval 2 \\
        --downlink-ratio 0.3

One gateway through a sniffer on this machine (sniffer listening on 1700, its
upstream pointing at 127.0.0.1:1701):

    tools/gwsim.py all --lns-bind 127.0.0.1:1701 --target 127.0.0.1:1700 \\
        --count 1 --duration 60

Traffic for trying device labels in the sniffer's WebUI: three gateways, each
sending a JoinRequest every 5 uplinks (their DevEUIs are printed at start, so
you know what to label), with every data uplink from a new random DevAddr, which
also fills the sniffer's list of identifiers seen live:

    tools/gwsim.py all --lns-bind 127.0.0.1:1701 --target 127.0.0.1:1700 \\
        --count 3 --duration 120 --uplink-interval 1 --joins 5 --random-devaddr

Only the fake LNS (for example behind a sniffer, with real gateways in front):

    tools/gwsim.py lns --bind 0.0.0.0:1701 --downlink-ratio 0.1

Only gateways, against a sniffer whose upstream is a real LNS:

    tools/gwsim.py gateways --target sniffer.lan:1700 --count 5 --duration 300

100-gateway load run. The sniffer runs on another machine (say 192.168.1.20,
listening on UDP 1700, with its upstream server set to this laptop,
192.168.1.50:1701). On the laptop:

    tools/gwsim.py all --lns-bind 0.0.0.0:1701 --target 192.168.1.20:1700 \\
        --count 100 --duration 1200 --keepalive 10 --uplink-interval 3 \\
        --report-interval 30 | tee gwsim-load.log

That opens 200 UDP sockets on the laptop (the soft descriptor limit is raised
automatically if it is too low) plus the LNS socket. Watch the sniffer from its
own machine with tools/procstat.sh. The exit status is 1 when any gateway got
no PUSH_ACK or no PULL_ACK at all during the run (--allow-missing-acks turns
that check off), so a run can be used as a pass/fail check; the periodic
summary lines show gateways that have not seen an ACK yet while it runs.

Packet formats follow the Semtech UDP protocol: version 2, a 2-byte random
token, an identifier byte, then for PUSH_DATA, PULL_DATA and TX_ACK the 8-byte
gateway EUI (big-endian) followed by an optional JSON body. Uplinks carry a
structurally valid UnconfirmedDataUp frame (random MIC), downlinks an
UnconfirmedDataDown frame. With --joins, the PUSH_DATA of every Nth uplink
carries a second frame, a structurally valid JoinRequest: MHDR 0x00, a fixed
JoinEUI, the gateway's DevEUI (DE:AD:BE:EF:00:00:00:00 plus the gateway index),
a random DevNonce and a random MIC. IPv4 only.
"""

from __future__ import annotations

import argparse
import base64
import heapq
import json
import random
import selectors
import signal
import socket
import struct
import sys
import time

PROTOCOL_VERSION = 2

PUSH_DATA = 0x00
PUSH_ACK = 0x01
PULL_DATA = 0x02
PULL_RESP = 0x03
PULL_ACK = 0x04
TX_ACK = 0x05

HEADER = struct.Struct("!BHB")  # version, token, identifier
HEADER_WITH_EUI = HEADER.size + 8

MHDR_JOIN_REQUEST = 0x00
MHDR_UNCONFIRMED_UP = 0x40
MHDR_UNCONFIRMED_DOWN = 0x60
MTYPE_UNCONFIRMED_UP = 2
MTYPE_CONFIRMED_UP = 4
# Names as the sniffer shows them, indexed by the MHDR bits 7..5.
MTYPE_NAMES = ("JoinRequest", "JoinAccept", "UnconfirmedDataUp", "UnconfirmedDataDown",
               "ConfirmedDataUp", "ConfirmedDataDown", "RejoinRequest", "Proprietary")

# JoinRequests (--joins): one JoinEUI for everybody, and a DevEUI per gateway
# made of this base plus the gateway's index.
JOIN_EUI = 0xCAFE000000000001
DEVEUI_BASE = 0xDEADBEEF00000000

# EU868 default channel plan: (frequency MHz, IF channel, RF chain).
CHANNELS = [
    (868.1, 0, 0),
    (868.3, 1, 0),
    (868.5, 2, 0),
    (867.1, 3, 1),
    (867.3, 4, 1),
    (867.5, 5, 1),
    (867.7, 6, 1),
    (867.9, 7, 1),
]

TX_ACK_BODY = b'{"txpk_ack":{"error":"NONE"}}'

# How long an unanswered token is remembered before it is forgotten.
TOKEN_TTL = 60.0
# Upper bound on one wait in the event loop, so signals are noticed promptly.
MAX_WAIT = 0.25
# Datagrams handled per readiness event before yielding back to the loop.
MAX_READS = 256
# Time the gateways keep listening after they stop sending, so the last
# ACKs are not lost to the shutdown.
DRAIN_SECONDS = 1.5


def out(message: str) -> None:
    print(message, flush=True)


def fmt_eui(eui: int) -> str:
    return "%016X" % eui


def fmt_eui_colon(eui: int) -> str:
    """Colon-separated upper-case hex, the way the sniffer shows a DevEUI."""
    return ":".join("%02X" % b for b in eui.to_bytes(8, "big"))


def compact_json(obj) -> bytes:
    return json.dumps(obj, separators=(",", ":")).encode()


def random_token() -> int:
    return random.getrandbits(16)


def mic() -> bytes:
    # Not a real MIC: nothing in the path verifies it.
    return struct.pack(">I", random.getrandbits(32))


def build_uplink(devaddr: int, fcnt: int) -> bytes:
    """UnconfirmedDataUp: MHDR, DevAddr (LE), FCtrl (ADR), FCnt (LE), FPort 1,
    four payload bytes, MIC."""
    return (
        bytes([MHDR_UNCONFIRMED_UP])
        + devaddr.to_bytes(4, "little")
        + bytes([0x80])
        + (fcnt & 0xFFFF).to_bytes(2, "little")
        + bytes([1])
        + struct.pack(">I", random.getrandbits(32))
        + mic()
    )


def build_join_request(join_eui: int, dev_eui: int, dev_nonce: int) -> bytes:
    """JoinRequest: MHDR, JoinEUI (LE), DevEUI (LE), DevNonce (LE), MIC."""
    return (
        bytes([MHDR_JOIN_REQUEST])
        + join_eui.to_bytes(8, "little")
        + dev_eui.to_bytes(8, "little")
        + (dev_nonce & 0xFFFF).to_bytes(2, "little")
        + mic()
    )


def build_downlink(devaddr: int, fcnt: int) -> bytes:
    """UnconfirmedDataDown: MHDR, DevAddr (LE), FCtrl, FCnt (LE), FPort 1,
    four payload bytes, MIC."""
    return (
        bytes([MHDR_UNCONFIRMED_DOWN])
        + devaddr.to_bytes(4, "little")
        + bytes([0x00])
        + (fcnt & 0xFFFF).to_bytes(2, "little")
        + bytes([1])
        + struct.pack(">I", random.getrandbits(32))
        + mic()
    )


def parse_header(data: bytes):
    """Returns (version, token, identifier) or None for a short datagram."""
    if len(data) < HEADER.size:
        return None
    return HEADER.unpack_from(data)


def rxpk_phy(rxpk) -> "bytes | None":
    """The LoRaWAN frame carried in one rxpk entry, else None."""
    try:
        return base64.b64decode(rxpk["data"])
    except (KeyError, TypeError, ValueError):
        return None


def uplink_devaddr(rxpk) -> "int | None":
    """DevAddr of a data-up frame carried in one rxpk entry, else None."""
    phy = rxpk_phy(rxpk)
    if phy is None or len(phy) < 12 or (phy[0] >> 5) not in (MTYPE_UNCONFIRMED_UP, MTYPE_CONFIRMED_UP):
        return None
    return int.from_bytes(phy[1:5], "little")


def prune(outstanding: dict, now: float) -> None:
    expired = [tok for tok, sent in outstanding.items() if now - sent > TOKEN_TTL]
    for tok in expired:
        del outstanding[tok]


def fresh_token(outstanding: dict) -> int:
    token = random_token()
    while token in outstanding:
        token = random_token()
    return token


def read_all(sock: socket.socket, handler) -> None:
    for _ in range(MAX_READS):
        try:
            data, addr = sock.recvfrom(65535)
        except (BlockingIOError, InterruptedError):
            return
        except OSError:
            return
        handler(data, addr)


def open_udp(bind_addr=("0.0.0.0", 0), rcvbuf: int = 0) -> socket.socket:
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        if rcvbuf:
            try:
                sock.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, rcvbuf)
            except OSError:
                pass
        sock.bind(bind_addr)
        sock.setblocking(False)
    except OSError:
        sock.close()
        raise
    return sock


class Reactor:
    """Selector loop with a timer heap. Callbacks run on the calling thread."""

    def __init__(self) -> None:
        self.sel = selectors.DefaultSelector()
        self.timers = []
        self.seq = 0
        self.stop_requested = False

    def add_reader(self, sock: socket.socket, callback) -> None:
        self.sel.register(sock, selectors.EVENT_READ, callback)

    def remove_reader(self, sock: socket.socket) -> None:
        try:
            self.sel.unregister(sock)
        except (KeyError, ValueError):
            pass

    def call_later(self, delay: float, fn, *args) -> None:
        self.seq += 1
        heapq.heappush(self.timers, (time.monotonic() + delay, self.seq, fn, args))

    def run(self, duration=None, interruptible: bool = True) -> None:
        end = None if duration is None else time.monotonic() + duration
        while not (interruptible and self.stop_requested):
            now = time.monotonic()
            while self.timers and self.timers[0][0] <= now:
                _, _, fn, args = heapq.heappop(self.timers)
                fn(*args)
            now = time.monotonic()
            if end is not None and now >= end:
                break
            timeout = MAX_WAIT
            if self.timers:
                timeout = min(timeout, max(0.0, self.timers[0][0] - now))
            if end is not None:
                timeout = min(timeout, end - now)
            for key, _ in self.sel.select(timeout):
                key.data()


class SimGateway:
    """One simulated forwarder with separate push and pull sockets."""

    def __init__(self, reactor: Reactor, index: int, eui: int, target, keepalive: float,
                 uplink_interval: float, joins: int = 0, random_devaddr: bool = False) -> None:
        self.reactor = reactor
        self.index = index
        self.eui = eui
        self.eui_bytes = eui.to_bytes(8, "big")
        # Unique per gateway as long as the EUIs differ in their low 24 bits.
        self.devaddr = 0x26000000 | (eui & 0x00FFFFFF)
        # The DevEUI of this gateway's JoinRequests, fixed for the whole run.
        self.dev_eui = DEVEUI_BASE + index
        self.target = target
        self.keepalive = keepalive
        self.uplink_interval = uplink_interval
        self.joins = joins  # a JoinRequest every this many uplinks; 0 = never
        self.random_devaddr = random_devaddr
        self.fcnt = 0
        self.uplinks = 0
        self.t0 = time.monotonic()
        self.stopped = False

        self.push_out = {}
        self.pull_out = {}

        self.push_sent = 0
        self.push_acked = 0
        self.join_sent = 0
        self.pull_sent = 0
        self.pull_acked = 0
        self.pull_resp = 0
        self.tx_ack_sent = 0
        self.stray = 0  # wrong packet type for the socket, or malformed
        self.bad_token = 0  # ACK whose token matches nothing outstanding
        self.bad_downlink = 0  # PULL_RESP without a usable txpk
        self.send_errors = 0
        self.last_error = ""

        self.push_sock = open_udp()
        try:
            self.pull_sock = open_udp()
        except OSError:
            self.push_sock.close()
            raise
        reactor.add_reader(self.push_sock, self.on_push_readable)
        reactor.add_reader(self.pull_sock, self.on_pull_readable)

    def start(self) -> None:
        # Random start offsets spread the gateways instead of bursting.
        self.reactor.call_later(random.uniform(0.0, self.uplink_interval), self.send_push)
        self.reactor.call_later(random.uniform(0.0, self.keepalive), self.send_pull)

    def stop(self) -> None:
        self.stopped = True

    def close(self) -> None:
        for sock in (self.push_sock, self.pull_sock):
            self.reactor.remove_reader(sock)
            sock.close()

    def _send(self, sock: socket.socket, payload: bytes) -> bool:
        try:
            sock.sendto(payload, self.target)
        except OSError as exc:
            self.send_errors += 1
            self.last_error = str(exc)
            return False
        return True

    def _data_uplink(self) -> dict:
        devaddr = random.getrandbits(32) if self.random_devaddr else self.devaddr
        phy = build_uplink(devaddr, self.fcnt)
        self.fcnt = (self.fcnt + 1) & 0xFFFF
        return self._rxpk(phy)

    def _join_request(self) -> dict:
        return self._rxpk(build_join_request(JOIN_EUI, self.dev_eui, random.getrandbits(16)))

    def _rxpk(self, phy: bytes) -> dict:
        freq, chan, rfch = random.choice(CHANNELS)
        return {
            "tmst": int((time.monotonic() - self.t0) * 1e6) & 0xFFFFFFFF,
            "freq": freq,
            "chan": chan,
            "rfch": rfch,
            "stat": 1,
            "modu": "LORA",
            "datr": "SF7BW125",
            "codr": "4/5",
            "rssi": random.randint(-120, -40),
            "lsnr": round(random.uniform(-8.0, 10.0), 1),
            "size": len(phy),
            "data": base64.b64encode(phy).decode(),
        }

    def send_push(self) -> None:
        if self.stopped:
            return
        now = time.monotonic()
        prune(self.push_out, now)
        token = fresh_token(self.push_out)
        rxpk = [self._data_uplink()]
        self.uplinks += 1
        with_join = self.joins > 0 and self.uplinks % self.joins == 0
        if with_join:
            rxpk.append(self._join_request())
        datagram = (
            HEADER.pack(PROTOCOL_VERSION, token, PUSH_DATA)
            + self.eui_bytes
            + compact_json({"rxpk": rxpk})
        )
        if self._send(self.push_sock, datagram):
            self.push_out[token] = now
            self.push_sent += 1
            if with_join:
                self.join_sent += 1
        self.reactor.call_later(self.uplink_interval * random.uniform(0.8, 1.2), self.send_push)

    def send_pull(self) -> None:
        if self.stopped:
            return
        now = time.monotonic()
        prune(self.pull_out, now)
        token = fresh_token(self.pull_out)
        datagram = HEADER.pack(PROTOCOL_VERSION, token, PULL_DATA) + self.eui_bytes
        if self._send(self.pull_sock, datagram):
            self.pull_out[token] = now
            self.pull_sent += 1
        self.reactor.call_later(self.keepalive, self.send_pull)

    def on_push_readable(self) -> None:
        read_all(self.push_sock, self.on_push_datagram)

    def on_pull_readable(self) -> None:
        read_all(self.pull_sock, self.on_pull_datagram)

    def on_push_datagram(self, data: bytes, addr) -> None:
        header = parse_header(data)
        if header is None or header[2] != PUSH_ACK:
            self.stray += 1
            return
        if self.push_out.pop(header[1], None) is None:
            self.bad_token += 1
            return
        self.push_acked += 1

    def on_pull_datagram(self, data: bytes, addr) -> None:
        header = parse_header(data)
        if header is None:
            self.stray += 1
            return
        _, token, ident = header
        if ident == PULL_ACK:
            if self.pull_out.pop(token, None) is None:
                self.bad_token += 1
                return
            self.pull_acked += 1
        elif ident == PULL_RESP:
            self.pull_resp += 1
            try:
                txpk = json.loads(data[HEADER.size:])["txpk"]
                usable = isinstance(txpk, dict) and "data" in txpk
            except (ValueError, KeyError, TypeError):
                usable = False
            if not usable:
                self.bad_downlink += 1
            ack = HEADER.pack(PROTOCOL_VERSION, token, TX_ACK) + self.eui_bytes + TX_ACK_BODY
            if self._send(self.pull_sock, ack):
                self.tx_ack_sent += 1
        else:
            self.stray += 1


class LnsGateway:
    """What the fake LNS knows about one gateway EUI."""

    def __init__(self) -> None:
        self.push_addrs = set()
        self.pull_addrs = set()
        self.pull_addr = None  # source of the latest PULL_DATA
        self.push_data = 0
        self.pull_data = 0
        self.downlinks = 0
        self.no_pull_addr = 0  # downlinks skipped: no PULL_DATA seen yet
        self.tx_acks = 0
        self.tx_acks_matched = 0
        self.tx_ack_errors = 0
        self.tx_ack_wrong_port = 0  # TX_ACK not from a PULL_DATA source port
        self.resp_out = {}
        self.dl_fcnt = 0

    def ports(self) -> set:
        return {a[1] for a in self.push_addrs} | {a[1] for a in self.pull_addrs}


class FakeLns:
    def __init__(self, reactor: Reactor, bind_addr, downlink_ratio: float) -> None:
        self.reactor = reactor
        self.downlink_ratio = downlink_ratio
        self.sock = open_udp(bind_addr, rcvbuf=4 * 1024 * 1024)
        self.bound = self.sock.getsockname()
        self.gateways = {}
        self.malformed = 0
        self.send_errors = 0
        self.mtypes = {}  # uplink frames received, by message type name
        self.devaddrs = set()  # distinct DevAddrs seen in data uplinks
        self.deveuis = set()  # distinct DevEUIs seen in join requests
        reactor.add_reader(self.sock, self.on_readable)

    def close(self) -> None:
        self.reactor.remove_reader(self.sock)
        self.sock.close()

    def on_readable(self) -> None:
        read_all(self.sock, self.handle)

    def _send(self, payload: bytes, addr) -> bool:
        try:
            self.sock.sendto(payload, addr)
        except OSError:
            self.send_errors += 1
            return False
        return True

    def _gateway(self, eui: int) -> LnsGateway:
        gw = self.gateways.get(eui)
        if gw is None:
            gw = self.gateways[eui] = LnsGateway()
        return gw

    def handle(self, data: bytes, addr) -> None:
        header = parse_header(data)
        if header is None or header[0] not in (1, 2):
            self.malformed += 1
            return
        version, token, ident = header
        if ident in (PUSH_DATA, PULL_DATA, TX_ACK):
            if len(data) < HEADER_WITH_EUI:
                self.malformed += 1
                return
            eui = int.from_bytes(data[HEADER.size:HEADER_WITH_EUI], "big")
            body = data[HEADER_WITH_EUI:]
            if ident == PUSH_DATA:
                self.on_push_data(version, token, eui, body, addr)
            elif ident == PULL_DATA:
                self.on_pull_data(version, token, eui, addr)
            else:
                self.on_tx_ack(token, eui, body, addr)
        else:
            self.malformed += 1

    def on_push_data(self, version: int, token: int, eui: int, body: bytes, addr) -> None:
        gw = self._gateway(eui)
        gw.push_addrs.add(addr)
        gw.push_data += 1
        self._send(HEADER.pack(version, token, PUSH_ACK), addr)
        try:
            rxpk_list = json.loads(body)["rxpk"]
        except (ValueError, KeyError, TypeError):
            return
        if not isinstance(rxpk_list, list):
            return
        self.count_frames(rxpk_list)
        if self.downlink_ratio <= 0.0 or random.random() >= self.downlink_ratio:
            return
        for rxpk in rxpk_list:
            devaddr = uplink_devaddr(rxpk) if isinstance(rxpk, dict) else None
            if devaddr is not None:
                tmst = rxpk.get("tmst", 0)
                self.send_downlink(gw, devaddr, tmst if isinstance(tmst, int) else 0)
                return

    def count_frames(self, rxpk_list) -> None:
        """Tally the frames of one PUSH_DATA by message type, DevAddr and DevEUI."""
        for rxpk in rxpk_list:
            phy = rxpk_phy(rxpk) if isinstance(rxpk, dict) else None
            if not phy:
                continue
            name = MTYPE_NAMES[phy[0] >> 5]
            self.mtypes[name] = self.mtypes.get(name, 0) + 1
            if name == "JoinRequest" and len(phy) >= 17:
                self.deveuis.add(int.from_bytes(phy[9:17], "little"))
            devaddr = uplink_devaddr(rxpk)
            if devaddr is not None:
                self.devaddrs.add(devaddr)

    def on_pull_data(self, version: int, token: int, eui: int, addr) -> None:
        gw = self._gateway(eui)
        gw.pull_addrs.add(addr)
        gw.pull_addr = addr
        gw.pull_data += 1
        self._send(HEADER.pack(version, token, PULL_ACK), addr)

    def on_tx_ack(self, token: int, eui: int, body: bytes, addr) -> None:
        gw = self._gateway(eui)
        gw.tx_acks += 1
        if gw.resp_out.pop(token, None) is not None:
            gw.tx_acks_matched += 1
        if addr not in gw.pull_addrs:
            gw.tx_ack_wrong_port += 1
        if body:
            try:
                error = json.loads(body)["txpk_ack"]["error"]
            except (ValueError, KeyError, TypeError):
                error = None
            if error not in (None, "NONE"):
                gw.tx_ack_errors += 1

    def send_downlink(self, gw: LnsGateway, devaddr: int, rx_tmst: int) -> None:
        if gw.pull_addr is None:
            gw.no_pull_addr += 1
            return
        now = time.monotonic()
        prune(gw.resp_out, now)
        token = fresh_token(gw.resp_out)
        phy = build_downlink(devaddr, gw.dl_fcnt)
        gw.dl_fcnt = (gw.dl_fcnt + 1) & 0xFFFF
        txpk = {
            "imme": False,
            "tmst": (rx_tmst + 1000000) & 0xFFFFFFFF,
            "freq": 869.525,
            "rfch": 0,
            "powe": 14,
            "modu": "LORA",
            "datr": "SF9BW125",
            "codr": "4/5",
            "ipol": True,
            "size": len(phy),
            "data": base64.b64encode(phy).decode(),
        }
        datagram = HEADER.pack(PROTOCOL_VERSION, token, PULL_RESP) + compact_json({"txpk": txpk})
        if self._send(datagram, gw.pull_addr):
            gw.resp_out[token] = now
            gw.downlinks += 1


# ---------------------------------------------------------------- reporting


def gateway_summary_line(gws, elapsed: float) -> str:
    push_sent = sum(g.push_sent for g in gws)
    push_acked = sum(g.push_acked for g in gws)
    pull_sent = sum(g.pull_sent for g in gws)
    pull_acked = sum(g.pull_acked for g in gws)
    downlinks = sum(g.pull_resp for g in gws)
    no_push = sum(1 for g in gws if g.push_acked == 0)
    no_pull = sum(1 for g in gws if g.pull_acked == 0)
    stray = sum(g.stray + g.bad_token + g.bad_downlink for g in gws)
    errors = sum(g.send_errors for g in gws)
    joins = sum(g.join_sent for g in gws)
    return (
        "[%7.1fs] gw:  %d gateways | push acked %d/%d | pull acked %d/%d | downlinks %d"
        " | without push ack %d | without pull ack %d | stray %d | send errors %d%s"
        % (elapsed, len(gws), push_acked, push_sent, pull_acked, pull_sent, downlinks,
           no_push, no_pull, stray, errors, " | join requests %d" % joins if joins else "")
    )


def lns_summary_line(lns: FakeLns, elapsed: float) -> str:
    gws = lns.gateways.values()
    frames = ", ".join("%s %d" % kv for kv in sorted(lns.mtypes.items())) or "none"
    return (
        "[%7.1fs] lns: %d gateways | push %d | pull %d | downlinks %d (no pull address %d)"
        " | tx_ack %d (matched %d, errors %d) | malformed %d"
        " | uplink frames: %s | distinct DevAddr %d, DevEUI %d"
        % (elapsed, len(lns.gateways), sum(g.push_data for g in gws),
           sum(g.pull_data for g in gws), sum(g.downlinks for g in gws),
           sum(g.no_pull_addr for g in gws), sum(g.tx_acks for g in gws),
           sum(g.tx_acks_matched for g in gws), sum(g.tx_ack_errors for g in gws),
           lns.malformed, frames, len(lns.devaddrs), len(lns.deveuis))
    )


def gateway_table(gws) -> None:
    out("")
    out("Gateways (%d)" % len(gws))
    columns = ("EUI", "push_sent", "push_acked", "pull_sent", "pull_acked", "downlinks",
               "tx_acks", "stray", "bad_token")
    out("%-16s %9s %10s %9s %10s %9s %7s %5s %9s" % columns)
    for g in gws:
        out("%-16s %9d %10d %9d %10d %9d %7d %5d %9d" % (
            fmt_eui(g.eui), g.push_sent, g.push_acked, g.pull_sent, g.pull_acked,
            g.pull_resp, g.tx_ack_sent, g.stray, g.bad_token))
    out("%-16s %9d %10d %9d %10d %9d %7d %5d %9d" % (
        "TOTAL", sum(g.push_sent for g in gws), sum(g.push_acked for g in gws),
        sum(g.pull_sent for g in gws), sum(g.pull_acked for g in gws),
        sum(g.pull_resp for g in gws), sum(g.tx_ack_sent for g in gws),
        sum(g.stray for g in gws), sum(g.bad_token for g in gws)))
    errors = sum(g.send_errors for g in gws)
    if errors:
        last = next(g.last_error for g in gws if g.last_error)
        out("send errors: %d (last: %s)" % (errors, last))
    bad = sum(g.bad_downlink for g in gws)
    if bad:
        out("PULL_RESP without a usable txpk: %d" % bad)


def lns_table(lns: FakeLns) -> None:
    out("")
    out("LNS: %d gateway EUIs, %d malformed datagrams, %d send errors" % (
        len(lns.gateways), lns.malformed, lns.send_errors))
    out("%-16s %-24s %6s %6s %9s %7s %8s  %s" % (
        "EUI", "source ports", "push", "pull", "downlinks", "tx_ack", "matched", "notes"))
    two_ports = 0
    for eui in sorted(lns.gateways):
        gw = lns.gateways[eui]
        push_ports = ",".join(str(p) for p in sorted({a[1] for a in gw.push_addrs}))
        pull_ports = ",".join(str(p) for p in sorted({a[1] for a in gw.pull_addrs}))
        notes = []
        if len(gw.ports()) == 2:
            two_ports += 1
        else:
            notes.append("%d source ports" % len(gw.ports()))
        if gw.tx_ack_wrong_port:
            notes.append("%d tx_ack from non-pull port" % gw.tx_ack_wrong_port)
        if gw.tx_ack_errors:
            notes.append("%d tx_ack errors" % gw.tx_ack_errors)
        if gw.no_pull_addr:
            notes.append("%d downlinks skipped" % gw.no_pull_addr)
        out(("%-16s %-24s %6d %6d %9d %7d %8d  %s" % (
            fmt_eui(eui), "push=%s pull=%s" % (push_ports or "-", pull_ports or "-"),
            gw.push_data, gw.pull_data, gw.downlinks, gw.tx_acks, gw.tx_acks_matched,
            "; ".join(notes))).rstrip())
    out("EUIs seen from exactly 2 source ports: %d/%d" % (two_ports, len(lns.gateways)))


def short_list(euis) -> str:
    shown = ", ".join(fmt_eui(e) for e in euis[:10])
    return shown + (", ... (%d more)" % (len(euis) - 10) if len(euis) > 10 else "")


def gateway_verdict(gws, allow_missing: bool) -> int:
    no_push = [g.eui for g in gws if g.push_acked == 0]
    no_pull = [g.eui for g in gws if g.pull_acked == 0]
    out("")
    if not no_push and not no_pull:
        out("RESULT: every gateway received PUSH_ACKs and PULL_ACKs")
        return 0
    if no_push:
        out("gateways with no PUSH_ACK (%d): %s" % (len(no_push), short_list(no_push)))
    if no_pull:
        out("gateways with no PULL_ACK (%d): %s" % (len(no_pull), short_list(no_pull)))
    if allow_missing:
        out("RESULT: missing ACKs ignored (--allow-missing-acks)")
        return 0
    out("RESULT: FAIL, some gateways received no ACKs")
    return 1


# --------------------------------------------------------------------- CLI


def hostport(value: str):
    host, sep, port = value.rpartition(":")
    if not sep or not port.isdigit() or not 0 < int(port) < 65536:
        raise argparse.ArgumentTypeError("expected HOST:PORT, got %r" % value)
    try:
        info = socket.getaddrinfo(host or "0.0.0.0", int(port), socket.AF_INET, socket.SOCK_DGRAM)
    except socket.gaierror as exc:
        raise argparse.ArgumentTypeError("cannot resolve %r: %s" % (host, exc))
    return info[0][4]


def positive_int(value: str) -> int:
    try:
        number = int(value)
    except ValueError:
        raise argparse.ArgumentTypeError("expected an integer, got %r" % value)
    if number < 1:
        raise argparse.ArgumentTypeError("must be at least 1")
    return number


def positive_float(value: str) -> float:
    try:
        number = float(value)
    except ValueError:
        raise argparse.ArgumentTypeError("expected a number, got %r" % value)
    if not number > 0:
        raise argparse.ArgumentTypeError("must be greater than 0")
    return number


def ratio(value: str) -> float:
    try:
        number = float(value)
    except ValueError:
        raise argparse.ArgumentTypeError("expected a number, got %r" % value)
    if not 0.0 <= number <= 1.0:
        raise argparse.ArgumentTypeError("must be between 0 and 1")
    return number


def eui_base(value: str) -> int:
    try:
        number = int(value, 16)
    except ValueError:
        raise argparse.ArgumentTypeError("expected up to 16 hex digits, got %r" % value)
    if len(value) > 16 or number < 0:
        raise argparse.ArgumentTypeError("expected up to 16 hex digits, got %r" % value)
    return number


def add_common_args(p: argparse.ArgumentParser) -> None:
    p.add_argument("--duration", type=positive_float, default=None, metavar="S",
                   help="stop after S seconds (default: run until Ctrl-C or SIGTERM)")
    p.add_argument("--report-interval", type=positive_float, default=10.0, metavar="S",
                   help="seconds between summary lines (default 10)")


def add_gateway_args(p: argparse.ArgumentParser) -> None:
    p.add_argument("--target", type=hostport, required=True, metavar="HOST:PORT",
                   help="where the gateways send their datagrams (sniffer or LNS)")
    p.add_argument("--count", type=positive_int, default=1, metavar="N",
                   help="number of simulated gateways (default 1)")
    p.add_argument("--keepalive", type=positive_float, default=10.0, metavar="S",
                   help="PULL_DATA period per gateway (default 10)")
    p.add_argument("--uplink-interval", type=positive_float, default=3.0, metavar="S",
                   help="mean PUSH_DATA period per gateway, jittered +/-20%% (default 3)")
    p.add_argument("--eui-base", type=eui_base, default=0xAA555A0000000000, metavar="HEX",
                   help="EUI of the first gateway, 16 hex digits (default AA555A0000000000)")
    p.add_argument("--joins", type=positive_int, default=0, metavar="N",
                   help="every gateway also sends a JoinRequest, with its own fixed DevEUI, "
                        "every N uplinks (default: none); the DevEUIs are printed at start")
    p.add_argument("--random-devaddr", action="store_true",
                   help="every data uplink uses a new random DevAddr instead of the "
                        "gateway's fixed one (fills the sniffer's list of live identifiers)")
    p.add_argument("--allow-missing-acks", action="store_true",
                   help="exit 0 even if a gateway got no PUSH_ACK or PULL_ACK")


def add_lns_args(p: argparse.ArgumentParser) -> None:
    p.add_argument("--downlink-ratio", type=ratio, default=0.1, metavar="R",
                   help="fraction of uplinks answered with a PULL_RESP, 0 to 1 (default 0.1)")


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="gwsim.py",
        description="Semtech UDP packet forwarder simulator and fake LNS.")
    sub = parser.add_subparsers(dest="role", required=True, metavar="{gateways,lns,all}")

    gw = sub.add_parser("gateways", help="run simulated gateways only")
    add_gateway_args(gw)
    add_common_args(gw)

    lns = sub.add_parser("lns", help="run the fake LNS only")
    lns.add_argument("--bind", dest="lns_bind", type=hostport, required=True,
                     metavar="HOST:PORT", help="address the fake LNS listens on")
    add_lns_args(lns)
    add_common_args(lns)

    both = sub.add_parser("all", help="run the fake LNS and the gateways in one process")
    both.add_argument("--lns-bind", dest="lns_bind", type=hostport, required=True,
                      metavar="HOST:PORT", help="address the fake LNS listens on")
    add_lns_args(both)
    add_gateway_args(both)
    add_common_args(both)
    return parser


def ensure_fd_limit(needed: int) -> None:
    """Raise the soft descriptor limit to cover `needed`, or exit if impossible."""
    try:
        import resource
    except ImportError:
        return
    soft, hard = resource.getrlimit(resource.RLIMIT_NOFILE)
    if soft == resource.RLIM_INFINITY or soft >= needed:
        return
    if hard != resource.RLIM_INFINITY and hard < needed:
        sys.exit("gwsim: needs about %d file descriptors but the hard limit is %d "
                 "(lower --count or raise the limit)" % (needed, hard))
    try:
        resource.setrlimit(resource.RLIMIT_NOFILE, (needed, hard))
    except (ValueError, OSError) as exc:
        sys.exit("gwsim: needs about %d file descriptors, soft limit is %d and it "
                 "cannot be raised (%s); try `ulimit -n %d`" % (needed, soft, exc, needed))


def main(argv=None) -> int:
    args = build_parser().parse_args(argv)
    run_lns = args.role in ("lns", "all")
    run_gateways = args.role in ("gateways", "all")

    count = args.count if run_gateways else 0
    if run_gateways:
        if args.eui_base + count - 1 > 0xFFFFFFFFFFFFFFFF:
            sys.exit("gwsim: --eui-base plus --count overflows 64 bits")
        ensure_fd_limit(2 * count + 1 + 64)

    reactor = Reactor()

    def request_stop(signum, frame):
        reactor.stop_requested = True

    signal.signal(signal.SIGINT, request_stop)
    signal.signal(signal.SIGTERM, request_stop)

    lns = None
    gws = []
    try:
        if run_lns:
            try:
                lns = FakeLns(reactor, args.lns_bind, args.downlink_ratio)
            except OSError as exc:
                sys.exit("gwsim: cannot bind the fake LNS to %s:%d: %s" % (
                    args.lns_bind[0], args.lns_bind[1], exc))
            out("fake LNS listening on %s:%d (downlink ratio %.2f)" % (
                lns.bound[0], lns.bound[1], args.downlink_ratio))
        if run_gateways:
            try:
                for i in range(count):
                    gws.append(SimGateway(reactor, i, args.eui_base + i, args.target,
                                          args.keepalive, args.uplink_interval,
                                          args.joins, args.random_devaddr))
            except OSError as exc:
                sys.exit("gwsim: cannot open sockets for gateway %d: %s "
                         "(check `ulimit -n`)" % (len(gws), exc))
            out("%d gateways, EUIs %s..%s, target %s:%d, keepalive %gs, uplink interval %gs" % (
                count, fmt_eui(gws[0].eui), fmt_eui(gws[-1].eui), args.target[0], args.target[1],
                args.keepalive, args.uplink_interval))
            if args.joins:
                out("a JoinRequest every %d uplinks (JoinEUI %s); DevEUI per gateway:" % (
                    args.joins, fmt_eui_colon(JOIN_EUI)))
                for g in gws[:10]:
                    out("  gateway %s  DevEUI %s" % (fmt_eui(g.eui), fmt_eui_colon(g.dev_eui)))
                if len(gws) > 10:
                    out("  ... (%d more, DevEUI %s plus the gateway's index)" % (
                        len(gws) - 10, fmt_eui_colon(DEVEUI_BASE)))
            if args.random_devaddr:
                out("every data uplink uses a new random DevAddr")
            for g in gws:
                g.start()

        started = time.monotonic()
        reporting = [True]

        def report() -> None:
            if not reporting[0]:
                return
            elapsed = time.monotonic() - started
            if gws:
                out(gateway_summary_line(gws, elapsed))
            if lns is not None:
                out(lns_summary_line(lns, elapsed))
            reactor.call_later(args.report_interval, report)

        reactor.call_later(args.report_interval, report)
        reactor.run(args.duration)
        reporting[0] = False

        if gws:
            for g in gws:
                g.stop()
            reactor.run(DRAIN_SECONDS, interruptible=False)

        elapsed = time.monotonic() - started
        out("")
        out("finished after %.1fs%s" % (elapsed, " (interrupted)" if reactor.stop_requested else ""))
        status = 0
        if gws:
            out(gateway_summary_line(gws, elapsed))
        if lns is not None:
            out(lns_summary_line(lns, elapsed))
        if gws:
            gateway_table(gws)
        if lns is not None:
            lns_table(lns)
        if gws:
            status = gateway_verdict(gws, args.allow_missing_acks)
        return status
    finally:
        for g in gws:
            g.close()
        if lns is not None:
            lns.close()


if __name__ == "__main__":
    sys.exit(main())
