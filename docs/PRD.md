# LoRaWAN Packet Sniffer v2 — Product Requirements Document

**Version:** 1.3
**Date:** 2026-02-23
**Author:** Xose Pérez
**Status:** Draft

---

## 1. Overview

LoRaWAN Packet Sniffer v2 is a complete rewrite of the existing Python-based packet sniffer (v1) in Rust. It acts as a transparent Man-In-The-Middle (MITM) proxy between a LoRaWAN gateway and a LoRa Network Server (LNS), intercepting, decoding, logging, and forwarding LoRaWAN traffic without modifying the packets. The tool supports selective message blocking and provides a real-time web interface for monitoring and analysis.

### 1.1 Goals

- **Full rewrite in Rust** following the coding style and patterns of chirpstack-concentratord
- **Dual-protocol support:** Semtech UDP Packet Forwarder and ChirpStack Concentratord (ZeroMQ/IPC)
- **Transparent MITM operation:** receive, log, and forward messages without modification
- **Selective blocking:** allow users to block specific uplink or downlink messages
- **Persistent storage:** store all messages and metadata in SQLite (embedded, single-file database)
- **Real-time WebUI:** React 19 application with WebSocket-based live updates, usable on 800x600 screens

### 1.2 Non-Goals

- Modifying or injecting packets (the sniffer is passive/blocking only, not an active attacker)
- Replacing the gateway's packet forwarder or concentratord
- LoRaWAN network server functionality
- Decryption of application-layer payloads (requires AppSKey/NwkSKey which the sniffer does not possess)
- Multi-gateway support (single gateway per sniffer instance in this version)

### 1.3 Background

The v1 sniffer is a Python application that passively listens on the gateway's UDP port (1700) and decodes incoming Semtech protocol packets. It supports only the upstream direction (uplinks and join requests), stores messages in memory (capped deque), and serves a polling-based web UI. Key limitations addressed by this rewrite:

| Limitation (v1) | Solution (v2) |
|---|---|
| Python, single-threaded | Rust, multi-threaded |
| UDP Packet Forwarder only | UDP + Concentratord IPC |
| Passive listener (one direction) | Full MITM proxy (both directions) |
| In-memory storage (500 messages max) | Persistent SQLite storage |
| HTTP polling (1s interval) | WebSocket real-time push |
| No message blocking | Selective uplink/downlink blocking |
| No downlink visibility | Full downlink capture and display |
| Basic vanilla JS UI | React 19 SPA |

---

## 2. Architecture

### 2.1 High-Level Diagram

```
                        LoRaWAN Packet Sniffer v2
                    ┌─────────────────────────────────┐
                    │                                  │
  ┌─────────┐      │  ┌───────────┐   ┌───────────┐  │      ┌─────────┐
  │         │──UL──▶│  │  Gateway   │──▶│    LNS    │──│─UL──▶│         │
  │ Gateway │      │  │  Frontend  │   │  Backend  │  │      │   LNS   │
  │         │◀─DL──│  │           │◀──│           │  │◀─DL──│         │
  └─────────┘      │  └───────────┘   └───────────┘  │      └─────────┘
                    │        │               │        │
                    │        ▼               ▼        │
                    │     ┌─────────────────────┐     │
                    │     │    Message Engine    │     │
                    │     │  (decode, filter,    │     │
                    │     │   log, block)        │     │
                    │     └────────┬────────────┘     │
                    │              │                   │
                    │        ┌─────┴─────┐            │
                    │        ▼           ▼            │
                    │   ┌────────┐  ┌─────────┐      │
                    │   │ SQLite │  │ WebSocket│      │
                    │   │Storage │  │  Server  │      │
                    │   └────────┘  └────┬────┘      │
                    │                    │            │
                    └────────────────────┼────────────┘
                                         │
                                    ┌────┴────┐
                                    │ React   │
                                    │ WebUI   │
                                    └─────────┘
```

### 2.2 Unified Message Model

Regardless of the underlying protocol (UDP Packet Forwarder or Concentratord IPC), all captured messages are normalized into a single canonical `MessageRecord` structure before filtering, storage, and display. Each proxy backend is responsible for mapping its protocol-specific data into this common model.

```rust
struct MessageRecord {
    // Identity
    id: String,                     // Unique message ID (ULID or UUID)
    timestamp: DateTime<Utc>,       // Capture timestamp

    // Proxy metadata
    direction: Direction,           // Up | Down
    status: Status,                 // Forwarded | Blocked
    blocked_by: Option<String>,     // Filter name that blocked this message (None if forwarded)
    source_protocol: Protocol,      // Udp | Concentratord

    // RF metadata
    freq_mhz: f64,                  // Frequency in MHz
    datarate: String,               // e.g. "SF7BW125"
    rssi: Option<i16>,              // Channel RSSI in dBm (uplink only)
    snr: Option<f32>,               // LoRa SNR in dB (uplink only)
    crc_status: Option<CrcStatus>,  // Ok | Bad | NoCrc (uplink only)

    // LoRaWAN metadata
    mtype: MType,                   // JoinRequest, UnconfirmedDataUp, etc.
    dev_addr: Option<String>,       // DevAddr (hex, data frames only)
    dev_eui: Option<String>,        // DevEUI (hex, join requests only)
    fcnt: Option<u32>,              // Frame counter (data frames only)
    fport: Option<u8>,              // Frame port (data frames only)
    payload_size: u16,              // PHY payload size in bytes

    // Computed
    toa_ms: Option<f64>,            // Time-on-Air in milliseconds
}
```

This model is the single source of truth for the storage layer, WebSocket broadcasts, and the WebUI. Protocol-specific details (Semtech binary headers, Protobuf wrappers) are not persisted — only the normalized fields above.

### 2.3 MITM Proxy Modes

#### 2.3.1 UDP Packet Forwarder Mode

The sniffer binds to the UDP port the gateway sends to (default 1700) and forwards packets to the actual LNS. It proxies all 6 Semtech protocol message types:

```
Gateway ──PUSH_DATA──▶ Sniffer ──PUSH_DATA──▶ LNS
Gateway ◀──PUSH_ACK── Sniffer ◀──PUSH_ACK── LNS
Gateway ──PULL_DATA──▶ Sniffer ──PULL_DATA──▶ LNS
Gateway ◀──PULL_ACK── Sniffer ◀──PULL_ACK── LNS
Gateway ◀─PULL_RESP── Sniffer ◀─PULL_RESP── LNS
Gateway ──TX_ACK────▶ Sniffer ──TX_ACK────▶ LNS
```

- The gateway is configured to point to the sniffer's address/port instead of the LNS directly
- The sniffer forwards all packets to the real LNS and relays responses back
- Token matching is preserved transparently
- For blocked messages, the sniffer drops the packet and does not forward it (optionally sending back an appropriate ACK/error to maintain protocol flow)

#### 2.3.2 Concentratord IPC Mode

The sniffer sits between the concentratord and the application consuming its events (e.g. ChirpStack Gateway Bridge):

```
Concentratord ──[ZMQ PUB]──▶ Sniffer ──[ZMQ PUB]──▶ Gateway Bridge
Concentratord ◀──[ZMQ REP]── Sniffer ◀──[ZMQ REP]── Gateway Bridge
```

- **Upstream (events):** The sniffer subscribes to concentratord's event socket (ZMQ SUB) and republishes events on its own PUB socket
- **Downstream (commands):** The sniffer binds a REP socket for incoming commands and forwards them to concentratord's command socket (ZMQ REQ)
- Protobuf messages are decoded for logging but forwarded as-is
- Blocked messages are dropped without forwarding

---

## 3. Functional Requirements

### 3.1 Packet Capture and Forwarding

| ID | Requirement | Priority |
|----|-------------|----------|
| F-01 | Proxy all 6 Semtech UDP protocol v2 message types transparently | Must |
| F-02 | Proxy Concentratord ZMQ events (UplinkFrame, GatewayStats) | Must |
| F-03 | Proxy Concentratord ZMQ commands (DownlinkFrame, GetGatewayId, etc.) | Must |
| F-04 | Decode LoRaWAN PHY header (MHDR) and extract MType | Must |
| F-05 | Extract MAC payload fields: DevAddr, FCtrl (ADR, ACK, FPending, FOptsLen), FCnt, FPort | Must |
| F-06 | Extract Join Request fields: AppEUI/JoinEUI, DevEUI, DevNonce | Must |
| F-07 | Extract Join Accept fields (unencrypted header): MHDR only (payload is encrypted) | Must |
| F-08 | Compute Time-on-Air for each frame | Should |
| F-09 | Parse and log all RF metadata (freq, DR, RSSI, SNR, timestamp, channel, CRC status) | Must |
| F-10 | Single gateway per sniffer instance (no multi-gateway routing required) | Must |

### 3.2 Message Blocking

| ID | Requirement | Priority |
|----|-------------|----------|
| F-20 | Block uplink messages matching filter criteria (prevent forwarding to LNS) | Must |
| F-21 | Block downlink messages matching filter criteria (prevent forwarding to gateway) | Must |
| F-22 | Filter criteria: MType (e.g. block all ConfirmedDataDown, block all JoinAccept) | Must |
| F-23 | Blocked messages are still logged with a "blocked" status and the name of the filter rule that matched | Must |
| F-24 | Option to suppress ACK responses for blocked messages (stealth mode): when enabled, the sniffer does not forward protocol-level acknowledgements (PUSH_ACK, PULL_ACK) so the end device or LNS is completely unaware the message was received | Must |
| F-25 | Blocking rules are managed via the configuration file only (not editable at runtime via WebUI) | Must |

### 3.3 Data Persistence

| ID | Requirement | Priority |
|----|-------------|----------|
| F-30 | Persist every captured message to an embedded SQLite database (WAL mode) | Must |
| F-31 | Store RF metadata: frequency, data rate, RSSI, SNR, channel, CRC status, timestamp | Must |
| F-32 | Store LoRaWAN metadata: MType, DevAddr/DevEUI, FCnt, FPort, payload size | Must |
| F-33 | Store proxy metadata: direction (up/down), action (forwarded/blocked), blocking filter name (if blocked), source protocol (UDP/IPC) | Must |
| F-35 | Application-level data retention policy: configurable TTL, periodic cleanup of expired rows | Must |
| F-36 | Database: **SQLite** (embedded, single-file, WAL mode). Chosen for minimal footprint (<1 MB), widest architecture support (including MIPS), zero deployment complexity, and sufficient throughput for LoRaWAN packet rates. See [TSDB.md](TSDB.md) for full evaluation. | — |

### 3.4 Web User Interface

| ID | Requirement | Priority |
|----|-------------|----------|
| F-40 | React 19 single-page application served by the Rust backend | Must |
| F-41 | Real-time message feed via WebSocket connection | Must |
| F-42 | Message table with columns: Timestamp, Direction, MType, DevAddr/DevEUI, FCnt, FPort, Freq, DR, RSSI, SNR, Size, ToA, Status (forwarded/blocked) | Must |
| F-43 | Filter messages by DevAddr or DevEUI | Must |
| F-44 | Filter messages by time range (default: last 5 minutes; persisted in browser `localStorage`) | Must |
| F-45 | Filter messages by MType (Join Request, Confirmed Up, Unconfirmed Up, etc.) | Must |
| F-46 | Filter messages by direction (uplink/downlink) | Should |
| F-47 | Filter messages by status (forwarded/blocked) | Should |
| F-48 | Color-coded signal quality indicators: RSSI thresholds (≥−100 dBm good, ≥−110 fair, below poor); SNR thresholds are spreading-factor-aware (margin above SF minimum decodable SNR: ≥+10 dB good, ≥+3 dB fair, below poor) | Should |
| F-49 | UI must be usable on 800x600 pixel screens (compact layout, no wasted space) and with touch input (no hover-only interactions, tap-friendly controls) | Must |
| F-50 | Sort messages by timestamp ascending or descending | Must |
| F-51 | Pause live feed and resume later, loading all messages received while paused | Must |
| F-52 | User option to hide/show individual table columns (persisted in browser `localStorage`) | Must |
| F-53 | Follow system theme (dark/light mode via `prefers-color-scheme` media query) | Must |

### 3.5 Network and Security

| ID | Requirement | Priority |
|----|-------------|----------|
| F-60 | Serve WebUI and WebSocket over plain HTTP/WS (TLS is handled by a reverse proxy, not the application) | Must |
| F-61 | WebSocket endpoint must work behind a TLS-terminating reverse proxy (no absolute `ws://` URLs; use relative paths or protocol-relative URLs so the browser auto-selects `ws://` or `wss://` based on the page origin) | Must |
| F-62 | CORS: allow all origins by default (the sniffer is a local/private network tool). Configurable `allowed_origins` list for restricted deployments. | Must |

---

## 4. Configuration

### 4.1 Format

TOML configuration file, following chirpstack-concentratord conventions. Parsed with `serde` + `toml` crate.

### 4.2 Configuration Structure

```toml
[sniffer]
  log_level = "INFO"                    # TRACE | DEBUG | INFO | WARN | ERROR | OFF
  log_to_syslog = false

[sniffer.api]
  # Web server and WebSocket endpoint
  bind = "0.0.0.0:8888"
  # Optional: allowed CORS origins (default: allow all)
  # cors_origins = ["https://example.com"]

[sniffer.database]
  # SQLite database file path
  path = "/var/lib/lorawan-sniffer/sniffer.db"
  retention = "30d"                     # Data retention period (humantime); app deletes rows older than this

[gateway]
  # Protocol mode: "udp" or "concentratord"
  protocol = "udp"

# --- UDP Packet Forwarder mode ---
[gateway.udp.upstream]
  # Listen address for the gateway (gateway sends packets here)
  bind = "0.0.0.0:1700"

[gateway.udp.downstream]
  # LNS server to forward packets to
  server = "localhost"
  port = 1700

# --- Concentratord IPC mode ---
[gateway.concentratord.upstream]
  # Concentratord sockets to subscribe/connect to
  event_url = "ipc:///tmp/concentratord_event"
  command_url = "ipc:///tmp/concentratord_command"

[gateway.concentratord.downstream]
  # Sockets to expose to the gateway bridge
  event_bind = "ipc:///tmp/sniffer_event"
  command_bind = "ipc:///tmp/sniffer_command"

# --- Blocking rules ---
# Filters match messages by MType. Direction determines whether
# the rule applies to uplinks, downlinks, or both.
# When suppress_ack is true (stealth mode), protocol-level ACKs
# are also suppressed so the sender is unaware the message arrived.

# Example: block all confirmed uplinks from reaching the LNS
[[filters]]
  name = "Block confirmed uplinks"
  enabled = true
  direction = "up"                      # "up" | "down" | "both"
  suppress_ack = false                  # true = also suppress PUSH_ACK/PULL_ACK
  mtype = ["ConfirmedDataUp"]           # List of MType names to match

# Example: block all downlinks to the end device (stealth)
[[filters]]
  name = "Block all downlinks (stealth)"
  enabled = true
  direction = "down"
  suppress_ack = true                   # Device won't know the LNS responded
  mtype = [
    "UnconfirmedDataDown",
    "ConfirmedDataDown",
    "JoinAccept"
  ]

# Example: block join accepts only
# [[filters]]
#   name = "Block join accepts"
#   enabled = false
#   direction = "down"
#   suppress_ack = false
#   mtype = ["JoinAccept"]
```

### 4.3 Configuration Loading

- Default path: `/etc/lorawan-sniffer/sniffer.toml`
- Override via CLI: `--config <path>`
- Subcommands following concentratord pattern:
  - `lorawan-sniffer` — run the sniffer (default)
  - `lorawan-sniffer configfile` — print default configuration to stdout
  - `lorawan-sniffer --help` — usage information

---

## 5. Technical Design

### 5.1 Project Structure

Follow chirpstack-concentratord's multi-crate workspace pattern:

```
lorawan-sniffer/
├── Cargo.toml                      # Workspace root
├── Cargo.lock
├── Makefile
├── sniffer.toml                    # Example configuration
│
├── lorawan-sniffer/                # Main binary crate
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs                 # Entry point, signal handling
│       ├── cmd/
│       │   ├── mod.rs
│       │   ├── root.rs             # Main run command
│       │   └── configfile.rs       # Print default config
│       ├── config/
│       │   └── mod.rs              # Configuration structs + parsing
│       └── proxy/
│           ├── mod.rs
│           ├── udp.rs              # UDP MITM proxy implementation
│           └── concentratord.rs    # ZMQ/IPC MITM proxy implementation
│
├── libsniffer/                     # Shared library crate
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs
│       ├── error.rs                # Error types (thiserror)
│       ├── decoder.rs              # LoRaWAN PHY decoder
│       ├── filter.rs               # Message matching and blocking engine
│       ├── storage.rs              # SQLite persistence layer (rusqlite)
│       ├── metadata.rs             # Unified MessageRecord and related types
│       └── toa.rs                  # Time-on-Air calculation
│
├── webui/                          # React 19 frontend
│   ├── package.json
│   ├── tsconfig.json
│   ├── vite.config.ts
│   ├── index.html
│   └── src/
│       ├── main.tsx
│       ├── App.tsx
│       ├── hooks/
│       │   └── useWebSocket.ts     # WebSocket connection hook
│       ├── components/
│       │   ├── MessageTable.tsx     # Main message table (react-virtuoso TableVirtuoso)
│       │   ├── FilterBar.tsx       # Filter controls
│       │   ├── ColumnPicker.tsx    # Show/hide columns (persisted to localStorage)
│       │   └── StatusBar.tsx       # Connection status + live/paused toggle
│       ├── types/
│       │   └── messages.ts         # TypeScript type definitions
│       └── styles/
│           └── index.css
│
└── Dockerfile
```

### 5.2 Rust Coding Conventions

Following chirpstack-concentratord patterns:

| Aspect | Convention |
|--------|-----------|
| Error handling | `thiserror` for custom errors, `anyhow` for propagation |
| Configuration | `serde` + `toml`, `#[serde(default)]` for optional fields, `humantime_serde` for durations |
| Logging | `log` crate, `simple_logger` for stdout, `syslog` for daemon mode |
| Async runtime | `tokio` for all network I/O tasks (UDP, HTTP, WebSocket). SQLite writes via `tokio::task::spawn_blocking`. Dedicated `std::thread` for ZMQ only (concentratord mode). `tokio::sync::mpsc` for cross-boundary communication. |
| CLI | `clap` with subcommands |
| Serialization | `serde` + `serde_json` for JSON, `prost` for protobuf (concentratord mode) |
| Graceful shutdown | `tokio::signal` + `tokio_util::sync::CancellationToken`, propagated to all tasks and the ZMQ thread |
| ZeroMQ | `zmq` crate for IPC sockets |
| Build | `cross` for cross-compilation, musl targets |

### 5.3 Concurrency Architecture

Hybrid model: `tokio` async runtime for all network I/O, with a dedicated OS thread for ZMQ (concentratord mode only). ZMQ sockets are not async-compatible, so isolating them in a thread avoids leaking complexity into the async code. The thread communicates with async tasks via `tokio::sync::mpsc` channels.

```
tokio runtime (#[tokio::main])
  │
  ├── Signal handler task         (tokio::signal → CancellationToken)
  │
  ├── UDP upstream task            (tokio::net::UdpSocket, gateway → sniffer)
  ├── UDP downstream task          (tokio::net::UdpSocket, LNS → sniffer)
  │
  ├── SQLite writer task            (rusqlite, receives via tokio::sync::mpsc, runs blocking writes via tokio::task::spawn_blocking)
  ├── Retention cleanup task         (periodic DELETE WHERE timestamp < now - retention, configurable interval)
  │
  ├── axum server task             (HTTP + WebSocket)
  │   ├── Serves embedded static React build
  │   ├── WebSocket endpoint (real-time push via tokio::sync::broadcast)
  │   └── REST API for historical queries / filter management
  │
  └── Broadcast hub                (tokio::sync::broadcast channel)
      └── Receives MessageRecords, fans out to all WS clients

std::thread (only spawned in concentratord mode)
  ├── ZMQ event subscriber         (zmq SUB → re-publishes on zmq PUB)
  └── ZMQ command relay            (zmq REP ← bridge, zmq REQ → concentratord)
      │
      └── Communicates with tokio tasks via tokio::sync::mpsc channels
          (decoded MessageRecords sent into the async broadcast hub)
```

**Why hybrid:**
- The sniffer is primarily a network I/O application (UDP, HTTP, WebSocket) — async is the natural fit and avoids polling loops
- `axum` provides HTTP + WebSocket + static file serving in one framework with zero boilerplate
- `tokio::sync::broadcast` makes WebSocket fan-out to N clients trivial (no manual locking)
- `CancellationToken` gives cleaner shutdown than manual `recv_timeout` polling
- ZMQ is the only sync-only dependency — isolating it in one thread keeps the rest of the codebase clean
- Zero CPU when idle (event-driven), vs concentratord-style 10ms polling loops

### 5.4 Message Flow (Internal)

```
Proxy task/thread
  │
  ├── Receive raw packet (UDP: await recvfrom / ZMQ: zmq::poll)
  ├── Decode protocol header (Semtech binary or Protobuf)
  ├── Decode LoRaWAN PHY layer (MHDR + MAC payload)
  ├── Build MessageRecord { metadata, direction }
  ├── Evaluate filter rules → action: forward | block
  ├── Set MessageRecord.status = forwarded | blocked
  │
  ├──▶ Forward packet to destination (if not blocked)
  ├──▶ Send MessageRecord to SQLite writer (via tokio::sync::mpsc)
  └──▶ Send MessageRecord to broadcast hub (via tokio::sync::broadcast)
           └──▶ All connected WebSocket clients receive it
```

### 5.5 WebSocket Protocol

The backend pushes JSON messages to connected WebSocket clients:

```json
{
  "type": "message",
  "data": {
    "id": "01HQXYZ...",
    "timestamp": "2026-02-23T10:30:00.123Z",
    "direction": "up",
    "protocol": "udp",
    "status": "forwarded",
    "blocked_by": null,
    "mtype": "UnconfirmedDataUp",
    "dev_addr": "01ABCDEF",
    "dev_eui": null,
    "fcnt": 1234,
    "fport": 1,
    "freq_mhz": 868.1,
    "datarate": "SF7BW125",
    "rssi": -55,
    "snr": 8.5,
    "size": 23,
    "toa_ms": 56.6,
    "crc_status": "ok"
  }
}
```

Control messages from client to server (for filtering):

```json
{
  "type": "subscribe",
  "filters": {
    "dev_addr": ["01ABCDEF"],
    "mtype": ["UnconfirmedDataUp", "ConfirmedDataUp"],
    "direction": "up",
    "time_from": "2026-02-23T10:00:00Z"
  }
}
```

### 5.6 LoRaWAN PHY Decoder

The decoder extracts the following from raw LoRaWAN frames (Base64-decoded `data` field):

```
┌──────────┬─────────────────────────────────────────────┬─────┐
│ MHDR (1B)│              MAC Payload                     │ MIC │
│          │                                               │(4B) │
├──────────┼─────────────────────────────────────────────┼─────┤
│ MType(3b)│  Data frame:                                 │     │
│ RFU (3b) │  ┌─────────┬───────┬───────┬──────┬───────┐│     │
│ Major(2b)│  │DevAddr(4)│FCtrl(1)│FCnt(2)│FOpts │FPort+││     │
│          │  │         │       │       │(0-15)│FRMPay ││     │
│          │  └─────────┴───────┴───────┴──────┴───────┘│     │
│          │                                               │     │
│          │  Join Request:                                │     │
│          │  ┌──────────┬────────┬──────────┐           │     │
│          │  │JoinEUI(8)│DevEUI(8)│DevNonce(2)│          │     │
│          │  └──────────┴────────┴──────────┘           │     │
└──────────┴─────────────────────────────────────────────┴─────┘
```

**MType values:**

| Value | Name | Description |
|-------|------|-------------|
| 000 | JoinRequest | Join Request |
| 001 | JoinAccept | Join Accept |
| 010 | UnconfirmedDataUp | Unconfirmed Data Uplink |
| 011 | UnconfirmedDataDown | Unconfirmed Data Downlink |
| 100 | ConfirmedDataUp | Confirmed Data Uplink |
| 101 | ConfirmedDataDown | Confirmed Data Downlink |
| 110 | RejoinRequest | Rejoin Request |
| 111 | Proprietary | Proprietary |

### 5.7 Semtech UDP Protocol Handling

The sniffer must handle all 6 message types of the Semtech UDP Packet Forwarder protocol v2:

| Type | Code | Direction | Header | Payload |
|------|------|-----------|--------|---------|
| PUSH_DATA | 0x00 | GW → LNS | 12B (ver + token + type + MAC) | JSON: `{"rxpk":[...], "stat":{...}}` |
| PUSH_ACK | 0x01 | LNS → GW | 4B (ver + token + type) | None |
| PULL_DATA | 0x02 | GW → LNS | 12B (ver + token + type + MAC) | None |
| PULL_ACK | 0x04 | LNS → GW | 4B (ver + token + type) | None |
| PULL_RESP | 0x03 | LNS → GW | 4B (ver + token + type) | JSON: `{"txpk":{...}}` |
| TX_ACK | 0x05 | GW → LNS | 12B (ver + token + type + MAC) | JSON: `{"txpk_ack":{...}}` (optional) |

---

## 6. WebUI Design

### 6.1 Layout (800x600 Optimized)

```
┌──────────────────────────────────────────────────────────┐
│ LoRaWAN│[filters...]             [⚙ Cols] [↓CSV] [● ●] │  ← 36px header
│ Sniffer│                                                  │    (title | filters)
├──────────────────────────────────────────────────────────┤
│ TIME  ▼│DIR│TYPE  │DEVICE  │FCNT│FP│FREQ  │DR    │RSSI│S│  ← sortable header
├─────────┼───┼──────┼────────┼────┼──┼──────┼──────┼────┼─┤
│10:30:01 │ ▲ │UncUp │01ABCDEF│1234│ 1│868.10│SF7BW │ -55│●│  ← message rows
│10:30:00 │ ▲ │UncUp │01ABCDEF│1233│ 1│868.30│SF7BW │ -62│●│     (scrollable)
│10:29:58 │ ▼ │UncDn │01ABCDEF│  12│ 0│869.52│SF12  │    │●│  ← blocked (red tint
│10:29:55 │ ▲ │JReq  │        │    │  │868.10│SF10  │-101│●│     + left accent)
│  ...    │   │      │        │    │  │      │      │    │ │
├─────────┴───┴──────┴────────┴────┴──┴──────┴──────┴────┴─┤
│ Total: 1,234 │ Shown: 856 │ Blocked: 12 │ ● WS  © RAK │⏵│  ← 28px footer
└──────────────────────────────────────────────────────────┘
```

Legend:
- **Dir:** ▲ uplink (blue), ▼ downlink (amber)
- **Type:** colored pill badge — join/rejoin = purple, data-up = blue, data-down = amber
- **S:** colored dot — green (forwarded), red (blocked); row gets red tint + left border accent
- **▼ / ▲** on Time column: click to toggle sort ascending/descending
- **Cols** button: opens a dropdown to show/hide individual columns (preference saved to `localStorage`)
- **Live / Paused:** toggle to pause the real-time feed; pulsing dot when live; when resumed, all messages received while paused are loaded at once
- **© RAKwireless 2026** copyright notice in the footer, left of the live toggle
- Default view: last 5 minutes of messages (configurable, persisted to `localStorage`)
- Compact column headers (uppercase, muted), abbreviated type names
- No horizontal scroll at 800px width
- Monospace font for data columns
- Touch-friendly: all controls are tap targets (no hover-only interactions)
- Follows system dark/light theme via `prefers-color-scheme`

### 6.2 Filter Bar

Inline filter controls in the header area, collapsible to save vertical space:

- **DevAddr/DevEUI:** text input with autocomplete from seen devices
- **MType:** multi-select dropdown (JReq, JAccept, UncUp, UncDn, ConfUp, ConfDn)
- **Direction:** toggle (All / Up / Down)
- **Time range:** quick presets (Last 5m, 15m, 1h, 24h) + custom range
- **Status:** toggle (All / Forwarded / Blocked)
- **Clear all filters** button

### 6.3 Technology Stack

| Layer | Technology |
|-------|-----------|
| Framework | React 19 |
| Language | TypeScript |
| Build tool | Vite |
| Styling | CSS (minimal, no framework — optimized for 800x600, system dark/light theme) |
| WebSocket | Native browser WebSocket API |
| Table virtualization | `react-virtuoso` (`TableVirtuoso`) — native `<table>` rendering, sticky headers, `followOutput` for live streaming |
| State | React built-in state (useState/useReducer) |

### 6.4 Frontend Build and Serving

- Frontend is built at compile time and embedded into the Rust binary (via `include_dir` or `rust-embed`)
- Served by the Rust HTTP server (no separate web server needed)
- Single deployable binary contains both backend and frontend

---

## 7. Deployment

### 7.1 Build Targets

| Target | Architecture | Priority |
|--------|-------------|----------|
| `x86_64-unknown-linux-musl` | x86_64 (amd64) | Must |
| `aarch64-unknown-linux-musl` | ARM64 | Must |
| `mipsel-unknown-linux-musl` | MIPS (little-endian) | Nice to have |

### 7.2 Packaging

- Statically linked binary (musl libc)
- Docker image (multi-arch: amd64 + arm64)
- Docker Compose example (SQLite is embedded, no external DB service needed)

### 7.3 Resource Requirements

- Target memory usage: < 50 MB RSS (SQLite is embedded, no separate process)
- Single binary, no runtime dependencies
- SQLite database stored as a single file on disk (configurable path)

---

## 8. Dependencies (Rust Crates)

| Crate | Purpose |
|-------|---------|
| `tokio` | Async runtime (UDP sockets, task spawning, channels, signal handling) |
| `tokio-util` | `CancellationToken` for graceful shutdown |
| `axum` | HTTP server, WebSocket upgrade, static file serving |
| `tower-http` | Middleware (CORS, compression, static file serving) |
| `rusqlite` | SQLite bindings (with `bundled` feature for static compilation) |
| `clap` | CLI argument parsing |
| `serde`, `serde_json` | Serialization |
| `toml` | Configuration parsing |
| `log`, `simple_logger`, `syslog` | Logging |
| `thiserror`, `anyhow` | Error handling |
| `zmq` | ZeroMQ for Concentratord IPC (used from a dedicated `std::thread`) |
| `chirpstack_api` | Protobuf definitions (Concentratord mode) |
| `humantime-serde` | Duration parsing in config |
| `base64` | Payload encoding/decoding |
| `rust-embed` | Embed frontend assets into binary |

---

## 9. Open Questions

| # | Question | Options | Decision |
|---|----------|---------|----------|
| 1 | Which database to use? | QuestDB, TimescaleDB, InfluxDB, TDengine, VictoriaMetrics, DuckDB, SQLite | **SQLite** — embedded, <1 MB, MIPS support, sufficient throughput for LoRaWAN rates. Retention policies implemented in application. See [TSDB.md](TSDB.md). |
| 2 | Async runtime vs OS threads? | `tokio` (async), `std::thread` (matches concentratord) | **Hybrid: `tokio` for all network I/O + `axum` for HTTP/WS. Dedicated `std::thread` for ZMQ only (concentratord mode).** |
| 3 | HTTP/WebSocket framework? | `axum`, `warp`, `actix-web` | **`axum`** (tokio-native, HTTP + WS + static files in one framework) |
| 4 | Frontend table library? | TanStack Table (headless, 18 kB), AG Grid (batteries-included, 298 kB), react-virtuoso (native `<table>`, 17 kB) | **`react-virtuoso` (`TableVirtuoso`)** — renders native HTML `<table>`, sticky headers via `fixedHeaderContent`, `followOutput` for live feed auto-scroll, 17 kB gzipped. Sorting and column visibility handled with plain React state (~30 lines). |
| 5 | Should blocking rules be editable via WebUI API? | Yes (more convenient), No (config file only, simpler) | **No** — configuration file only. Simpler implementation, avoids config drift between file and runtime state. |
| 6 | Should raw MAC commands (FOpts) be decoded and displayed? | Yes (useful for debugging ADR, link checks), No (out of scope) | **No** — out of scope for v2. See Future Work (section 12). |

---

## 10. Milestones

### M1 — Core Proxy Engine
- TOML configuration loading
- UDP Packet Forwarder MITM proxy (all 6 message types)
- LoRaWAN PHY decoder (MHDR + MAC payload extraction)
- Console logging of decoded messages
- Graceful shutdown

### M2 — Filtering and Blocking
- Filter rule engine (match by MType, per direction)
- Uplink and downlink blocking
- ACK suppression (stealth mode)
- Configuration-driven filter rules

### M3 — Persistence
- SQLite integration (rusqlite with bundled feature, WAL mode)
- Message storage with full metadata
- Application-level data retention (periodic DELETE of expired rows, configurable TTL)

### M4 — WebUI
- React 19 application with Vite
- WebSocket real-time message feed with pause/resume
- Message table with filters (device, time, type, direction, status)
- Sortable by timestamp (asc/desc)
- Column visibility toggle (persisted to localStorage)
- System theme support (dark/light)
- Embedded in Rust binary

### M5 — Concentratord Support
- ZeroMQ event proxy (SUB → PUB)
- ZeroMQ command proxy (REP → REQ)
- Protobuf decode/encode for logging

### M6 — Polish and Packaging
- CORS configuration
- Cross-compilation (amd64 + arm64, mips optional)
- Docker image and Compose files
- Documentation

---

## 11. Future Work

Features explicitly deferred from this version:

- **FOpts / MAC command decoding:** Decode and display raw MAC commands in the FOpts field (ADR requests, link check, duty cycle, RX timing setup, etc.). Useful for debugging ADR behavior and network management commands.
- **Runtime filter management via WebUI:** Allow enabling/disabling blocking rules from the web interface without editing the config file and restarting.
- **Multi-gateway support:** Route and identify messages from multiple gateways in a single sniffer instance.
- **Message detail view:** Expandable row in the table to show full metadata for a single message.
- **Application payload decryption:** Optional decryption of FRMPayload when the user provides AppSKey/NwkSKey.
- **Export:** CSV/JSON export of filtered message history.
- **Filters:** Block messages based on other criteria, like DevAddr or Gateway EUI
- **WebUI configuration:** Change block criteria in the WebUI

---

## 12. References

- [Semtech UDP Packet Forwarder Protocol](https://github.com/Lora-net/sx1302_hal/blob/master/packet_forwarder/PROTOCOL.md)
- [ChirpStack Concentratord](https://github.com/chirpstack/chirpstack-concentratord)
- [LoRaWAN L2 1.0.4 Specification](https://resources.lora-alliance.org/technical-specifications)
- LoRaWAN Packet Sniffer v1 (this repository, `v1/` folder)
