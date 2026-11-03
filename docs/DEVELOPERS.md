# Developer Guide

This document is written for programmers who are new to Rust but comfortable with software development concepts. It explains the architecture of the LoRaWAN Packet Sniffer, the design decisions behind it, and what every file does — with extra detail on Rust language features when something interesting is happening.

---

## Table of Contents

1. [What the software does](#1-what-the-software-does)
2. [Repository layout](#2-repository-layout)
3. [Cargo workspaces — how Rust organises multi-crate projects](#3-cargo-workspaces)
4. [Architecture overview](#4-architecture-overview)
5. [Data flow end-to-end](#5-data-flow-end-to-end)
6. [Key Rust concepts you will encounter](#6-key-rust-concepts-you-will-encounter)
7. [File-by-file reference — `core/`](#7-file-by-file-reference--core)
8. [File-by-file reference — `lorawan-sniffer`](#8-file-by-file-reference--lorawan-sniffer)
9. [File-by-file reference — `webui`](#9-file-by-file-reference--webui)
10. [Build, release and packaging](#10-build-release-and-packaging)
11. [Design decisions](#11-design-decisions)
12. [Common maintenance tasks](#12-common-maintenance-tasks)

---

## 1. What the software does

The sniffer is a **transparent man-in-the-middle (MITM) proxy** for LoRaWAN gateways. It inserts itself between a gateway and a LoRaWAN Network Server (LNS):

```
Gateway  <──>  lorawan-sniffer  <──>  LNS / Gateway Bridge
```

Every packet that flows through is:

1. **Decoded** — LoRaWAN header fields (MType, DevAddr, DevEUI, FCnt, FPort) are extracted without requiring session keys. A frame that cannot be a valid LoRaWAN frame (failed CRC, reserved header bits, wrong length) is marked *malformed* and gets no identifiers at all.
2. **Filtered** — configurable rules can block specific packet types in specific directions, optionally for specific gateways.
3. **Persisted** — every packet is written to a SQLite database with a configurable retention window.
4. **Broadcast** — every packet is immediately pushed over WebSocket to any connected browser.
5. **Published** — optionally, every packet is also published to an MQTT broker.

A React web application (compiled into the binary) provides a real-time dashboard. It also loads the history from the database, lets an operator attach labels to devices, and can be protected by a login with a shared token.

Two gateway protocols are supported:

- **Semtech UDP Packet Forwarder** — the most common open protocol, used by nearly all LoRa gateways. Any number of gateways can point at one sniffer. Without an upstream LNS it runs in *listen mode*: it only records what the gateways send (status `Captured`) and, if asked, answers their keepalives so they consider their server alive.
- **ChirpStack Concentratord** — a ZeroMQ/Protobuf protocol used by ChirpStack-based gateway software.

---

## 2. Repository layout

```
lorawan-packet-sniffer/
├── Cargo.toml                  # Workspace root (no code here, just glue) and the release profile
├── Cargo.lock                  # Exact dependency versions (committed for binaries)
├── rust-toolchain.toml         # Pins the Rust compiler channel and components
├── Cross.toml                  # Settings for `cross`: passes SOURCE_DATE_EPOCH into the build container
├── .cargo/config.toml          # Static-linking flags for the musl release targets
├── Makefile                    # Build, test, release archive, Debian package and Docker commands
├── Dockerfile                  # Multi-stage image built from source (x86_64)
├── .dockerignore               # Keeps target/, node_modules/, .git/ and docs/ out of the build context
├── .gitignore                  # Ignores target/, dist/, node_modules/, webui/dist/ and database files
├── docker-compose-udp.yml      # Docker Compose example for the Semtech UDP backend
├── docker-compose-concentratord.yml  # Docker Compose example for the Concentratord backend
├── sniffer.toml                # Example configuration (what `make run` uses)
├── README.md                   # User documentation
├── CHANGELOG.md                # Release notes; the release workflow publishes the newest section
├── CLAUDE.md                   # Short orientation file for AI coding assistants
├── LICENSE                     # MIT licence, shipped in every archive and package
│
├── .github/workflows/
│   └── release.yml             # Release pipeline: check, test, build, publish (section 10)
│
├── docs/
│   ├── DEVELOPERS.md           # This guide
│   ├── MOBILE.md               # Touchscreen report for the WebUI
│   ├── PRD.md                  # Original product requirements
│   ├── TSDB.md                 # Comparison of time-series databases (why SQLite)
│   └── images/webui.png        # Screenshot used by the README
│
├── docker/
│   ├── Dockerfile.release      # Release image, assembled from the release binaries (nothing is compiled)
│   ├── docker-entrypoint.sh    # Gives every environment variable its default, then starts the sniffer
│   └── sniffer.toml            # The image's configuration: "$VARIABLE" references filled from the environment
│
├── packaging/                  # What `make dist` puts into the release archives and Debian packages
│   ├── package.sh              # Builds the .tar.gz release archive from a cross-compiled binary
│   ├── package-deb.sh          # Builds the .deb from a release archive
│   ├── install.sh              # Install, upgrade and --uninstall as a systemd service (archive installs)
│   ├── lorawan-sniffer.service # The systemd unit (sandboxed, dynamic user)
│   ├── concentratord.conf      # systemd drop-in that makes the unit work in Concentratord mode
│   ├── sniffer.toml            # Configuration template of a system install
│   ├── sniffer.env             # Template of the secrets file the unit reads (EnvironmentFile)
│   └── deb/
│       ├── control             # Package metadata (placeholders filled by package-deb.sh)
│       ├── conffiles           # The two files dpkg treats as configuration
│       ├── copyright           # Licence text of the package
│       ├── lintian-overrides   # Two documented lintian exceptions for the static binary
│       ├── preinst, postinst, prerm, postrm   # Maintainer scripts, each with one debhelper token
│       ├── check-autoscripts.sh               # Compares them with debhelper's own output
│       └── autoscripts/        # debhelper 13.11.4's systemd snippets, byte for byte
│           ├── postinst-systemd-enable, postinst-systemd-restart
│           ├── prerm-systemd-restart
│           ├── postrm-systemd, postrm-systemd-reload-only
│           ├── SHA256SUMS      # Checksums package-deb.sh verifies before using the snippets
│           └── README.md       # Where the snippets come from and their licence
│
├── tools/
│   ├── gwsim.py                # Semtech UDP gateway simulator and fake LNS (standard library only)
│   └── procstat.sh             # Samples the memory and file descriptors of a running process
│
├── lorawan-sniffer/            # The single crate — library logic + executable
│   ├── Cargo.toml
│   ├── tests/
│   │   └── shutdown.rs         # Integration test: SIGTERM stops the real binary cleanly
│   └── src/
│       ├── main.rs             # Entry point: CLI parsing, configuration check and logger setup
│       ├── test_support.rs     # Test-only: a logger that captures every line (shared by several tests)
│       ├── core/               # Protocol-agnostic core logic (no network, no HTTP)
│       │   ├── mod.rs          # Module declarations
│       │   ├── error.rs        # Unified error type
│       │   ├── eui.rs          # Eui64: parsing and display of gateway EUIs, DevEUIs and JoinEUIs
│       │   ├── metadata.rs     # Core data types (MessageRecord, MType, etc.)
│       │   ├── decoder.rs      # LoRaWAN PHY payload decoder and malformed-frame rules
│       │   ├── filter.rs       # Filter rule evaluation engine
│       │   ├── toa.rs          # Time-on-Air calculator
│       │   ├── history.rs      # What a history request asks for; label case folding
│       │   ├── labels.rs       # Validation of device identifiers and label text
│       │   ├── seen.rs         # Bounded list of the device identifiers seen live
│       │   ├── storage.rs      # SQLite persistence layer (writer + read-only connection)
│       │   ├── storage/
│       │   │   ├── history_tests.rs        # Tests of Storage::query_history
│       │   │   ├── history_large_tests.rs  # Million-row measurement (ignored by default)
│       │   │   └── secret_tests.rs         # Tests of the stored session secret
│       │   ├── writer.rs       # WriterItem: what the storage queue carries
│       │   ├── random.rs       # Random bytes that can fail: RandomSource, generate_token
│       │   ├── ratelimit.rs    # RateLimitedLog: at most one log line per interval
│       │   └── linelog.rs      # warn_line! / error_line!: log lines a test can read back
│       ├── config/
│       │   └── mod.rs          # Configuration structs, TOML loader and validation
│       ├── cmd/
│       │   ├── mod.rs          # Module re-exports
│       │   ├── root.rs         # Main run loop: spawns all async tasks
│       │   └── configfile.rs   # `configfile` subcommand (prints template, with a generated login token)
│       ├── api/
│       │   ├── mod.rs          # HTTP/WebSocket server (axum), router, layer order, label endpoints
│       │   ├── host.rs         # Host check (DNS-rebinding protection), outermost layer
│       │   ├── auth.rs         # Optional WebUI login: sessions, lockout, require_login, login/logout
│       │   ├── messages.rs     # GET /api/messages: message history
│       │   ├── labels.rs       # LabelWriter: the one task that changes device labels
│       │   ├── auth_tests.rs   # Unit tests of the login core
│       │   ├── login_tests.rs  # Router tests of the login
│       │   ├── message_tests.rs# Router tests of /api/messages and the reload guarantee
│       │   └── label_tests.rs  # Tests of the label endpoints, /ws/labels and the Host check
│       ├── mqtt/
│       │   ├── mod.rs          # MQTT publisher: start(), event loop, publisher task, counters
│       │   ├── topic.rs        # Topic templates ({gateway_eui}, {mtype}, ...)
│       │   ├── tls.rs          # rustls client configuration (CA file, mutual TLS)
│       │   └── tests.rs        # Publisher tests against an in-process fake broker
│       └── proxy/
│           ├── mod.rs          # Shared proxy code: frame_fields, classify, Outputs, log_message
│           ├── gw.rs           # Protobuf message definitions (hand-written)
│           ├── concentratord.rs# ChirpStack Concentratord ZMQ MITM proxy
│           └── udp/
│               ├── mod.rs      # Semtech UDP proxy: main loop, both directions, record builders
│               ├── packet.rs   # Wire format: header, JSON structs, synthetic ACKs
│               ├── session.rs  # One GatewaySession per gateway EUI, each with its own LNS socket
│               ├── loopguard.rs# Protection against the proxy forwarding to itself
│               └── tests.rs    # Loopback tests with fake gateways and a fake LNS
│
└── webui/                      # React + TypeScript frontend
    ├── package.json            # Scripts (dev, build, test) and dependencies; its version feeds the footer
    ├── package-lock.json
    ├── index.html              # The one HTML page; Vite injects the bundle
    ├── vite.config.ts          # Build settings, dev-server proxy to the sniffer, __APP_VERSION__
    ├── tsconfig.json
    ├── tsconfig.node.json
    ├── src/
    │   ├── main.tsx            # React app entry point
    │   ├── App.tsx             # The login gate, then the sniffer page (SnifferApp)
    │   ├── session.ts          # Login's WebUI side: checkSession, logIn, logOut, reportUnauthorized
    │   ├── vite-env.d.ts       # Type declarations for Vite and the __APP_VERSION__ constant
    │   ├── types/
    │   │   └── messages.ts     # TypeScript types mirroring MessageRecord and the WebSocket frames
    │   ├── lib/
    │   │   └── history.ts      # Pure logic: filter rules, time range, merging, counts, request state machine
    │   ├── components/
    │   │   ├── MessageTable.tsx # Packet table with virtual scrolling
    │   │   ├── FilterBar.tsx   # Device / type / status / gateway filters and the time selector with its day picker
    │   │   ├── ColumnPicker.tsx # ⚙ Settings menu: columns, Export CSV, Log out
    │   │   ├── LabelDialog.tsx # Modal to set, change or remove a device label
    │   │   ├── LoginPage.tsx   # The token field and Log in button
    │   │   ├── HistoryNotice.tsx # The strip under the table: cap, errors, database behind
    │   │   └── StatusBar.tsx   # Counts, connection indicator, version, live/pause toggle
    │   ├── hooks/
    │   │   ├── useWebSocket.ts # WebSocket connection hook (used for /ws and /ws/labels)
    │   │   ├── useHistory.ts   # Loads the table from /api/messages and merges live records
    │   │   └── useLabels.ts    # Device labels from /ws/labels, plus save and remove calls
    │   └── styles/
    │       └── index.css       # All styling (pure CSS, theme variables, dark mode)
    └── test/
        ├── history.test.mjs    # `npm test`: plain Node test of lib/history.ts
        └── history-vectors.json # Filter cases that the Rust tests run too
```

Some folders are produced locally and are not in git: `target/` (Cargo), `webui/node_modules/` and `webui/dist/` (npm), and `dist/` (release archives and packages from `make dist`), as are `*.db` database files. The Rust build embeds `webui/dist/`, so the WebUI has to be built before a release build of the binary (`make build-full` does both).

---

## 3. Cargo workspaces

In Rust, a **crate** is the basic unit of compilation — like a package in Node.js or a module in Python. This repository has a single crate (`lorawan-sniffer`) inside a **workspace**:

```toml
# Cargo.toml (root)
[workspace]
  resolver = "3"
  members = [
    "lorawan-sniffer",
  ]
```

The workspace root `Cargo.toml` also defines `[workspace.dependencies]` — a central list of all external library versions. The crate's `Cargo.toml` references these with `{ workspace = true }` instead of pinning version numbers inline. This is useful when the workspace grows additional members later, since versions stay in one place.

`cargo build`, `cargo test`, and `cargo clippy` run across the whole workspace by default.

---

## 4. Architecture overview

```
                         ┌─────────────────────────────────────────────────┐
                         │                lorawan-sniffer binary           │
                         │                                                 │
Gateway                  │   ┌──────────┐    ┌────────────┐                │
  UDP ─────────────────► │   │          │    │            │   ┌──────────┐ │
                         │   │  proxy/  │───►│  cmd/root  │──►│ storage  │ │  SQLite DB
  ZMQ ─────────────────► │   │  (MITM)  │    │  (tasks)   │   │ (writer) │ │  on disk
                         │   │          │    │            │   └──────────┘ │
LNS / Bridge             │   └──────────┘    └─────┬──────┘                │
  UDP ◄─────────────────►│                         │                       │
  ZMQ ◄─────────────────►│                   ┌─────▼──────┐                │
                         │                   │  api/mod   │◄── Browser     │
                         │                   │  (axum)    │──► WebSocket   │
                         │                   └────────────┘                │
                         └─────────────────────────────────────────────────┘
```

The diagram shows the main path. The label writer, the live-identifier listener and the MQTT publisher hang off the same channels, as the table below says.

The binary runs these concurrent tasks:

| Task | What it does |
|------|-------------|
| **Proxy** (a tokio loop for UDP, two OS threads for ZMQ) | Receives packets, decodes them, applies filters, forwards or blocks them (in UDP listen mode, only records them) |
| **Storage writer** | Receives `WriterItem`s over an async channel, writes records to SQLite and answers settle markers |
| **API server** | Serves HTTP REST + WebSocket + the embedded React SPA, behind the Host check and, if enabled, the login |
| **Label writer** (`api/labels.rs`) | The one task that changes device labels: saves, then publishes the event for `/ws/labels` |
| **Live-identifier listener** | Subscribes to the record broadcast and notes which DevAddr/DevEUI values were seen live (`core/seen.rs`) |
| **MQTT publisher** (two tasks, only when enabled) | Publishes every record to the broker, best effort (`mqtt/`) |
| **Retention cleanup** | Periodically deletes rows older than the configured TTL, and the labels of devices that have disappeared |

All tasks share these handles:
- `mpsc::Sender<WriterItem>` — the storage queue (one sender per proxy, one receiver in the writer task). An item (`WriterItem`, in `core/writer.rs`) is a record to insert, or a `Settle` marker that history requests use to wait for the writer.
- `broadcast::Sender<Arc<String>>` — the WebSocket fan-out (one sender, N receivers — one per connected browser); a second one carries label events to the `/ws/labels` clients
- `CancellationToken` — a cooperative shutdown signal
- `Arc<AtomicU64>` — the number of inserts that failed, incremented by the storage writer and reported by the API in `X-Storage-Write-Failures`
- `SeenDevices` — the bounded list of identifiers seen live, shared by the listener, the label writer and the retention task

`cmd/root.rs` creates all of these and wires them together; section 8 lists the order.

---

## 5. Data flow end-to-end

Here is the journey of a single uplink packet from gateway to database:

```
1.  Gateway sends a UDP PUSH_DATA packet to port 1700.

2.  proxy/udp/mod.rs receives it in the tokio event loop (recv_from).

3.  The binary header is parsed:
      [version(1)] [token(2)] [0x00=PUSH_DATA] [gateway_eui(8)] [JSON...]
    The gateway EUI selects the gateway's session, created on first contact.
    In proxy mode the session owns a socket of its own, connected to the LNS.
    (A datagram that is the proxy's own traffic coming back is dropped before
    any of this: see "Loop protection" in the UDP entry of section 8.)

4.  The JSON body is deserialized into PushDataPayload { rxpk: Vec<RxPk>, ... }.

5.  For each RxPk (one RF packet):
      a. decoder::decode_phy(payload, crc) base64-decodes the PHY payload and
         applies the malformed-frame rules (CRC failed, bad MHDR, wrong
         length). A frame that passes has its LoRaWAN header parsed to
         extract MType, DevAddr, FCnt, FPort; a malformed one keeps only its
         MType, size and reason.
      b. classify() runs filter::evaluate() over the configured rules in
         order. First match wins. If no rule matches → Forwarded. A malformed
         frame has no type for the rules, so rules listing MTypes skip it.
         In listen mode the rules are not consulted: the status is Captured.
      c. A MessageRecord is assembled from all available fields.
      d. The record is logged at DEBUG level (log_message).
      e. Outputs::emit queues the record for MQTT and for storage (on the
         mpsc channel, as WriterItem::Record), and only then serialises it
         to JSON and sends it on the broadcast channel
         → API server → all WebSocket clients.
         Storage comes before the broadcast on purpose: see "Reading history".

6.  If any rxpk was blocked, the PUSH_DATA is NOT forwarded to the LNS.
    Instead, a synthetic PUSH_ACK is sent back to the gateway so it
    does not retry (unless suppress_ack = true, stealth mode).
    In listen mode nothing is forwarded or blocked; with listen_acks the
    sniffer answers the PUSH_DATA itself with a PUSH_ACK.

7.  The storage writer takes one item at a time. For a record it calls
    tokio::task::spawn_blocking(|| storage.insert(&record))
    to run the SQLite write on a thread-pool thread (not blocking the async event loop),
    and waits for it before taking the next item. An insert that fails, or panics,
    is logged and counted. A Settle marker is answered at once: everything queued
    before it has been processed.
```

### Reading history

The other direction is the WebUI asking the database what it holds. `GET /api/messages` accepts a time window (`from`, inclusive, and `before`, exclusive; the older `since` is still understood), the filters of the WebUI, `newest`, `count=true` and `settle=true`, and answers with the newest N matching records, oldest first, and their exact count. The WebUI's time selector is either a span counted back from now (5 minutes to 24 hours: only `from`) or one local day picked from the browser's date picker (`from` is its local midnight and `before` the next one, so a day of 23 or 25 hours is still exactly one day, and two adjacent days never share a record). `api/messages.rs` parses the request into a `HistoryQuery`, and `Storage::query_history` runs it on a **second, read-only SQLite connection** (WAL mode only), so a scan of a large range never holds the mutex the writer needs. The README documents the parameters and headers; the bounds (one query at a time, three response bodies in memory) are described under `api/messages.rs` in section 8. A live record is merged into the table only if it falls inside the selected window, so a past day shows no new rows.

The WebUI reloads the selected range every time `/ws` opens, and that reload must not miss a record that was broadcast during the outage, whatever the timing. Four orderings in the code give that guarantee, and none of them is a delay:

1. `Outputs::emit` queues a record for storage **before** it broadcasts it, and looks for listeners only after queueing, so a client that subscribes in between either gets the record live or has it ahead of its marker.
2. `/ws` takes its broadcast subscription in the handler, **before** the `101` answer, so everything broadcast after the browser's `open` event reaches the socket.
3. The browser sends the reload after `open`, with `settle=true`: the handler puts a `WriterItem::Settle` marker in the storage queue and waits (up to 5 s) for the answer.
4. The writer handles the queue in order and answers a marker only after everything before it.

A record saved in the range was therefore queued before the marker, and is in the reload, or was broadcast after the subscription, and reached the browser live. If you change `emit`, the `/ws` handler, the writer loop or the reload, keep all four. "Settled" means processed (saved, or its failure logged and counted in the shared `write_failures` counter), not saved; the counter is reported in `X-Storage-Write-Failures`.

### Labels and the live-identifier list

Device labels never touch the forwarding path. A label belongs to an identifier: the DevAddr of a data frame, else the DevEUI of a join request, in the text form records carry. A malformed record has no identifier, so it can never be labelled. Labels live in the `device_labels` table, and every change goes through one task, the **label writer** (`api/labels.rs`): `PUT`/`DELETE /api/labels/{device}` and the retention sweep send it a command; it saves, publishes the event on the label `broadcast` channel, and only then answers. That is why `/ws/labels` clients see changes in the order they were committed.

To label a device whose records the storage writer dropped (queue full), `run_seen_listener` subscribes to the record broadcast, reads the identifier out of each envelope and notes it in a bounded `SeenDevices` list. `PUT` accepts an identifier that has a stored record or was seen live within the retention period. The WebUI joins labels to rows by identifier, so a label appears on every row of that device as soon as the `/ws/labels` event arrives.

### The login

With `[sniffer.api] login = true` the router gets a layer, `require_login`, in front of its routes: everything under `/ws` and `/api/` except `/api/login` and `/api/logout` needs a session cookie or `Authorization: Bearer <token>` (`/api/session` included: a `401` from it is how the page learns it must show the login), and the WebUI bundle stays public because it is the page that asks for the token. The layers, from the outside in, are the Host check, CORS (only with `cors_origins`), `require_login`, and the routes, so a foreign `Host` is refused before login is considered and a CORS preflight needs no credential. Sessions are stateless, signed values (nothing is stored per session), and a token that is presented after ten wrong ones in a minute is refused unexamined. The parts are described in `api/mod.rs` and `api/auth.rs` in section 8. With login off none of this exists: no cookie, no `401`, and `/api/login` and `/api/logout` are unknown paths.

---

## 6. Key Rust concepts you will encounter

This section explains Rust features that appear frequently in the code, aimed at developers coming from languages like Python, Go, JavaScript, or C++.

### 6.1 Ownership and cloning

Rust enforces that every piece of data has exactly one owner. When you pass a value to a function, the function takes ownership and the original variable is no longer valid. To share data, you either borrow it (`&T`), or clone it.

You will see `.clone()` in `cmd/root.rs` when handing the `Storage` and `CancellationToken` handles to multiple tasks:

```rust
let writer_storage = storage.clone();
let api_storage = storage.clone();
```

`Storage` is cheap to clone because internally it is just a couple of `Arc<Mutex<Connection>>` handles (the writer, and in WAL mode a read-only reader) — cloning it just increments reference counters, it does not copy a database connection.

### 6.2 `Arc<T>` — shared ownership across threads

`Arc` stands for **Atomically Reference Counted**. It is a smart pointer that wraps a value and keeps track of how many places hold a reference to it. When the last `Arc` is dropped, the inner value is freed.

`Arc` is the safe way to share data between threads in Rust. You will see it used in two places:

1. `Arc<Mutex<rusqlite::Connection>>` — the SQLite connections (the writer, and in WAL mode a read-only reader) shared between all tasks.
2. `Arc<String>` — pre-serialised JSON strings sent over the broadcast channel. Wrapping in `Arc` means the JSON is serialised once but the pointer is cheaply copied to every WebSocket subscriber.

### 6.3 `Mutex<T>` — mutual exclusion

`Mutex` allows only one thread at a time to access the inner value. You call `.lock()` to get a `MutexGuard`. When the guard goes out of scope, the lock is released automatically — no explicit unlock needed (RAII pattern, analogous to Python's `with` statement).

```rust
let conn = self.conn.lock().map_err(|_| SnifferError::LockPoisoned)?;
conn.execute(...)?;
// lock released here automatically when `conn` drops
```

A lock becomes "poisoned" if a thread panics while holding it. The code handles this gracefully by returning a `LockPoisoned` error instead of propagating the panic.

### 6.4 `Result<T, E>` and the `?` operator

Rust has no exceptions. Functions that can fail return `Result<T, E>`:
- `Ok(value)` if successful
- `Err(error)` if something went wrong

The `?` operator is syntactic sugar: if the result is `Err`, it immediately returns that error from the current function. If it is `Ok`, it unwraps the value. Equivalent to checking and re-throwing in Java/Python.

```rust
let bytes = sub.recv_bytes(0)?;   // if this fails, return the error immediately
```

### 6.5 `Option<T>` — nullable values

Rust has no `null`. Instead, a value that might be absent is represented as `Option<T>`:
- `Some(value)` if present
- `None` if absent

You will see `Option` everywhere for fields that are not always present (e.g., `rssi` is only available for uplinks, `dev_addr` only for data frames).

```rust
pub rssi: Option<i16>,   // None for downlinks
pub dev_addr: Option<String>, // None for join frames
```

### 6.6 Enums as algebraic types

Rust enums are much more powerful than C/Java enums. Each variant can carry data. This is used in the error type and the protocol types:

```rust
pub enum SnifferError {
    Decode(String),          // carries the error message
    Database(#[from] rusqlite::Error), // automatically converts rusqlite errors
    LockPoisoned,            // no data, just a signal
}
```

The `#[from]` attribute on `Database` means Rust automatically generates a conversion: any `rusqlite::Error` can be turned into `SnifferError::Database(e)` by the `?` operator.

### 6.7 Traits — Rust's interfaces

A **trait** defines a set of methods a type must implement. Common traits you will see:

| Trait | What it means |
|-------|--------------|
| `Clone` | The type can be duplicated with `.clone()` |
| `Debug` | The type can be printed with `{:?}` |
| `Serialize` / `Deserialize` | The type can be converted to/from JSON (via serde) |
| `Send` | The type can be moved between threads |
| `Sync` | The type can be shared by reference between threads |

Most traits are derived automatically with `#[derive(...)]`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageRecord { ... }
```

This generates the implementation code at compile time — you do not write it yourself.

### 6.8 `async` / `await` and tokio

Rust's async model is cooperative. `async fn` functions return a `Future` — a description of work to be done. They do not run until you `.await` them. The tokio runtime drives all futures on a thread pool.

The key insight: `await` is a yield point. While waiting for I/O (network, timer), the thread is freed to run other tasks. This is why the entire UDP proxy, API server, storage writer, and retention task can all run "concurrently" on just a few threads.

`tokio::select!` waits for the first of several futures to complete:

```rust
tokio::select! {
    result = upstream.recv_from(&mut buf) => { /* handle packet */ }
    _ = cancel.cancelled() => { break; }
}
```

This is how the proxy listens for both incoming packets and a shutdown signal simultaneously.

### 6.9 Blocking code in async context — `spawn_blocking`

SQLite and ZeroMQ are synchronous (blocking) libraries. Calling them directly inside an `async` function would block the tokio thread and starve other tasks.

The solution is `tokio::task::spawn_blocking`, which offloads blocking work to a dedicated thread pool that is separate from the async worker threads:

```rust
tokio::task::spawn_blocking(move || s.insert(&record)).await
```

The `move` keyword transfers ownership of the captured variables (`s` and `record`) into the closure.

### 6.10 Channels — communicating between tasks

Rust encourages message-passing rather than shared mutable state. Three channel types are used:

**`mpsc`** (multiple-producer, single-consumer): Used for the storage queue. Multiple proxy tasks send `WriterItem`s (records, and the settle markers that history requests queue); one storage writer task receives them. The label writer and the MQTT publisher have queues of their own.

```rust
let (tx, rx) = mpsc::channel::<WriterItem>(1024);
```

**`broadcast`**: Used for WebSocket fan-out. One proxy task sends a JSON string; every connected WebSocket client's task receives a copy.

```rust
let (tx, _) = broadcast::channel::<Arc<String>>(512);
let rx = tx.subscribe(); // each WebSocket client gets its own receiver
```

**`oneshot`**: a channel that carries exactly one value. A settle marker and every label command carry a `oneshot::Sender` for their answer, and the requester `.await`s the receiving end.

### 6.11 Trait objects as test seams

Some types take a *trait object* (`dyn Trait`) instead of a concrete dependency, so that a test can put something else in its place without changing the code under test. `Arc<dyn RandomSource>` (`core/random.rs`) is the system's random generator in production and a source that always fails in a test, which is how the "random source failed" paths are exercised. `Clock` in `api/auth.rs` is the real tokio clock in production and a manual one in the lockout tests, which move the one-minute window without sleeping. `process_items` in `cmd/root.rs` takes the insert as a closure, so a test can make it fail, panic or wait. Code that exists only for tests sits behind `#[cfg(test)]`: it is not compiled into the release binary at all (`test_support.rs`, the capture half of `core/linelog.rs`, the `spawn_for_test` constructor of the label writer).

---

## 7. File-by-file reference — `core/`

The `core/` directory holds the protocol-agnostic logic: no network code and no HTTP, and (apart from the ends of tokio channels that `writer.rs` names) nothing async. These modules could be tested in complete isolation even if the surrounding binary were removed.

The entry point is `core/mod.rs`, which simply declares the submodules:

```rust
pub mod decoder;
pub mod error;
pub mod eui;
pub mod filter;
pub mod history;
pub mod labels;
pub mod linelog;
pub mod metadata;
pub mod random;
pub mod ratelimit;
pub mod seen;
pub mod storage;
pub mod toa;
pub mod writer;
```

In Rust, every file must be explicitly declared as a module before it can be used. Modules are referenced from other files in the same crate as `crate::core::filter`, `crate::core::metadata`, etc.

---

### `core/error.rs`

Defines `SnifferError` — the single error type used throughout the library.

```rust
#[derive(Error, Debug)]
pub enum SnifferError {
    #[error("decode error: {0}")]
    Decode(String),
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("storage lock poisoned")]
    LockPoisoned,
    // ...
}
```

Three variants belong to history reads: `Cancelled` (the query stopped because whoever asked went away), `HistoryUnavailable` (a row filter was asked of a database that is not in WAL mode) and `Busy` (the reader's 1 s busy timeout ran out). The API turns the last two into `503` answers with a `code`. `Config` and `Io` (converted from `std::io::Error`) complete the enum.

**Design note:** The `thiserror` crate (`#[derive(Error)]`) generates the `std::error::Error` trait implementation automatically. The `#[error("...")]` attribute defines the human-readable message. `{0}` refers to the first field of the variant.

`#[from] rusqlite::Error` tells Rust: "if someone applies `?` to a `rusqlite::Error` in a function that returns `Result<_, SnifferError>`, automatically wrap it in `SnifferError::Database`." This means the calling code does not need explicit conversions.

**Why a single error type?** Having one enum with named variants lets callers match on specific error kinds without needing to know about internal dependencies (e.g., rusqlite).

---

### `core/eui.rs`

`Eui64`, the one type for 64-bit identifiers: a gateway EUI, a DevEUI or a JoinEUI. It is a tuple struct around `[u8; 8]` that always holds its bytes in **display order** (most significant byte first), whatever the source:

- `Eui64::from_be_slice` — for bytes already in display order, such as the gateway EUI in a Semtech UDP header.
- `Eui64::from_le_slice` — for bytes as a LoRaWAN frame carries them (JoinEUI and DevEUI are little-endian on the air); it reverses them.

`Display` writes eight colon-separated upper-case hex bytes (`AA:BB:CC:DD:00:11:22:33`), which is the text form used in records, in the database and in the WebUI. `FromStr` accepts exactly 16 hex digits, contiguous or in pairs split by one kind of separator (`:` or `-`), and rejects everything else (other lengths, mixed separators, a `0x` prefix, surrounding spaces) with `ParseEuiError`: leniency here would let a typo in a filter rule silently match nothing.

**Rust note:** implementing `FromStr` is what makes `"AA:BB:CC:DD:00:11:22:33".parse::<Eui64>()` work, and implementing `Display` is what makes `.to_string()` and `{}` work. Many types in the code get their text form this way instead of through ad-hoc helper functions.

---

### `core/metadata.rs`

Defines the core data types that flow through the entire system.

**`MType`** — LoRaWAN message type, decoded from the top 3 bits of the first byte (MHDR) of every LoRaWAN packet:

```rust
pub enum MType {
    JoinRequest,         // 000 — device wants to join the network
    JoinAccept,          // 001 — network accepts the join
    UnconfirmedDataUp,   // 010 — uplink, no ACK required
    UnconfirmedDataDown, // 011 — downlink, no ACK required
    ConfirmedDataUp,     // 100 — uplink, ACK required
    ConfirmedDataDown,   // 101 — downlink, ACK required
    RejoinRequest,       // 110 — roaming join
    Proprietary,         // 111 — vendor-specific
    Unknown,             // bits present but unrecognised
}
```

**`Direction`** — whether the packet travels from gateway to LNS (`Up`) or from LNS to gateway (`Down`).

**`Malformed`** — why a frame cannot be a valid LoRaWAN 1.0 / 1.1 frame: `Crc`, `Mhdr` or `Length`. It serialises as `"crc"`, `"mhdr"` or `"length"`, the same text the `malformed` column of SQLite holds. `MessageRecord.malformed` is `None` for a valid frame; on a malformed one, `dev_addr`, `dev_eui`, `join_eui`, `dev_nonce`, `fcnt`, `fport`, the five FCtrl flags and `f_opts_len` are all `None`.

**`Status`** — whether the packet was forwarded (`Forwarded`), dropped by a filter rule (`Blocked`), or only seen (`Captured`: the UDP proxy is in listen mode, so there is no upstream to forward to and no rule is consulted).

**`Protocol`** (`Udp` or `Concentratord`) says which backend captured a record, and **`CrcStatus`** (`Ok`, `Fail`, `NoCrc`) is the CRC result the gateway reported for an uplink.

**`MessageRecord`** — the canonical, protocol-agnostic struct that represents one captured packet. It is:
- Stored in SQLite (via `storage.rs`)
- Sent over WebSocket as JSON (via `api/mod.rs`)
- Returned by the REST API
- Published to MQTT as JSON, when enabled

Every field has a comment explaining its semantics. A test (`record_json_keys`) pins the 28 JSON keys: optional fields are serialised as `null`, never omitted, so clients can rely on every key being present. `Option<T>` fields are absent in situations where the data is not available (e.g., `rssi` is `None` for downlinks because RSSI is a receive-side measurement).

---

### `core/decoder.rs`

Decodes a LoRaWAN PHY (physical layer) payload into a `DecodedFrame`, and decides whether the frame can be a valid LoRaWAN frame at all.

**Important constraint:** LoRaWAN payloads are encrypted with session keys. The sniffer does not have those keys, so it can only read the unencrypted header. Fields like `FRMPayload` (the actual application data) remain opaque.

```rust
pub fn decode_phy(data_b64: &str, crc: Option<&CrcStatus>) -> Option<DecodedFrame>
pub fn decode_phy_bytes(bytes: &[u8], crc: Option<&CrcStatus>) -> Option<DecodedFrame>
```

`decode_phy` accepts a Base64-encoded string (as it arrives in the UDP JSON) and returns `None` only if the Base64 is invalid or the payload is empty. `decode_phy_bytes` accepts raw bytes (as they arrive from Concentratord Protobuf). `crc` is the gateway's CRC status, already mapped to `CrcStatus` by the backend, so `core/` never sees `stat = -1` or `BAD_CRC`. It is `None` for a downlink and when the status is unknown. The parameter is explicit so that the compiler points at every call site when the signature changes.

**Classification.** Before parsing anything, the decoder applies three rules in order and records the first that holds as `DecodedFrame.malformed`:

| Reason | Rule |
|---|---|
| `Crc` | `crc` is `Some(CrcStatus::Fail)`. `NoCrc`, an unknown status and `None` are not failures, so a source that reports no CRC is not marked |
| `Mhdr` | `bytes[0] & 0x1F != 0`: the RFU bits (4:2) or the Major version (1:0) are not zero |
| `Length` | `length_fits()` says no: JoinRequest 23 bytes; JoinAccept 17 or 33; RejoinRequest 19 or 24; data frames at least 12 + FOptsLen. Proprietary and Unknown have no length rule |

Classification reads only the MHDR and, for a data frame of 6 bytes or more, the FOptsLen bits of FCtrl (byte 5). A shorter data frame has no FCtrl, but it is under the 12-byte minimum whatever FOptsLen would have been.

A malformed frame is returned with only `mtype`, `payload_size` and `malformed`; nothing is parsed from its body. This is deliberate: a fake DevAddr is never produced, rather than produced and cleared later, so storage, WebSocket, REST, MQTT and the live-identifier list are protected with no code of their own, and the record builders keep copying fields. A payload that cannot be decoded at all has no first byte; `proxy::frame_fields` marks it `Crc` if the CRC failed and leaves it unmarked otherwise.

`storage.rs` applies the same length rule in SQL (`CLASSIFY_MALFORMED_SQL`) to rows stored before the classification existed. A test compares it with `length_fits` for every stored combination of type, size, FOptsLen and CRC status, so change both or neither.

**LoRaWAN frame format:**

```
JoinRequest (23 bytes):
  MHDR(1) + JoinEUI(8,LE) + DevEUI(8,LE) + DevNonce(2,LE) + MIC(4)

JoinAccept (17 bytes, or 33 with a CFList):
  MHDR(1) + encrypted body + MIC(4)

RejoinRequest (19 or 24 bytes, depending on the RejoinType; the RJType byte is not checked):
  MHDR(1) + RJType(1) + ... + MIC(4)

Data frame (at least 12 + FOptsLen bytes):
  MHDR(1) + DevAddr(4,LE) + FCtrl(1) + FCnt(2,LE) + FOpts(0-15) + [FPort(1)] + FRMPayload + MIC(4)
```

`decode_join_request` and `decode_data_frame` are safe on a slice of any length: one too short for its type yields no fields.

**Rust note on `..Default::default()`:**

```rust
Some(DecodedFrame {
    mtype,
    payload_size,
    ..Default::default()   // fill the remaining fields with their zero values
})
```

This is struct update syntax. `Default::default()` creates a zero-valued instance of the struct, and `..` copies the remaining fields from it. For `Option<T>` fields, the default is `None`.

**EUI byte order:** LoRaWAN stores EUIs in little-endian order over the air, but displays them big-endian. `Eui64::from_le_slice` (in `core/eui.rs`) reverses the byte order, and its `Display` writes the colon-separated text that goes into the record.

The file has a comprehensive test suite (`#[cfg(test)] mod tests`) with crafted byte arrays covering every MType.

---

### `core/filter.rs`

The filter engine. Deliberately simple: a linear scan through rules, first match wins.

**`FilterRule`** — a compiled filter rule:
- `name: String` — shown in logs and stored as `blocked_by`
- `enabled: bool` — rules can be present in config but disabled without removal
- `direction: Option<Direction>` — `None` matches both directions
- `mtypes: Vec<MType>` — empty list matches any MType
- `gateways: Vec<Eui64>` — empty list matches every gateway
- `suppress_ack: bool` — stealth mode flag

**`FilterDecision`** — the result of evaluating rules against one packet:
- `blocked_by: Option<String>` — `None` means forwarded; `Some(name)` means blocked by the named rule
- `suppress_ack: bool` — whether to suppress the protocol-level acknowledgement

**`evaluate(rules, direction, mtype, gateway) -> FilterDecision`** — iterates the rules slice. The order in the config file is the order they are evaluated. `mtype` is an `Option<&MType>`: `None` for a malformed frame, whose type is only its first byte read as an MHDR. A rule with a non-empty `mtypes` list never matches `None`; a rule with no list still does. Any change to rule evaluation must keep that. `gateway` is an `Option<&Eui64>` in the same spirit: a rule restricted to specific gateways does not match when the gateway is unknown, because blocking on a guess would hit gateways the rule was never meant for, while letting the message through only leaves it unfiltered.

**Why is this in `core/` and not directly in `proxy/`?** The filter logic is protocol-agnostic. Both the UDP proxy and the Concentratord proxy call the same `filter::evaluate()`. Keeping it in a separate module avoids duplication and makes it independently testable.

The test suite covers edge cases: disabled rules, direction mismatch, empty MType list (matches all), multiple MTypes per rule, and the stealth mode flag propagation.

---

### `core/toa.rs`

Computes LoRa **Time-on-Air** (ToA) in milliseconds using the Semtech SX1276 datasheet formula.

ToA is the duration for which the radio channel is occupied by a single packet. It is a function of:
- **Spreading Factor (SF)**: SF7 is fastest (short range), SF12 is slowest (long range). Each step roughly doubles ToA.
- **Bandwidth (BW)**: wider bandwidth = shorter ToA. Common values: 125 kHz, 250 kHz, 500 kHz.
- **Coding Rate (CR)**: forward error correction overhead. 4/5 is the most efficient.
- **Payload size**: more bytes = longer transmission.

The function returns `None` for FSK (non-LoRa) modulations because the formula does not apply.

**Rust numeric note:** The formula mixes integer and floating point arithmetic. Rust requires explicit type casts (`as f64`, `as i32`) — there are no implicit numeric conversions. This is verbose but eliminates entire classes of bugs common in C.

---

### `core/history.rs`

What a history request asks for. It does no I/O and writes no SQL, so the rule for matching a record can be read and tested on its own.

- `HistoryQuery` — an optional lower bound (`LowerBound::After` for the old `since`, exclusive; `LowerBound::From` for `from`, inclusive), an optional upper bound `upper` (the API's `before`, exclusive, so that two windows sharing an edge never share a record), a `HistoryFilter`, a `limit` of at most `MAX_HISTORY_ROWS` (5,000), `newest` (keep the newest `limit` rather than the oldest) and `count`.
- `HistoryFilter` — device text, message types, status, gateway and `hide_malformed`. `has_row_filter()` says whether the request asks for anything beyond the time bound; that is what decides whether it may run when there is no read-only connection.
- `HistoryPage` — the records oldest first, the number of matches, and the number of malformed matches (`None` when it was not computed).
- `fold_case` and `devices_with_matching_label` — the label rule. A label is free text, so SQL cannot compare it; it is folded with `str::to_lowercase`, which maps characters the way JavaScript's `toLowerCase` does (`ÑANDÚ` to `ñandú`, the final sigma, `İ`), and matched in memory. The identifiers of the matching labels go to SQL as one JSON array. A label belongs to the record's identifier, the DevAddr if it has one, else the DevEUI.

The browser applies the same rule to live records in `webui/src/lib/history.ts`. `webui/test/history-vectors.json` lists records, labels and cases, and both sides run it.

---

### `core/labels.rs`

Validation and normalisation for device labels. It does no I/O, so the API can reject bad input before anything touches the database.

- `parse_device_id(raw)` turns what a person typed into the exact text records carry: 8 hex digits are a DevAddr and come back upper-case (`00e8a80b` becomes `00E8A80B`); 16 hex digits, contiguous or separated by `:` or `-`, are a DevEUI and come back colon-separated and upper-case (it reuses `Eui64`). Anything else is `LabelError::BadDevice`. That text is the key of the `device_labels` table and of the WebUI's label map, so a label saved as `00e8a80b` still matches the rows that show `00E8A80B`. The two forms cannot collide (8 against 16 digits).
- `normalize_label(raw)` trims the text. Empty means "remove the label" and is returned as `Ok(None)`, so "save an empty label" and "delete" are the same thing to the caller. Control characters are refused, not stripped (a label is shown on one line and written into a CSV, and altering what the operator typed would be more surprising than saying no), and the length limit `MAX_LABEL_CHARS` (64) counts characters, not bytes.
- `MAX_LABELS` (10,000) bounds the table, and with it the work the retention sweep does while holding the storage lock.

The error messages are written for the person using the WebUI: `LabelDialog` shows them as they are.

---

### `core/seen.rs`

`SeenDevices`: a bounded, best-effort record of the device identifiers seen in live traffic, with the time each was last seen. It exists because the storage writer may drop records when its queue is full, so a device on screen can have no row in the database; an identifier that was seen live can still be labelled.

- `note(id)` adds or refreshes an identifier. At the cap (`MAX_SEEN`, 100,000) a new one is ignored with a rate-limited warning, while identifiers already present keep being refreshed, so a flood of random addresses cannot grow it without bound.
- `seen_within(id, window)` answers "was this seen recently?"; `expire(window)` forgets the old ones (the retention task calls it).
- It is filled by `run_seen_listener` in `cmd/root.rs`, a task that subscribes to the record broadcast, never from the forwarding path. Like any broadcast receiver it can lag and miss records; the device is noted again with its next one.

Internally it is an `Arc<Mutex<HashMap<String, Instant>>>`, so clones share one list. Every critical section is a few map operations and nothing is logged or awaited while the lock is held. A poisoned lock is ignored (`unwrap_or_else(PoisonError::into_inner)`) because no operation can leave the map half-updated, and refusing to answer after an unrelated panic would only make labelling fail for no reason. The `_at` variants of each method take the clock as a parameter so tests cover the time rules without sleeping.

---

### `core/storage.rs`

The SQLite persistence layer.

**The central design:** `Storage` wraps `rusqlite::Connection`s inside `Arc<Mutex<Connection>>`: the writer, and in WAL mode a second, read-only one:

```rust
pub struct Storage {
    reader: Option<Arc<Mutex<Connection>>>,   // history reads; declared first
    conn: Arc<Mutex<Connection>>,             // the writer: inserts, labels, retention
    filters_on_writer: bool,                  // true only for open_in_memory()
}
```

The reader is declared first because fields drop in declaration order and it must close first: the last connection to close checkpoints the log and deletes the `-wal` and `-shm` files, and a read-only connection cannot do that.

Why this structure?

- `rusqlite::Connection` is `Send` (can be moved between threads) but not `Sync` (cannot be shared by reference simultaneously). Raw sharing would be a compile error.
- `Mutex<Connection>` adds synchronisation: only one thread can use the connection at a time. `Mutex<T>` is `Sync` when `T: Send`.
- `Arc<Mutex<Connection>>` adds shared ownership: multiple clones can exist, all pointing to the same connection. Cloning is O(1) — it increments an atomic counter.

This means `Storage` can be `.clone()`d freely and passed into different tasks, without copying the database connection.

**SQLite pragmas set at startup:**

```sql
PRAGMA journal_mode = WAL;    -- Write-Ahead Logging: reads don't block writes
PRAGMA synchronous  = NORMAL; -- Good durability/performance balance
PRAGMA foreign_keys = ON;     -- Enforce referential integrity
```

`open` reads `journal_mode` back after setting it, and the writer sets `journal_size_limit` to 8 MiB so that the `-wal` file shrinks again after a long run of readers.

**Schema:** The `messages` table has one row per captured packet. Enum values are stored as text strings (e.g., `"up"`, `"down"`, `"JoinRequest"`) for readability. Timestamps are RFC 3339 text, which is lexicographically sortable and understood by SQLite's date functions. Booleans are `INTEGER` 0/1.

**Schema versions.** The database carries its version in SQLite's `PRAGMA user_version`. `INIT_SQL` creates the original (version 0) schema, and each entry of `MIGRATIONS` moves it up one version, so a new database and one written by an older release take the same path. Today: 1 adds the gateway, join and frame-control columns; 2 adds the `device_labels` table; 3 adds the `malformed` and `classified` columns; 4 adds the `session_secret` table. Each migration runs in its own transaction together with its version bump. **Never edit or reorder an entry that has shipped; append a new one**, because a database already past that version would skip an inserted entry and the entry after the gap would then run a second time. Old releases ignore tables and columns they do not know, so a database can go back to an older version.

Indexes: `idx_messages_timestamp`, partial indexes on `dev_addr` and on `dev_eui` (only rows that have one), and `idx_messages_mtype`. A fifth, the partial index `messages_unclassified`, is created after the first successful malformed-frame pass (below).

**Malformed frames in stored rows.** The decoder marks malformed frames as they arrive, so a record stored by this version already says what it is. Rows stored before that, or by an older version run on the same database after a rollback, have `classified = 0`. On every start `Storage::open` finds them, marks the ones that fail the CRC or length rule from their stored fields (`CLASSIFY_MALFORMED_SQL`), clears the identifiers that were read from their bytes, and sets the flag, in one transaction. The first pass visits every row, which on a large database takes a while and happens before forwarding starts (the README has figures); afterwards a start counts the unchecked rows through the partial index and returns at once. A pass that fails (a full disk, say) rolls back whole, is logged, and is repeated at the next start; it never stops the sniffer from opening the database.

**Device labels.** `device_labels(device, label, updated_at)` is keyed by the identifier text that records carry. `set_label` runs in a `BEGIN IMMEDIATE` transaction: it refuses a device that has no stored message and was not seen live (`LabelWrite::UnknownDevice`) and a new label past the limit (`LimitReached`), and otherwise upserts. `delete_older_than(cutoff, OrphanPolicy)` is the retention sweep: in one transaction it deletes expired messages and then the labels whose device has no stored message left and that the policy's predicate does not keep (`OrphanPolicy::KeepIf`, which the label writer answers from the live list). It returns `Pruned { messages, labels }`, so the label writer can publish one removal event per deleted label.

**History reads.** `query_history(&HistoryQuery, cancel)` answers a history request, on the reader when there is one:

- The reader is opened only if `PRAGMA journal_mode` reads back `wal`, read-only and with a 1 s busy timeout. In WAL mode a reader sees a consistent snapshot and neither blocks the writer nor is blocked by it. Without WAL a reader would block every commit and a scan on the writer would block every insert, so there is no reader: a time-only query still runs on the writer, a query with a row filter returns `HistoryUnavailable`, and a WARN at startup says so. `open_in_memory()` (tests only) runs everything on its one connection.
- One deferred read transaction covers the labels, the counts and the rows, so they come from one snapshot. A progress handler reads the `cancel` flag every 10,000 VM steps and interrupts the statement; a guard removes the handler on every exit path, so a cancelled query cannot affect the next one on the connection.
- Every range query names `INDEXED BY idx_messages_timestamp`. A device text cannot use an index, but the scan follows the timestamp index and reads only `rowid` and the filter columns: it counts every match and keeps the first N rowids, then reads those N rows in full. Without the hint, the planner chose `idx_messages_mtype` plus a temporary B-tree sort for a type filter.
- Timestamps are stored as `to_rfc3339()` text (`+00:00` and 0, 3, 6 or 9 fraction digits) and sort as text. Bounds are rendered with the same function. The module's doc comment explains why that orders correctly.
- All `EXPLAIN QUERY PLAN` shapes are checked by tests in `core/storage/history_tests.rs`, which run each case on an in-memory and on a file database. `large_db_history_queries` (`#[ignore]`) builds a database of 1,000,000 records and measures the queries, the writer's lock wait and the log size.

**Blocking calls in async context:** All `Storage` methods (`insert`, `query_history`, etc.) are synchronous — they block the calling thread. The callers (`cmd/root.rs`, `api/labels.rs`, `api/messages.rs`) wrap every call in `tokio::task::spawn_blocking` to avoid blocking the async event loop.

**The session secret.** Migration 4 creates `session_secret`, a table with at most one row (`id` is `CHECK (id = 1)`, `secret` a 32-byte blob). `Storage::session_secret(fresh)` first reads the row with no transaction and no write, which is what every start after the first does and works on a read-only file. Only if there is no row does it `BEGIN IMMEDIATE`, read again (another process may have created it while this one waited for the lock), call `fresh` once and `INSERT`; a lock held past the busy timeout (5 s) is an error, never a secret of its own that the other process does not share. A row that is not 32 bytes is `SecretError::Damaged`, reported and never replaced, because replacing it would log every browser out. No `SecretError` value holds secret bytes. Older versions ignore the table, so their databases stay compatible, but they do not enforce the login.

**`INSERT OR REPLACE`:** If a record with the same UUID is inserted twice (e.g., due to a retry), the second insert replaces the first without error. In practice UUIDs are unique, but the `OR REPLACE` makes the operation idempotent.

**Tests.** The unit tests sit in a `tests` module at the bottom of `storage.rs`; three bigger groups live in files next to it, declared there with `#[cfg(test)] mod history_tests;` and so on. (In Rust, a module `storage` declared in `storage.rs` finds its children in the folder `storage/`.) `storage/history_tests.rs` tests `query_history`: what it returns, which connection answers, how it stops, and the query plans. `storage/secret_tests.rs` tests the session secret and the migration behind it. `storage/history_large_tests.rs` is the ignored million-row measurement (see "Running tests" in section 12). A few `#[cfg(test)]` helpers on `Storage` (`hold_writer`, `insert_many`, `probe`) let tests of other modules stall the writer or count how many history queries overlap.

---

### `core/writer.rs`

`WriterItem`, what the storage queue carries. The queue is a tokio `mpsc` channel, and its items are either a `Record(MessageRecord)` to insert or a `Settle { done, permit }` marker. A history request that must not miss any record queues a marker behind them; the writer takes items strictly in order and finishes each before taking the next, so when it answers a marker, everything queued before it has been *processed* (saved, or its failure logged and counted). The module comment spells out why "processed" is not "saved".

**Rust note:** a marker carries two pieces of tokio machinery. `done` is a `oneshot::Sender<()>`: a channel that carries exactly one value, here "reached it", back to the request that is waiting. `permit` is an `OwnedSemaphorePermit`, the single permit of the semaphore that admits markers (`history_settle` in `AppState`); because it travels inside the marker and is dropped when the writer is done with it, at most one marker is ever queued, even if the request that made it gave up waiting. The enum has `#[allow(clippy::large_enum_variant)]` on purpose: boxing the record would add an allocation to every record on the forwarding path. The `record_channel` helper under `#[cfg(test)]` gives tests a queue that hands out plain records.

---

### `core/random.rs`

Random bytes that can fail. The login needs random bytes for its token, its stored secret and every session, and reading them can fail (a stripped-down container with no entropy source). When it does, the sniffer must report an error and never panic, fall back to something weaker, or carry on with a short value. `ring`'s own `SecureRandom` trait is sealed, so this small trait wraps it and lets tests put a failing source in its place:

- `RandomSource` (trait): `fill(&self, out: &mut [u8]) -> Result<(), RandomError>`. `SystemRandomSource` is the production implementation, over `ring::rand::SystemRandom`.
- `random_bytes::<N>(source)` returns `[u8; N]`; `hex` and `generate_token` (16 bytes as 32 lower-case hex characters, the shape `openssl rand -hex 16` gives) build on it.

**Rust note:** `random_bytes::<32>(..)` uses a *const generic*: the array length is part of the type, so the compiler knows the size and no allocation is needed. `&dyn RandomSource` and `Arc<dyn RandomSource>` are *trait objects*: the caller picks the implementation at run time, which is how `FailingRandom` (test-only, in `random::testing`) replaces the real source.

---

### `core/ratelimit.rs`

`RateLimitedLog`: lets at most one log line through per interval (10 s by default) and says how many it held back (`... (3 suppressed since the last message)`). A gateway can send hundreds of packets per second, so a warning per dropped packet (full queue, unreachable broker, refused session) would bury the log and cost more CPU than the work being dropped. One instance limits one kind of message; give each distinct condition its own, otherwise a noisy one hides a rare one. You will see them as fields (`DropLogs` in the UDP proxy, `MqttStats`, `Auth`) and as `static`s.

All methods take `&self` (a `Mutex<Option<Instant>>` and atomics inside), so an instance can sit in an `Arc` or a `static` shared by tokio tasks and plain threads, and `new()` is a `const fn` for that reason. `warn` and `error` take `impl Display`, so pass `format_args!(..)`: the message is then built only if it is actually logged. `permit(now)` takes the time as a parameter so tests cover the interval logic without sleeping.

---

### `core/linelog.rs`

Log lines that a test can read back. A process has one global logger, installed by whichever test gets there first, and the tests run in parallel, so a test cannot rely on it to have kept the lines its own code logged. The lines a test needs to see are logged through the `warn_line!` and `error_line!` macros, which behave like `warn!` and `error!` and, when a test has called `start_capture`, also keep the line in a buffer private to the test's thread (`take_capture` reads it). `RateLimitedLog` logs through the same function, so its output is capturable too. In a normal build the capture code does not exist.

**Rust note:** `macro_rules!` defines the two macros; they exist so that `module_path!()` is expanded at the call site and the log line still names the module that logged it. The per-thread buffer is a `thread_local!`. A `#[tokio::test]` runs everything it spawns on the test's own thread, which is why a thread-local buffer sees the tasks under test.

---

## 8. File-by-file reference — `lorawan-sniffer`

---

### `lorawan-sniffer/src/main.rs`

The binary entry point. Its responsibilities are minimal:

1. **Declare the modules** — `api`, `cmd`, `config`, `core`, `mqtt`, `proxy`, and `test_support` in test builds only.
2. **Parse the CLI** using `clap` — the `#[derive(Parser)]` attribute generates argument parsing from the struct definition. `-c FILE` can be repeated; `--version` prints the crate version (the installer uses it to check that a binary runs on the machine).
3. **Load configuration** — calls `config::get()` and exits with a message on error.
4. **Handle the `configfile` subcommand** — prints a template and exits. This comes before validation, so the template can be printed even when the loaded configuration would not pass it.
5. **Validate** — `config::validate()` stops startup with a message for a bad `allowed_hosts`, login or MQTT setting.
6. **Set up logging** — either `simple_logger` (stdout) or `syslog` depending on config, then log the `load_warnings` that loading collected (a `$VAR` that names an unset variable, for example).
7. **Delegate to `cmd::root::run()`** — everything else happens there.

**Rust note on `#[macro_use] extern crate log`:** The `log` crate provides macros (`info!`, `warn!`, `error!`, `debug!`, `trace!`) that work like `printf` but route to whatever logging backend is configured at runtime. The `extern crate` syntax is an older Rust idiom; in modern Rust (edition 2018+) it is usually not needed, but the macros still require explicit import via `#[macro_use]`.

---

### `lorawan-sniffer/src/test_support.rs`

Compiled only for tests (`#[cfg(test)] mod test_support;` in `main.rs`). It holds `capture_logger()`, which installs a process-wide logger that keeps every line at every level, for tests that need to see what other tasks logged: the login's log test and the MQTT tests share it. A process has one global logger, so every test that wants log lines installs this same one and filters what it reads by what it logged. (`core/linelog.rs` is the other mechanism: it captures per thread, for the lines logged through `warn_line!` and `error_line!`.)

---

### `lorawan-sniffer/tests/shutdown.rs`

An integration test: the only one that starts the real compiled binary (Cargo gives its path in `CARGO_BIN_EXE_lorawan-sniffer`). It writes a configuration for UDP listen mode on free loopback ports, waits for the line that says the proxy holds its socket, sends SIGTERM (and, in a second test, SIGINT), and checks that the process exits with status 0, logs `Received SIGTERM, shutting down` and `Sniffer stopped`, and takes under three seconds (nothing is queued, so it must not sit out the writer's drain timeout). `systemctl stop` and `docker stop` both send SIGTERM; as PID 1 in a container the sniffer would otherwise ignore it until Docker killed it.

---

### `lorawan-sniffer/src/config/mod.rs`

All configuration structs and the TOML loader.

The configuration is hierarchical:

```toml
[sniffer]
  log_level = "INFO"
  [sniffer.api]
    bind = "0.0.0.0:8888"
  [sniffer.database]
    path = "/var/lib/lorawan-sniffer/sniffer.db"
    retention = "30d"

[gateway]
  protocol = "udp"
  [gateway.udp.downstream]
    bind = "0.0.0.0:1700"

[[filters]]
  name = "block-joins"
  enabled = true
  mtype = ["JoinRequest"]
```

**Serde defaults:** Every struct implements `Default` and is annotated `#[serde(default)]`. This means missing fields in the TOML file are filled in with default values. Operators can use a minimal config file.

**Structure.** `Configuration` has `sniffer` (`log_level`, `log_to_syslog`, and the `api`, `database` and `mqtt` sections), `gateway` (`protocol` = `udp` or `concentratord`, with a `udp` and a `concentratord` section) and `filters` (a list of `FilterConfig`). The UDP section picks a mode with the `UdpMode` enum: `proxy` relays to the LNS (`upstream.server`/`port`), `listen` only records (`listen_acks` says whether the sniffer answers PUSH_DATA and PULL_DATA itself so gateways think their server is alive). `session_timeout` and `max_gateways` size the per-gateway session table. `MqttConfig` and `Api` have no `Debug` impl on purpose: they hold secrets, and a stray `{:?}` must not be able to print them.

**Environment variables.** Before the TOML is parsed, `expand_env` replaces every `$NAME` with the value of that variable. The value is escaped for a TOML string (backslash, quote and control characters), so a password containing a quote cannot end its string early and smuggle in more settings, and the inserted text is never scanned again. A reference to a variable that is not set stays as written and is reported as a load warning (the keys go to `Configuration.unexpanded_keys`, never the values). A TOML syntax error is reported with its line and column but never quotes the line (`parse_error`), because that line may be a token or a password and the message ends up in the journal.

**Validation.** `validate` runs once after loading, before anything starts. `allowed_hosts` entries must be bare host names (no scheme, port, path or wildcard) or exactly `["*"]`; the login token is checked by `validate_login` when login is on (below); and the MQTT section is checked only when MQTT is enabled: QoS 0 to 2, a topic template that parses, a client certificate and key given together, and every TLS file readable. A leftover typo in a disabled section must not stop the sniffer. Failing at startup with the reason is kinder than a 403 or a reconnect loop later.

**The login settings.** `[sniffer.api] login` (default `false`) and `token` (default empty) are read like any other setting, `$VAR` expansion included. `validate` runs `validate_login` right after the `allowed_hosts` check and before the MQTT early return: with login off the token is not looked at; with login on an unexpanded `$VAR` reference (recorded in `Configuration.unexpanded_keys` when the file is loaded), an empty token or one that fails `token_shape_ok` (at least 32 hexadecimal characters, at least 8 distinct, `A` and `a` counting as one) stops startup with a message that names `[sniffer.api] token` and says how to generate one, and none of them quotes the value. The shape check is a format check: it cannot tell a random token from a typed one, and no text may describe it as proving randomness. `Api` and `Configuration` have no `Debug` on purpose.

**`humantime_serde`:** The `retention` field is a `std::time::Duration` but is written in TOML as a human-readable string like `"30days"`. The `humantime_serde` crate handles the conversion automatically.

**`pub fn get(filenames: Vec<String>) -> Result<Configuration>`:** Reads and concatenates multiple TOML files (allowing split configuration), expands `$ENV_VAR` references, and parses the result. Returns an `anyhow::Result` — the `anyhow` crate is used in the binary (application code) for ergonomic error handling with rich context messages.

**Why `anyhow` here but `thiserror` in `core/error.rs`?**

| Context | Crate | Reason |
|---------|-------|--------|
| `core/` (internal logic) | `thiserror` | Typed errors allow callers to match specific variants |
| `config/`, `cmd/` (application layer) | `anyhow` | Errors are shown to the operator; rich messages matter more than type |

---

### `lorawan-sniffer/src/cmd/mod.rs`

Just re-exports the two submodules: `pub mod configfile` and `pub mod root`. No logic here.

---

### `lorawan-sniffer/src/cmd/configfile.rs`

Implements the `configfile` subcommand. It takes the current `Configuration` (which has all defaults populated) and renders a Handlebars template with those values.

The result is a commented TOML template that an operator can use as a starting point. Running `lorawan-sniffer configfile > sniffer.toml` gives a complete, annotated configuration file.

The template's `token` line is a fresh token: `run` reads 16 bytes from a `RandomSource`, writes them as 32 hex characters (`generate_token`), and renders the template with that as `generated_token`. The token loaded from a file or the environment has no placeholder and is removed from the values before rendering, so a mistaken placeholder fails the strict render and cannot print the loaded secret. The token is made before anything is printed, so a failing random source (`RandomError`) prints nothing and `main.rs` exits with "Cannot generate a login token: …".

---

### `lorawan-sniffer/src/cmd/root.rs`

The application's main orchestration layer. Called by `main.rs` after logging is set up.

**Sequence of operations (`run`):**

1. Install the stop-signal handlers for SIGINT (Ctrl-C) and SIGTERM (`systemctl stop`, `docker stop`) first. Until a handler exists SIGTERM would kill the process outright, and as PID 1 in a container the kernel drops a signal that has no handler.
2. Build `Vec<FilterRule>` from `config.filters` (the `build_filters` helper; a gateway EUI that does not parse stops startup, so a typo cannot leave a rule that silently matches nothing).
3. Open the SQLite database (`Storage::open`). This is also where old schemas are migrated and old rows are checked for malformed frames.
4. If `[sniffer.api] login` is true, read or create the session secret and build the `Auth` (`start_login`). This happens before anything listens: a sniffer that cannot make its key must not come up serving the API without the login the operator asked for.
5. Create the channels: the `mpsc` storage queue (carrying `WriterItem`s), the `broadcast` channel for record fan-out and a second one for label events. Build `Outputs` from the first two.
6. Create a `CancellationToken`.
7. Start the live device tracking (`run_seen_listener`) and the label writer (`LabelWriter::spawn`).
8. If MQTT is enabled, `mqtt::start` and attach the publisher to `Outputs`.
9. `tokio::spawn` the storage writer task (`run_writer`).
10. `tokio::spawn` the API server task (`run_api`), handing it an `ApiContext`.
11. `tokio::spawn` the retention cleanup task (`run_retention`).
12. Run the proxy in the main task. For UDP in proxy mode, `resolve_upstream` first resolves the LNS address (preferring IPv4 when a name has several) and refuses an address that leads back to the sniffer's own socket; in listen mode there is no upstream. For Concentratord, `proxy::concentratord::run`.
13. When the proxy returns or a stop signal arrives, `cancel.cancel()`, then wait for the writer to drain.

**`tokio::select!` for graceful shutdown:**

```rust
tokio::select! {
    result = proxy::udp::run(...) => { result?; }
    name = stop.recv() => {
        info!("Received {}, shutting down", name);
        cancel.cancel();
    }
}
```

This waits for whichever completes first: the proxy exits, or SIGINT/SIGTERM arrives (`stop.recv()` returns the signal's name). Either way, `cancel.cancel()` propagates the shutdown signal to all other tasks. `run` then waits up to `WRITER_DRAIN_TIMEOUT` (5 s) for the writer to save what is still queued, because returning at once would end the runtime and lose the last records; the limit keeps a stuck disk from holding the stop forever, inside the stop timeouts of systemd and Docker. `tests/shutdown.rs` checks this path against the real binary.

**Storage writer (`run_writer`):**

```rust
// process_items, which run_writer calls with the real insert
loop {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            // drain remaining items before exit, the same way
            while let Ok(item) = rx.try_recv() { process_item(item, ...).await; }
            break;
        }
        Some(item) = rx.recv() => process_item(item, ...).await,
    }
}

// process_item
match item {
    WriterItem::Record(record) => {
        // on a blocking thread, awaited before the next item is taken
        match spawn_blocking(move || insert(record)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => { /* ERROR "Storage write error: …", failures += 1 */ }
            Err(e)     => { /* ERROR "Storage writer task panicked: …", failures += 1 */ }
        }
    }
    WriterItem::Settle { done, permit } => {
        let _ = done.send(());   // everything queued before this has been processed
        drop(permit);
    }
}
```

The stop is looked at first (`biased`), so a queue that never empties cannot keep the writer from draining and exiting. On cancellation, the writer drains any items still in the channel before stopping, answering markers and counting failures as it goes. This ensures no records are lost on clean shutdown. `process_items` takes the insert as a parameter so that tests can make it fail, panic or wait.

The order matters. Each insert is awaited before the next item, and `write_failures` is incremented before the next item is taken, so a `Settle` answer always follows the failures of the records queued before it. The API reads the counter only after the marker's answer; that handoff orders the two, so `Relaxed` atomics are enough.

**Retention task (`run_retention`):**

Uses `tokio::time::interval`, ticking every half of the retention period clamped between 1 second and 5 minutes (`sweep_interval`; for any retention of ten minutes or more that is the five minutes the sweep has always used, and a short retention in a test is pruned promptly). `MissedTickBehavior::Skip` means if the system is slow and a tick is missed, it is simply skipped — no burst of back-to-back runs.

Each tick (`retention_tick`) forgets old entries of the live device list and asks the label writer to `prune`: delete messages older than the retention period and the labels of devices that have no stored message and were not seen live. The sweep never deletes a label during the first retention period after a start (`keep_orphans`): the live list starts empty at every start, so a label whose device has simply not been heard yet would look abandoned. It logs what it removed at INFO and nothing when there was nothing to remove.

**Live device tracking (`run_seen_listener`):**

Subscribes to the same `broadcast` channel as the WebSockets (the first receiver, created with the channel) and notes the `dev_addr` or `dev_eui` of each record envelope in `SeenDevices`. It reads only those two fields of the JSON and skips the rest. A broadcast receiver never makes the sender wait, it only falls behind: on `Lagged` the missed records are gone, a rate-limited warning says so, and the listener carries on.

---

### `lorawan-sniffer/src/api/mod.rs`

The HTTP/WebSocket server, built on the `axum` web framework.

**Routes:**

| Route | Handler | Description |
|-------|---------|-------------|
| `GET /ws` | `ws_handler` | WebSocket upgrade for the live feed |
| `GET /ws/labels` | `labels_ws_handler` | WebSocket feed of the device labels (see the README) |
| `GET /api/messages` | `messages::messages_handler` | REST message history |
| `GET /api/labels`, `PUT`/`DELETE /api/labels/{device}` | label handlers | Device labels (see the README) |
| `GET /api/session` | `auth::session_open` / `auth::session_closed` | `{"login": true}` or `{"login": false}`; always registered |
| `POST /api/login`, `POST /api/logout` | `auth::login`, `auth::logout` | Start and end a session; registered only when the login is on |
| `GET /*` | `frontend_handler` | Serve the embedded React SPA |

**Layer order.** `build_router` adds layers from the inside out, so the last one added is the first to see a request: the routes, then (only with `login = true`) `require_login`, then CORS (only with `cors_origins`), then the Host check. A foreign `Host` is refused before login is considered, a CORS preflight needs no credential, and the login's `401` and `429` come from outside every route. The README's "Host check" section explains why that check is outermost.

**The login (`api/auth.rs`, described below).** `ApiContext.login` is `Option<Arc<Auth>>`: `cmd/root.rs` builds it (`start_login`) only when `[sniffer.api] login` is true, and the router adds the login routes and the layer only when it is `Some`. With it `None` the router is the one that existed before the login, plus `/api/session`.

**`AppState`:** axum passes shared state to every handler via the `State` extractor. Besides the broadcast sender (to subscribe new WebSocket clients) and the `Storage` handle (to query history), it holds what history requests need: a weak sender for the storage queue (weak, so the API never keeps the queue open after the proxy has stopped), three `Semaphore`s, and the failure counter the writer increments.

| Field | What it limits or holds |
|---|---|
| `history` (1 permit) | One query at a time; the permit moves into the `spawn_blocking` closure, so it is released when SQLite has stopped, not when the handler is dropped |
| `history_bodies` (3 permits) | Large responses being sent at once. The permit moves into the response body (`HeldBody`), which hands hyper the JSON in 64 KiB pieces copied out of its buffer; hyper asks for the next piece only when its write buffer has room, so the permit is held until the client has taken nearly all of it. Handed over in one piece, hyper would buffer it all and drop the body, and the permit, before a byte was sent |
| `history_settle` (1 permit) | At most one `Settle` marker in the storage queue; the permit travels inside the marker and is released when the writer consumes it. A `RateLimitedLog` (`settle_log`) limits the warning for a writer that does not answer in time |
| `write_failures` | `Arc<AtomicU64>` shared with the writer; read for `X-Storage-Write-Failures` |

**WebSocket handler:**

Each WebSocket connection gets its own `broadcast::Receiver`, taken in `ws_handler` **before** the handshake is answered (`ws.on_upgrade` comes after `subscribe()`), so every record broadcast after the browser's `open` event reaches the socket. The handler loops in a `tokio::select!`, waiting for either a new message on the broadcast channel or a close frame from the client.

A client that falls behind is closed, not left to miss records: when the receiver reports `Lagged` (more than 512 records buffered; WARN `WebSocket client lagged <n> messages; closing it`), or when a frame send takes more than 10 s (a peer that stops reading; WARN `WebSocket client did not accept a message within 10s; closing it`), the handler tries to send a close frame with code 1013 ("try again later", reason `fell behind; reconnect and reload`) for at most 1 s and then drops the socket. A client that reconnects is expected to reload what it shows from `/api/messages`.

The JSON format sent to clients:
```json
{ "type": "message", "data": { ...MessageRecord fields... } }
```

**REST handlers.** `GET /api/messages` is `api/messages.rs` (below). The `/api/labels` handlers are in this file, described under "Label endpoints". `/api/session` is `auth::session_open` or `auth::session_closed`, chosen when the router is built.

**Label feed (`/ws/labels`):**

`labels_ws_handler` takes its subscription to the label channel **before** the full set is read, so a change saved after the set was read always arrives as an event (one saved just before may arrive twice, which is harmless because an event carries the whole value for its identifier). The first frame is the full set, `{"type":"labels","data":{...}}`; after that one `{"type":"label","data":{"device":...,"label":...}}` per change, with `null` for a removal. A client that falls behind is not closed, unlike a `/ws` client: its subscription is replaced (`resubscribe`) and it gets a fresh full set, which already contains everything the dropped events said. `/ws` carries no label messages.

**Label endpoints:**

`list_labels`, `put_label` and `delete_label` validate with `core::labels` and never touch the database themselves: they send a command to the `LabelWriter` (`api/labels.rs`) and wait for its answer. `put_label` checks in a fixed order, so the answer to a request that is wrong in several ways does not depend on how it was assembled: `Origin` (403), identifier (400), body (415 if it is not declared JSON, else 400), label (400). An empty label is a removal and needs no known device, so removing always works. `LabelWrite::UnknownDevice` becomes 404 and `LimitReached` 409. Bodies over 4 KiB are refused (`MAX_LABEL_BODY_BYTES`, a body-limit layer on the route), which keeps the default 2 MB read off an endpoint anyone on the network can reach. `origin_allowed` is the `Origin` rule shared with the WebSocket handshakes: browsers do not apply CORS to those, and a form can send a `PUT`, so the header is checked against the request's own host and `cors_origins`. Every error is `{"error": "<text for the person using the WebUI>"}` (`json_error`).

**Embedded frontend (`rust-embed`):**

```rust
#[derive(RustEmbed)]
#[folder = "../webui/dist"]
struct Frontend;
```

The `RustEmbed` derive macro reads all files in `webui/dist/` at **compile time** and includes them as byte arrays in the binary. At runtime, `Frontend::get("index.html")` retrieves the file from memory. No file system access at runtime — the binary is self-contained. (That is how a release build works. `rust-embed` embeds the files only in release builds; a debug build reads them from disk when it runs. Either way the folder has to exist when the crate is compiled, which is why the Dockerfile creates a placeholder `index.html` for its dependency-caching layer.)

The `frontend_handler` implements SPA routing: if the requested path is not found as a static file, it falls back to `index.html`. This allows the React router to handle client-side navigation.

**CORS:** If `cors_origins` is empty the API sends no CORS headers, so only the sniffer's own pages can use it from a browser. Listed origins get CORS for `GET`, `PUT`, `DELETE` and `Content-Type`, and the four history headers are exposed to them (`expose_headers`), since a script cannot read a header that is not exposed.

---

### `lorawan-sniffer/src/api/auth.rs`

The optional WebUI login: the shared token, the session cookie, the lockout on wrong tokens, and the HTTP layer that asks for a credential. `api/mod.rs` wires it in (see "The login" there); this file holds the parts:

- **`Auth`** holds the session key (`session_key(secret, token)` = `HMAC(secret, label ‖ 0x00 ‖ token)` used as an HMAC key), a `Comparer` for tokens, the `RandomSource`, two `RateLimitedLog`s (failed logins; a login whose `Origin` and `Host` differ) and the `Lockout`. It has no `Debug`. `Auth::new` draws a 32-byte per-process key from the random source and keeps the token only as its HMAC under that key; `hmac::verify` is the comparison, so it takes the same time wherever the inputs first differ. `Auth::with_parts` (tests only) swaps the clock and the comparison.
- **A session** is `v1.<unix seconds>.<16 random bytes in hex>.<HMAC-SHA256 in hex>` and nothing is stored per session. `session_valid` parses the public shape first (a wrong shape reveals only that it is wrong), then `hmac::verify`. The cookie is `lorawan_sniffer_session; HttpOnly; SameSite=Strict; Path=/`, with no `Max-Age`, `Expires` or `Secure`; logout sends the same cookie with `Max-Age=0`.
- **`Lockout::attempt(compare)`** is the only way a token is compared (`Auth::check_token` wraps it). One `std::sync::Mutex<Window { start, wrong, warned }>` is held across reading the clock, rolling the window over, checking `wrong >= LIMIT` (10), calling `compare`, and counting a wrong one. A single lock is the point: with check and count in separate steps, many requests that arrive together would all pass the check before any of them added to the count. The window is 60 s and fixed, so up to 20 wrong tokens can be compared across a boundary. `Locked(left)` is answered without comparing, a right token is never counted, a panicking `compare` leaves the count unchanged (the poisoned lock is recovered), and nothing in it `.await`s. The clock is `Clock`/`TokioClock`, so tests with a paused Tokio clock move the window.
- **`credential(headers)`** returns `Bearer`, `Cookie` or `None`: a `Bearer` authorization header wins (and an unusable one, with no token, spaces in it or repeated, is `None` and does not fall back to a cookie); an `Authorization` of any other scheme is ignored, so a proxy's `Basic` does not hide the cookie; else the session cookie.
- **`require_login`** lets `is_public(path)` paths through (anything outside `/ws`, `/ws/…`, `/api/…`, plus `/api/login` and `/api/logout`), refuses no credential and an invalid cookie with a plain `401` (the cookie failure is logged, not counted), and sends a Bearer token through `check_token`: `Right` passes, `Wrong` is `401`, `Locked` is `429` with `Retry-After`. The prefix test is on the path as the router sees it; odd spellings either match and are refused, or reach no data route.
- **`same_host_json`** runs first in `login` and `logout`: exactly one `Content-Type` that `mime` parses as `application/json` with no `+suffix` and well-formed parameters (else `415`), then `origin_allowed(headers, uri, &[])`, an `Origin` with the request's host and port (else `403`; `cors_origins` are not consulted and `X-Forwarded-*` is never read). Values from the client reach the log only through `log_value` (64 characters, then `…`; unprintable bytes shown as `?`).
- **Startup** (`start_login` in `cmd/root.rs`): `Storage::session_secret(|| random_bytes::<32>(..))` reads the stored secret with no write, or creates it in a `BEGIN IMMEDIATE` transaction; then `Auth::new`. Each failure (random source, database, damaged row) is an error that stops startup.

---

### `lorawan-sniffer/src/api/messages.rs`

`GET /api/messages`, the history of captured messages. The README lists every parameter, header and status; the same table is in the doc comment at the top of `api/mod.rs`. This file holds what makes the endpoint safe to call from many tabs at once against a large database: one query at a time, three large answers in memory at most, and an optional marker that makes a reload wait for the storage writer.

**The handler.** In outline, `messages_handler` does this:

1. `parse` turns the query string into a `HistoryQuery`. `from` is inclusive and `before` exclusive, so adjacent windows never share a record (the WebUI's day selector sends one local midnight and the next); `before` must be later than `from` or `since`, and `from` together with `since` is refused. `since` and `limit` keep their old meaning; a bad new parameter is a `400` with `{"error": ...}`. `newest` defaults to `true` unless a lower bound was parsed.
2. For `settle=true` only: it takes `history_settle`, sends a `Settle` marker and waits for the answer, all under one 5 s timeout. A timeout is not an error: the query runs anyway and the answer says `X-History-Settled: false`, with a rate-limited WARN (`Storage writer is behind; history served without waiting for it`, or `Storage writer stopped; …` when the queue is closed). A request without `settle` skips this.
3. It takes a `history_bodies` permit, then the `history` permit, and runs `Storage::query_history` in `spawn_blocking`, with a `CancelOnDrop` flag. If the client disconnects, axum drops the handler, the flag is set, and the scan stops at its next progress tick (DEBUG: `History query cancelled: the client went away`).
4. It answers with the records and the count headers. `HistoryUnavailable` and `Busy` become `503` with `code` `no_wal` and `busy`; anything else is a `500` with no body.

The marker wait comes before the two permits so that requests that settle wait for the writer together, within the same few seconds. If a request took a body place first, only three could wait for the writer at a time, and the fourth to tenth would wait for a place and then for their own marker, in waves of up to 5 s.

A response layer on the route adds `X-Storage-Write-Failures` to every response, including axum's own `Query` rejections, which are produced before the handler runs.

---

### `lorawan-sniffer/src/api/host.rs`

The Host check, the outermost layer of the router. `check_host` is a middleware that reads the `Host` header of every request (for HTTP/2, which has none, the authority of the request URI), WebSockets and the WebUI bundle included, and answers `403` as plain text unless `host_allowed` says yes. The text names the host, the setting to change (`[sniffer.api] allowed_hosts`, or `SNIFFER_API_ALLOWED_HOSTS` in Docker) and carries `X-Content-Type-Options: nosniff` so a browser never renders the echoed name as HTML. A request with two `Host` headers, or none, is a `400`. Refusals are logged at DEBUG only: the sender decides how many there are, and a warning per request would let any page on the network fill the log.

Allowed: any IPv4 or IPv6 address, with or without a port (`AllowedHosts` and `parse_host` handle the `[v6]:port` form), `localhost`, and the names listed in `allowed_hosts`, compared case-insensitively and without a trailing dot; `["*"]` turns the check off. The reason is **DNS rebinding**: a page on `evil.example` makes that name resolve to the sniffer's address, the browser then treats the sniffer as the page's own origin, and `Origin` checks and CORS stop protecting it. What the attacker cannot change is the name, so the request still says `Host: evil.example`. An IP literal cannot be rebound, which is why only names need listing. `AllowedHosts::new` is built once from the configuration; `config::is_host_name_byte` is the character rule shared with the startup validation, so the check only ever suggests a name that validation accepts. The README's "Host check" section covers it from the operator's side.

---

### `lorawan-sniffer/src/api/labels.rs`

The label writer: the one place where device labels change. `LabelWriter` is a cloneable handle to a single task; every change, whether it comes from `PUT`/`DELETE /api/labels/{device}` or from the retention sweep, is a `Command` (`Set`, `Delete`, `Prune`) sent to that task over a bounded `mpsc` channel (64), each with a `oneshot` for its answer. The task takes one command at a time, saves it through `Storage` on a blocking thread, publishes the matching event on the label channel, and only then answers. Two properties follow:

- **Publish order equals commit order.** If handlers saved and published on their own, two requests could commit in one order and publish in the other, and a WebUI that applies events as they arrive would end up showing a label the database no longer holds.
- **A cancelled request cannot lose its event.** A browser that closes the page drops the handler's future at its next `.await`; if the handler did the saving, that could land between the commit and the publish. Here the handler only queues a command and waits; once queued, the change is saved and published whether or not anyone still waits.

`Set` asks `SeenDevices` whether the device was seen live within the retention period (the answer `Storage::set_label` needs); `Prune` is the retention sweep and publishes one removal event per label it deleted. Only the identifier goes into the log (`label set for 00E8A80B`), never the label text, which is operator data. A command that was never taken from the queue (the sniffer is stopping) is dropped together with its reply channel, and the requester sees `WriterError::Stopped`. Tests use `LabelWriter::spawn_for_test`, which sets a lower label limit and an optional `Gate` that holds each command between taking it and saving it, so a test can drop a request at exactly that point.

---

### API tests

Four test files sit next to `api/mod.rs`. `api/mod.rs` declares three of them with `#[cfg(test)] mod …;` (and has a small in-file `tests` module of its own); `auth.rs` pulls in `auth_tests.rs` itself with `#[path = "auth_tests.rs"] mod tests;`, so those tests can reach the private items of `auth.rs`. The in-process router cannot upgrade a connection, so anything that needs a WebSocket runs against a real listener on loopback with a small hand-written client.

- `auth_tests.rs` — the login core: sessions, the token comparison, the lockout (with a manual clock and on real threads), `credential`, the parsing helpers.
- `login_tests.rs` — the login through the built router (`Setup::login(token)`): what each endpoint answers, which paths need a credential, the order the layers answer in.
- `message_tests.rs` — `/api/messages` through the router and what stands behind it: the permits, the marker, the failure count on every answer, the `/ws` feed a reload depends on, and the shared filter vectors from `webui/test/history-vectors.json` (`include_str!`).
- `label_tests.rs` — the label endpoints, the Host check as the router applies it, the `/ws/labels` feed and the writer, including cancellation.

---

### `lorawan-sniffer/src/proxy/mod.rs`

Shared utilities used by both proxy backends. These are declared `pub(super)` — visible to sibling modules (`udp/`, `concentratord.rs`) but not outside the `proxy` module.

**`frame_fields`:** Converts an `Option<DecodedFrame>` and the CRC status into a flat `FrameFields`, filling fields with `None` / `MType::Unknown` if decoding failed. `FrameFields::rule_mtype()` is the type the filter sees: `None` for a malformed frame.

**`classify`:** Decides forward / block / capture for a message from the `listen` flag, the filters, the direction, the type (`None` for a malformed frame) and the gateway. A listening proxy never consults the filters: everything is captured. The returned `Verdict` (`Forward`, `Block { rule, suppress_ack }` or `Capture`) gives the `Status` and the name of the rule that blocked it, for `MessageRecord`.

**`Outputs` and `Outputs::emit`:** `Outputs::new(storage_queue, live_broadcast)` is the set of destinations (`with_mqtt` adds MQTT), and clones share the queues, so ZMQ threads and tokio tasks can each hold one. `emit` is the one place a record leaves the proxy. It logs the record, then does only non-blocking hand-offs:

1. queues the record for MQTT with `try_send`, if MQTT is on;
2. queues a `WriterItem::Record` for storage with `try_send`;
3. serialises the `/ws` JSON, but only if a WebSocket client is subscribed (`receiver_count() > 0`), and broadcasts it. The check comes after the storage queue on purpose: a subscriber that arrives between the two would otherwise get neither the broadcast nor a place ahead of its marker.

If a queue is full (which should never happen at LoRaWAN data rates) the record is dropped for that output and a rate-limited warning is logged. Using `try_send` instead of `.await send` means the proxy loop never stalls waiting for the storage writer. Storage is queued before the broadcast so that any record a browser could have seen live is already ahead of a later `Settle` marker; see "Reading history" in section 5.

**`log_message`:** Formats a compact one-line log entry, at DEBUG level only (one line per packet would fill the journal on a server in front of many gateways; a malformed record ends with `malformed=<reason>`):
```
[FORWARDED] UP   UnconfirmedDataUp 01ABCDEF gw=AA:BB:CC:DD:00:11:22:33 fcnt=1234 fport=1  868.100MHz SF7BW125 size=23 toa=62ms rssi=-55 snr=8.0
```

---

### `lorawan-sniffer/src/proxy/udp/mod.rs`

The Semtech UDP Packet Forwarder MITM proxy: the main loop and the two per-direction handlers. This is the most complex part of the binary, so it is a folder: `mod.rs` here, and `packet.rs`, `session.rs`, `loopguard.rs` and `tests.rs` below, each with one job.

**Protocol overview:** The Semtech protocol uses a 4-byte header on every UDP datagram:
```
[protocol_version(1)] [token(2)] [message_type(1)] [... body ...]
```

The 2-byte token is a random number chosen by the sender. The receiver echoes it back in the acknowledgement so the sender can match responses to requests.

**One gateway-facing socket, one session per gateway.** A single UDP socket bound to `gateway.udp.downstream.bind` (default `0.0.0.0:1700`) serves any number of gateways. A gateway is identified by the EUI that PUSH_DATA, PULL_DATA and TX_ACK carry right after the header (`packet::gateway_eui`), not by its source address, so a forwarder that restarts, or whose NAT mapping changes, simply moves its session. The `SessionTable` (`session.rs`) keeps one `GatewaySession` per EUI: where to answer the gateway (a packet forwarder sends PUSH_DATA from one socket and PULL_DATA/TX_ACK from another, so a session holds two addresses) and, in proxy mode, a **socket of its own connected to the LNS**. The LNS therefore sees one source port per gateway, and a datagram that comes back on a socket belongs to the gateway that owns it: nothing is looked up by token and nothing is rewritten, which matters for PULL_RESP, whose token is chosen by the server and cannot be matched to an earlier request. Datagrams are forwarded as the exact bytes received. The naming convention follows standard networking: *downstream* is towards the gateway (the client), *upstream* is towards the LNS (the server).

**The main loop** (`run_with_socket`; `run` only binds the socket first, so tests can bind port 0 and learn the port) is a `tokio::select!` over four things: a datagram from a gateway (`handle_from_gateway`), a datagram from any session's LNS socket (a reader task per session forwards them, tagged with the gateway's EUI, over an `mpsc` channel to the loop; `handle_from_lns`), a sweep timer that releases gateways silent for longer than `session_timeout` (closing their socket and reader task), and shutdown. The loop owns the session table alone, so it needs no locks. A receive error is logged (rate-limited) and never ends the loop: it says nothing about the other gateways. Excess gateways beyond `max_gateways` are ignored with a rate-limited warning; existing sessions are never evicted to make room.

**Modes:**

| Mode | `upstream` | What happens |
|------|-----------|--------------|
| `proxy` (default) | `Some(addr)` | Uplinks, keepalives and TX_ACKs go to the LNS; ACKs and downlinks come back to the gateway that owns the socket they arrived on. Filters apply. |
| `listen` | `None` | There is no LNS. Messages are only recorded (status `Captured`, the filters are not consulted), and with `listen_acks` the proxy answers PUSH_DATA with PUSH_ACK and PULL_DATA with PULL_ACK. It never sends a downlink. |

**Blocking behaviour:**

| Scenario | Action |
|----------|--------|
| Uplink blocked, stealth off | Drop PUSH_DATA to LNS, send synthetic PUSH_ACK to gateway |
| Uplink blocked, stealth on | Drop PUSH_DATA, no PUSH_ACK |
| Downlink blocked, stealth off | Drop PULL_RESP to gateway, send synthetic TX_ACK to LNS |
| Downlink blocked, stealth on | Drop PULL_RESP, no TX_ACK |

"Non-stealth" ACKs keep the gateway and LNS from retrying indefinitely, creating a cleaner interception. "Stealth" mode causes timeouts on both ends, which may be detectable but avoids revealing the interception. The synthetic TX_ACK carries the EUI of the downlink's gateway.

**`process_push_data`:** A single PUSH_DATA can contain multiple `rxpk` entries (if the packet forwarder batches several radio packets). The function processes all entries. If **any** is blocked, the entire PUSH_DATA is withheld. ACK is suppressed only if **all** blocked entries request stealth. `process_pull_resp` does the same for the single `txpk` of a PULL_RESP.

**`build_uplink_record` / `build_downlink_record`:** Return `(MessageRecord, Verdict)` as a tuple. Returning the verdict alongside the record avoids evaluating the filters twice — it is needed both to set `Status` inside the record and to decide whether to forward or to send an ACK. `crc_status` maps the Semtech `stat` value (1, -1, 0) to `CrcStatus` so that `core/` never sees the raw number. An uplink's `stat` reaches the decoder, which is how a CRC-failed packet becomes a malformed record.

**Loop protection.** If the configured upstream leads back to the proxy's own gateway-facing port, every forwarded datagram would arrive again as if a gateway had sent it. `handle_from_gateway` first asks `is_own_traffic`: a datagram whose source is one of the proxy's own LNS sockets, or that is a byte-for-byte copy of one sent upstream in the last two seconds, is dropped with an ERROR (rate-limited) before anything is recorded or forwarded. See `loopguard.rs`.

**`DropLogs`:** one `RateLimitedLog` per condition that unauthenticated datagrams or a failing network can trigger at packet rate (undersized or unknown datagrams, gateway cap reached, loops, receive errors, send errors, session set-up errors), so a noisy one cannot hide a rare one.

---

### `lorawan-sniffer/src/proxy/udp/packet.rs`

The Semtech wire format and nothing else. Constants for the six datagram kinds (PUSH_DATA 0x00, PUSH_ACK 0x01, PULL_DATA 0x02, PULL_RESP 0x03, PULL_ACK 0x04, TX_ACK 0x05) and the protocol version; `Header::parse` for the fixed 4 bytes; `gateway_eui(buf)` for the 8 bytes after them; the `serde::Deserialize` structs for the JSON (`PushDataPayload` with its `rxpk` list and optional `stat`, `RxPk`, `PullRespPayload`, `TxPk`; fields the sniffer does not read are not declared, and serde skips them); and the builders for the synthetic datagrams (`build_push_ack`, `build_pull_ack`, `build_tx_ack`, whose JSON is `{"txpk_ack":{"error":"NONE"}}`). A missing coding rate defaults to `4/5`.

---

### `lorawan-sniffer/src/proxy/udp/session.rs`

Per-gateway state. `SessionTable` maps `Eui64` to `GatewaySession` (`push_addr`, `pull_addr`, the LNS-facing `Arc<UdpSocket>`, `last_seen`). `get_or_create` refuses with `SessionError::CapReached` when the table is full and the EUI is new; otherwise, in proxy mode, it binds an ephemeral port of the LNS's address family, connects the socket to the LNS and spawns `read_upstream`, a task that forwards everything arriving on it to the main loop. Dropping a session aborts its reader task, so a released gateway leaves no task or socket behind. `sweep_idle` releases sessions silent for longer than the timeout. `is_own_upstream(src, upstream_ip)` recognises a datagram from one of the table's own ports (kept in a set) so the loop guard can reject it. A receive error on an LNS socket is logged and the reader carries on: on a connected UDP socket the kernel reports an ICMP "port unreachable" as `ECONNREFUSED` on the next call, which just means the LNS was not listening for a moment.

---

### `lorawan-sniffer/src/proxy/udp/loopguard.rs`

Two layers against the proxy forwarding to itself. `check_startup(bind, upstream_addrs)` refuses to start when an upstream address has the bind port and either the bind IP, or, for a wildcard bind, any address of this machine (`is_local_ip` binds a probe socket and calibrates itself against a documentation address, because hosts with `ip_nonlocal_bind` make every address look local). `cmd/root.rs` calls it from `resolve_upstream`. `RecentForwards` catches what the startup check cannot see (Docker port forwarding, NAT): a ring of the last 256 forwarded datagrams, kept as a hash and a time, and `is_echo` says whether a byte-identical datagram was sent within two seconds. The hash is keyed randomly per instance, so nobody can craft a datagram that collides with a forwarded one to get it dropped. Gateways put a fresh random token in every datagram, so a legitimate sender never repeats itself byte for byte; at worst it retransmits, and dropping a retransmission is harmless because the original was forwarded.

---

### `lorawan-sniffer/src/proxy/udp/tests.rs`

Loopback tests: fake gateways on one side, a fake LNS on the other, the real proxy loop in between, all on `127.0.0.1` with ephemeral ports and a timeout on every receive. They cover multi-gateway routing (same-token ACKs, a PULL_RESP reaching only its gateway), gateway-specific rules, session release and the gateway cap, blocking in both directions with and without stealth, listen mode, malformed frames, the loop protection, and that a broker that is down does not slow forwarding. A "nothing arrives" assertion is only meaningful once the proxy has handled the datagram in question, so the tests send the datagram that must be swallowed first and one that must get through second: once the second shows up, the first has been dealt with.

---

### `lorawan-sniffer/src/proxy/gw.rs`

Hand-written Protobuf message definitions for the ChirpStack Concentratord v4 wire format.

**Why hand-written instead of generated?** The canonical approach is to use `prost-build` with a `.proto` source file, running a code generator in `build.rs`. That approach requires the `protoc` compiler to be installed on the build machine. For this project, only a small subset of the proto schema is needed, and the hand-written stubs are simpler to maintain.

The stubs use `prost` attribute macros:
- `#[prost(message)]` — this is a Protobuf message (struct)
- `#[prost(oneof = "...", tags = "1, 2")]` — a Protobuf `oneof` field (Rust enum, only one variant active at a time)
- `#[prost(bytes = "vec", tag = "1")]` — a bytes field, stored as `Vec<u8>`
- `#[prost(uint32, tag = "1")]` — an unsigned 32-bit integer at field number 1

**Unknown fields are silently skipped.** Prost ignores field numbers it does not recognise. This means the stubs only need to declare fields that the sniffer actually reads — unrecognised fields from newer concentratord versions pass through unchanged.

The file defines the event hierarchy (`Event` → `UplinkFrame` / `GatewayStats`) and command hierarchy (`Command` → `SendDownlinkFrame` / `GetGatewayId` / ...), plus supporting types for modulation parameters and acknowledgement status codes.

---

### `lorawan-sniffer/src/proxy/concentratord.rs`

The ChirpStack Concentratord ZMQ MITM proxy.

**Why `std::thread` instead of `tokio::spawn`?**

ZeroMQ sockets cannot be used from async code directly. The `zmq` crate is synchronous — its `recv_bytes()` call blocks the thread. Using it inside an `async fn` would block a tokio worker thread and prevent other tasks from running.

The solution is two dedicated OS threads (not tokio tasks):

```rust
let event_handle = std::thread::Builder::new()
    .name("zmq-event".to_string())
    .spawn(move || { run_event_thread(...); })?;
```

These threads communicate with the tokio runtime through the same channels that the UDP proxy uses — the channels are `Send`, so they work across thread boundaries.

**Shutdown coordination with `AtomicBool`:**

Threads cannot use `CancellationToken` directly (it is an async type). Instead, an `Arc<AtomicBool>` is shared between the tokio runtime and the threads:

```rust
let shutdown = Arc::new(AtomicBool::new(false));
// ...
cancel.cancelled().await;  // tokio side: wait for the stop signal (SIGINT or SIGTERM)
shutdown.store(true, Ordering::Release);  // signal both threads
```

Each thread polls this flag at the top of its loop with a 100ms ZMQ poll timeout:

```rust
loop {
    if shutdown.load(Ordering::Acquire) { break; }
    zmq::poll(&mut items, 100)?;  // wait up to 100ms for a message
    // ...
}
```

`Ordering::Release` / `Ordering::Acquire` form a synchronisation pair: the write with `Release` is guaranteed to be visible to any subsequent read with `Acquire`. This prevents the CPU or compiler from reordering the memory operations across the flag write/read.

**Gateway identity:**

A Concentratord serves exactly one gateway, but not every message says which. Uplinks carry the gateway ID in `rx_info`, and so does the reply to a `GetGatewayId` command; downlinks carry it only when the gateway bridge fills it in. Both threads therefore share a `LearnedGateway` (an `Arc<Mutex<Option<Eui64>>>`; each thread holds a clone, and each message uses its own ID when it has one). `process_uplink` and `process_downlink` fall back to the learned ID, so a downlink without an ID is still attributed to the gateway seen on the uplinks, and the gateway filter and the record's `gateway_eui` keep working. A broken ID (not 16 hex digits) is ignored with a rate-limited warning, because a bridge that sends one sends it with every frame.

**ZMQ topology:**

```
Event channel (PUB/SUB):
  concentratord  ─[PUB]──► sniffer SUB ─► decode+filter ─► sniffer PUB ─► gateway bridge

Command channel (REQ/REP):
  gateway bridge ─[REQ]──► sniffer REP ─► decode+filter ─► sniffer REQ ─► concentratord
                                                          ◄─ response ◄──
```

**REQ/REP constraint for stealth mode:** The ZMQ REQ/REP pattern requires that every request receives exactly one reply. The bridge blocks waiting for a reply. This means the concentratord proxy cannot simply discard a downlink command — it must send *something* back. When a downlink is blocked, the sniffer sends a synthetic `DownlinkTxAck` with `IGNORED` status. The bridge receives a well-formed reply but the concentratord never transmits the packet.

**Relaying commands:** A command that is not blocked is passed on byte for byte on a REQ socket to concentratord and its answer returned (`relay`). The socket has send/receive timeouts and `REQ_RELAXED` set, and `relay` retries every second until concentratord answers or the shutdown flag is raised, because concentratord may still be initialising its hardware at startup. The reply to a relayed `GetGatewayId` is also decoded to learn the gateway ID.

**IPC socket permissions:** After binding its upstream sockets the proxy sets IPC socket files to 0777 (`chmod_ipc_socket`): connecting to a Unix socket needs write permission on the file, and the gateway bridge often runs as another user (`nobody` in chirpstack-mqtt-forwarder). This is also why the systemd unit needs the `concentratord.conf` override in this mode (a shared `/tmp`, no dynamic user).

**Joining the threads:** On shutdown, the tokio runtime waits for both threads to finish by joining them inside a `spawn_blocking` call:

```rust
tokio::task::spawn_blocking(move || {
    event_handle.join().ok();
    cmd_handle.join().ok();
}).await?;
```

This is necessary because `JoinHandle::join()` is a blocking call (it waits for the thread to exit). Calling it directly in async code would block the event loop.

---

### `lorawan-sniffer/src/mqtt/mod.rs`

The optional MQTT output (`[sniffer.mqtt] enabled = true`): every captured record is also published, as the same JSON the WebSocket carries, to a topic built from the record. `cmd/root.rs` calls `mqtt::start(&MqttConfig, CancellationToken)` once; it parses the topic template, builds the `rumqttc` client (with a rustls configuration when `tls = true`), spawns two tasks and returns the `mpsc::Sender<MessageRecord>` that `Outputs::with_mqtt` hands to the proxy, together with an `Arc<MqttStats>`. An invalid topic, QoS or TLS file stops startup; an unreachable broker does not — the connection is retried for as long as the sniffer runs.

- **`run_event_loop`** drives the connection. `rumqttc` does its network work only while it is polled, and polling also reconnects after an error, so this loop never returns on one: it waits with a doubling backoff (1 s up to 30 s) and resets the backoff when the broker acknowledges a connection. A `connected` flag (`Arc<AtomicBool>`, written with `Release` and read with `Acquire`) tells the publisher whether the connection is up.
- **`run_publisher`** takes records from the queue (capacity 1,024). While the broker is not connected it drops them, counts them and logs a rate-limited warning; otherwise it serialises the record and calls `try_publish` on the topic rendered from it.
- **`MqttStats`** holds atomic counters: `published`, `dropped` and `pending_high_water`, plus the two `RateLimitedLog`s for the warnings. For QoS 0 and a settled connection, `published + dropped` equals the number of records offered; for QoS 1 and 2 the figures are an upper bound on loss.

Delivery is best effort and never slows the proxy: `Outputs::emit` offers each record with `try_send`, and a full queue drops it. The module comment explains in detail why the client's own queue needs a policy (`rumqttc` keeps a hidden `pending` queue that would resend stale records after a reconnect): sessions are pinned clean, records are not handed to the client while the connection is down, and `pending` is emptied and counted after every connection error. Read it before changing the queue sizes (`REQUEST_CAP` 256, `RECORD_QUEUE_CAP` 1,024). `rumqttc`'s `Debug` output for its options prints the username and password, so nothing here formats the options or the credentials with `{:?}`; log lines name the broker as `host:port` and nothing else.

---

### `lorawan-sniffer/src/mqtt/topic.rs`

Topic templates. `TopicTemplate` is parsed once at startup (it implements `FromStr`) from text such as `lorawan-sniffer/{gateway_eui}/{direction}/{mtype}` into a list of literal and placeholder segments, so every mistake is a startup error and the per-record work is appending strings. `render(&MessageRecord)` produces the topic. The placeholders are `{gateway_eui}`, `{direction}`, `{mtype}`, `{status}`, `{dev_addr}` and `{dev_eui}`; a value the record does not have renders as `unknown`, so the topic keeps the same number of levels whatever the message. An empty template, an unknown placeholder, unbalanced braces and the MQTT wildcards `+` and `#` (which a publish topic cannot contain) are `TopicError`s. `config::validate` parses the template too, so the mistake is reported before anything starts.

---

### `lorawan-sniffer/src/mqtt/tls.rs`

`client_config(&MqttConfig)` builds the rustls `ClientConfig` for the broker connection. The broker's certificate is checked against the bundled public CA roots (`webpki-roots`) or, when `ca_file` is set, against the certificates in that file only (a private CA is not trusted on top of the public ones). A `client_cert_file` and `client_key_file` together switch on mutual TLS. A file that cannot be read or holds no usable certificate or key is an error at startup, not an endless reconnect loop. The crypto provider (`ring`) is passed in explicitly: rustls' default builder would pick whichever provider the dependency graph enables, and `aws-lc-rs` needs CMake and NASM to cross-compile to musl (the workspace `Cargo.toml` turns off `rumqttc`'s default TLS feature for the same reason).

---

### `lorawan-sniffer/src/mqtt/tests.rs`

Tests of the publisher against an in-process fake broker: a bare `TcpListener` that speaks just enough MQTT 3.1.1, using `rumqttc`'s own packet codec. It answers CONNECT with CONNACK, records every PUBLISH, and can be told to hang up after a number of publishes to imitate a broker that keeps failing. Records are offered through `Outputs`, exactly as the proxy does. The reconnect backoff is shortened (`Tuning`) so tests take milliseconds, and `test_support::capture_logger` lets them check the rate-limited log lines against the counters.

---

## 9. File-by-file reference — `webui`

The WebUI is a standard React + TypeScript SPA built with Vite. It communicates with the backend via:
- **WebSocket** at `/ws` — real-time live feed, and `/ws/labels` for device labels
- **REST GET `/api/messages`** — the table's history: on page load, on every change of the time range or a filter, and on every `/ws` open. The server decides what a range and a filter contain; the WebUI replaces its rows with the newest 5,000 matching records and merges live records into them.
- **REST `PUT`/`DELETE /api/labels/{device}`** — label edits. The result is not written into the page's state; the change comes back on `/ws/labels`.
- **REST `/api/session`, `/api/login`, `/api/logout`** — whether the sniffer asks for a login, and the login itself.

Every URL is relative (`/api/...`, and `ws://` or `wss://` built from `location`), so the page works the same from the sniffer, behind a reverse proxy, and through the Vite dev server.

### `webui/package.json`, `vite.config.ts`, `index.html`, `src/main.tsx`

`package.json` has three scripts: `dev` (Vite's dev server), `build` (`tsc && vite build`, so a type error fails the build) and `test` (`node test/history.test.mjs`). The runtime dependencies are only `react`, `react-dom` and `react-virtuoso` (the virtual table). `vite.config.ts` does three things: writes the build to `dist/` (which `rust-embed` embeds), proxies `/ws` and `/api` to `localhost:8888` for `make webui-dev`, and defines `__APP_VERSION__` from `package.json`, which the footer shows. The version comes from `package.json` and not from the Rust crate because the Docker WebUI stage only has the `webui/` folder, so a release has to bump both; a test in `api/mod.rs` fails if the two differ (see "Cutting a release" in section 12). `index.html` is the one page, and `main.tsx` mounts `<App />` in it. `vite-env.d.ts` declares the types of Vite and of the `__APP_VERSION__` constant; `tsconfig.json` is strict and also rejects unused locals and parameters.

### `webui/src/types/messages.ts`

TypeScript interfaces that mirror the Rust `MessageRecord` struct and its enums. These are the types used throughout the frontend. Keeping them in one file makes it easy to update when the Rust types change. `deviceId()` lives in `lib/history.ts` and is re-exported from here. The file also has the label frames (`LabelsSnapshot`, `LabelEvent`), `shortEui()` and the two texts for a malformed row: `malformedReason()` (the full sentence for the hover text) and `malformedShort()` (two or three words that fit in a cell, because a touch screen has no hover).

### `webui/src/lib/history.ts`

The history logic as plain functions: no React, no timers, no `fetch`. It may only use `import type`, because `npm test` transpiles this one file and loads it on its own.

- **The filter rules.** `matchesFilter` says whether a record belongs in the table (device text against the DevAddr, the DevEUI and the label; type; status; gateway; time; malformed). `historyParams` builds the query string that asks the server for the same thing, with every value percent-encoded. `toHistoryFilter` turns the filter bar's state and a clock reading into a `HistoryFilter` (the next bullet covers its time range). These must agree with `core/history.rs` and `Storage::query_history`.
- **The time range.** The filter bar's time selector is either a span counted back from now (`5m`, `15m`, `1h`, `24h`) or one local day. `toHistoryFilter` turns it into `fromMs` / `untilMs`, `dayBounds(day)` gives a day's local midnight and the next one (a day with a clock change is 23 or 25 hours long, which is why a day is two instants, not 24 h), `pickDay` accepts a date-input value only if it is a real date not after today, and `normaliseFilters` makes a range saved by an older version safe to use. `historyParams` sends the bounds as `from` and `before`.
- **Merging.** `mergeRecords` de-duplicates by `id`, keeps the newest 10,000 and lets the loaded copy win. `mergeAfterLoad` builds the table from a load's answer plus the live records it lacks, by membership in the answer and never by comparing timestamps.
- **Counts.** `olderAfterLoad`, `matchingCount` and `malformedAfterLoad` give the footer's numbers from the response headers and the live records.
- **The request state machine.** `nextRequests(state, event, now)` takes an event (filter change, typing, labels, socket open, response, failure, pause, resume) and returns the next state and the actions to run (send a load, abort, start or cancel a timer, apply, hold). One generation counter covers every load; an answer from an older generation is dropped.
- **Back-off.** `nextReconnect` gives the reconnect delay and `nextRetryDelay` the retry delay, both jittered. Feed `nextReconnect` the previous *nominal* delay, not the jittered one, or the series drifts.

### `webui/src/hooks/useHistory.ts`

Runs what `nextRequests` decides: timers, one `AbortController` per request, a 30 s timeout, the retry timers, and `fetch` with the headers read back into counts. It keeps the loaded rows, a log of the live records received since each load was sent (the carry-over needs them), and the pause buffer. It returns the rows, the counts, `busy`, `behind`, `error` and `writeFailures`, and `pause()` / `resume()`.

### `webui/src/hooks/useWebSocket.ts`

A React hook that manages one WebSocket connection (connect, receive, reconnect after a drop). It is generic over the event type and takes the path, so the same code serves `/ws` (records) and `/ws/labels` (labels). It hands each parsed frame to a callback and calls `onOpen` every time the socket opens, which is how `/ws` triggers a reload of the range. Reconnect waits grow from about 3 s to 60 s and are randomised; a connection that lasted a minute resets them. A browser cannot read why a handshake was refused, so on every close the hook first calls `checkSession()`: `login` means it reports the end of the session and stops; anything else, `unreachable` included, reconnects as before (a close with code 1013 for falling behind is not a session end).

### `webui/src/hooks/useLabels.ts`

Device labels as React state: a `Record<identifier, label>` plus `synced`, which becomes true when the first snapshot has arrived (until then an empty map means "nothing heard yet", not "no device is labelled", so a device filter that depends on a label cannot be decided in the browser). The state is fed **only** by the `/ws/labels` stream: a snapshot replaces the map, an event updates one entry. `saveLabel` and `removeLabel` call `PUT` / `DELETE /api/labels/{device}` and return success or the API's error message, but never write the state themselves. A response can arrive after a newer change made in another browser, and applying it would put the older label back; the stream delivers changes in the order the server saved them, so following it alone always ends on the stored value. Saving empty text goes through `DELETE`, so there is one path for removal. A 401 calls `reportUnauthorized()` and returns "Your session has ended. Log in again."; the requests time out after 15 s so a stalled connection cannot leave the dialog busy.

### `webui/src/session.ts`

The login's side in the browser, with no React: `checkSession()` asks `GET /api/session` and answers `open` (login off), `logged-in`, `login` (a 401) or `unreachable` (anything else, a network error included); `logIn(token)` and `logOut()` `POST` JSON with `Content-Type: application/json`, which the server insists on for both; and a small bus, `onUnauthorized(listener)` and `reportUnauthorized()`, that carries "the session is gone" from wherever a 401 is seen (a history load, a label save, a refused WebSocket) to the gate in `App.tsx`. Only relative URLs, and the cookie is never read or stored by script.

### `webui/src/App.tsx`

The root component, in two layers. `App` is the gate: it asks `checkSession()` on load and shows "Connecting…", "Cannot reach the sniffer. Retrying…" (asking again every 3 s), `LoginPage`, or `SnifferApp`; a call to `reportUnauthorized()` sends it back to the login, which removes `SnifferApp` and its sockets. `SnifferApp` is the page itself. Connects the hooks to the display components: `useHistory` for the rows, `useLabels` for the labels, `useWebSocket` for the live feed. It filters the held rows with `matchesFilter` at render time, on a 5 s tick so that rows leave the range without new traffic, and works out Matching, Shown and Blocked. The filters and the column choices are kept in `localStorage` (`sniffer-filters`, `sniffer-columns`) with the stored values laid over the defaults, so a setting added by a later version still exists for a browser that saved the old shape. The CSV export (started from the ⚙ Settings menu) writes the rows shown with a `malformed` and a `label` column at the end, so tools that read the columns by position keep working; the label is always quoted, and one that starts with `=`, `+`, `-` or `@` gets a leading apostrophe so a spreadsheet does not run it as a formula.

### `webui/src/components/MessageTable.tsx`

The main data grid. Displays one row per `MessageRecord`. Implements virtual scrolling (only renders visible rows, through `react-virtuoso`'s `TableVirtuoso`) for performance with large message counts. Each row has `data-id` with the record's `id`, which scripts read to compare the table with the database. Type badges are coloured by category; RSSI and SNR are coloured by quality (`rssiClass`: at or above -100 dBm good, at or above -110 fair, else poor; `snrClass` is relative to the spreading factor's minimum decodable SNR, parsed from the data rate: 10 dB above it is good, 3 dB fair, else poor). The Label column (`LabelCell`) is clickable only on rows that have an identifier. A malformed row gets `row-malformed`, a dashed badge, and its short reason in the Device cell. Values that change over time, such as the labels, reach the rows through Virtuoso's `context` and not through closures, because Virtuoso may keep an item renderer it was given earlier.

### `webui/src/components/LabelDialog.tsx`

The modal for the label of one identifier (a DevAddr or a DevEUI), opened by clicking the Label cell of a row that has one. A text field limited to 64 characters (the server's limit), **Save**, **Delete** (shown only while a label exists, and following the live value if another browser changes it) and **Cancel**. Enter saves; Escape or a click outside cancels, except that a text selection dragged out of the field must not count as a click on the backdrop. The field is initialised from the current label once and is not rewritten under the user's hands if the label changes meanwhile. While a request is in flight the buttons are disabled, and a failure shows the server's message (`{"error": ...}`) in the dialog instead of closing it. Focus moves to the field on open and back to the opener on close.

### `webui/src/components/FilterBar.tsx`

Filter controls: the device text (it matches the DevAddr, the DevEUI and the label), the MType multi-select (with the "Show malformed" checkbox at the end of its menu, because the header has no room for another control at 800 px), status, gateway and the time selector. They set what the WebUI asks `/api/messages` for, and they filter the live records; they do not affect what the sniffer captures or stores. The gateway list is every gateway seen in the session, shown in its short form unless two share the same last 8 digits.

The time selector offers four spans and **Select day**. The calendar is the browser's own: a date input laid invisibly over the select is opened with `showPicker()` from inside the select's change event (it needs the user's activation), so the popup appears under the select. Days to come are greyed out (`max` is set to today each time, because "today" moves on at midnight). If `showPicker()` is missing or refused, the date input is shown in the select's place to be typed into. The change handler is a native listener on the input rather than React's `onChange`, because React compares the value with the one it saw last and misses a date set through the input's `value` property by a script or a form-filling extension. A pick sets `timePreset: 'day'` and `day: 'YYYY-MM-DD'`, and `lib/history.ts` turns that into the `from` / `before` pair.

### `webui/src/components/ColumnPicker.tsx`

The **⚙ Settings** popover: it shows or hides individual columns, then, after a separator, holds the actions, which close it when chosen: **Export CSV** (the rows shown; `App` passes `onExport` and the row count) and, when the login is on, **Log out** (`onLogout`). The header has no room for buttons of their own at 800 px.

### `webui/src/components/LoginPage.tsx`

The login page: a password field, **Log in**, Enter submits. It does not send a blank field, trims the token, ignores a second submit while one is out (every wrong token counts against everyone's attempts), keeps the text after an error, and shows the server's message (wrong token, the lockout, a proxy that changed `Host`) or, when the sniffer accepted the token but a new check still finds no session, a line about allowing cookies.

### `webui/src/components/HistoryNotice.tsx`

The one-line strip between the table and the footer. It joins, in this order, the messages that apply: a failed load, the count of records the database failed to store, "the database is behind", and the cap ("Showing the newest 5,000 of N matching messages in …"). It renders nothing when there is nothing to say.

### `webui/src/components/StatusBar.tsx`

Displays Matching, Shown, Blocked and the malformed count, the WebSocket connection state, the version and copyright line (the version comes from `package.json`, see `vite.config.ts`), and the live/pause toggle. Matching reads "Loading…" while a load is waiting or in flight. Each count carries `data-stat` and `data-value` attributes, which scripts read to compare the page with the database.

### `webui/src/styles/index.css`

All styling, in pure CSS with no framework. Theming is done with CSS custom properties on `:root`, and dark mode is a `@media (prefers-color-scheme: dark)` block that overrides the same variables, so no component knows which theme is active. The ones you will touch most: `--dir-up` / `--dir-down` (uplink blue, downlink amber), `--mtype-{join,up,down,other}-{bg,fg}` (the type badge palette), `--blocked-bg` / `--blocked-border` (blocked-row tint and left accent), `--malformed-border` (the dashed border of the malformed badge; `.row-malformed` dims the row), `--signal-good` / `--signal-fair` / `--signal-poor` (RSSI and SNR colours), `--row-alt` (zebra stripe), `--notice-*` (the history strip) and `--font-mono` (every data cell). Layout is designed for an 800 px wide touchscreen first (see `docs/MOBILE.md`), which is why several columns are opt-in and the header has no room for more controls.

### `webui/test/`

`history.test.mjs` is the test behind `npm test`: a plain Node script that transpiles `lib/history.ts` with the `typescript` package the project already has. It covers the request state machine, merging, counts and back-off, and the cases of `history-vectors.json`, which the Rust side also reads (see "Modifying a history filter" below).

---

## 10. Build, release and packaging

This section covers how the code becomes a release: the toolchain files, the Makefile, the release archives, the Debian packages, the Docker images and the pipeline that runs them when a version tag is pushed. The README has the user-facing steps ([Building from Source](../README.md#building-from-source), [Cross-Compilation](../README.md#cross-compilation), [Install on Debian or Ubuntu](../README.md#install-on-debian-or-ubuntu-deb), [Install from the archive](../README.md#install-from-the-archive-other-linux-systems), [Docker](../README.md#docker) and [Releases](../README.md#releases)); this section explains how the pieces fit together and what to keep in mind when you change them.

### `rust-toolchain.toml`

Pins the entire team to the same Rust toolchain:

```toml
[toolchain]
  channel = "stable"
  components = ["rustfmt", "clippy"]
  profile = "default"
  targets = ["x86_64-unknown-linux-musl"]
```

When anyone runs `cargo build`, `rustup` automatically downloads and uses the toolchain specified here. The `x86_64-unknown-linux-musl` target is pre-installed so Docker cross-compilation works without extra setup.

### `Cargo.toml` (root) and `Cargo.lock`

Besides the workspace members and `[workspace.dependencies]` (section 3), the root `Cargo.toml` has `[profile.release] strip = true`: release binaries carry no symbol table, which makes them about a third smaller and is what a Debian package expects of the programs it installs. Two dependency choices are made for cross-compilation, and commented in the file: `rumqttc` is used without its default TLS provider (which would pull in `aws-lc-rs`, needing CMake and NASM to build for musl) and `rustls` is given the `ring` provider instead.

`Cargo.lock` records the exact version of every dependency (including transitive dependencies) used in the last successful build. Committed to version control for binaries (unlike libraries, which omit `Cargo.lock`). This ensures reproducible builds — `cargo build` on any machine produces the same binary. The release workflow builds with `--locked`, so a `Cargo.toml` that the lock file does not match fails the release instead of quietly updating it.

### `.cargo/config.toml` and `Cross.toml`

`.cargo/config.toml` sets `-C target-feature=+crt-static` for the two musl targets, which makes the binaries fully static (no libc to match on the target machine). It is a dotfile that the `.gitignore` re-includes on purpose, and the release workflow refuses to build if it is missing from the checkout, because the build would still succeed and give a different binary from the one built locally.

`Cross.toml` configures [`cross`](https://github.com/cross-rs/cross), which runs `cargo build` inside a container that has the cross-compiler for the target. The one setting, `[build.env] passthrough = ["SOURCE_DATE_EPOCH"]`, is what makes the binaries reproducible: `cross` hands the container only the variables listed there, and the binary embeds the WebUI's files together with their timestamps (`rust-embed`), which are taken from `SOURCE_DATE_EPOCH` when it is set (see "Reproducible builds" below).

### `Dockerfile`

A three-stage build, for building an image from source:

1. **`webui` stage** (`node:20-alpine`): Runs `npm ci` + `npm run build`. Produces `webui/dist/`.
2. **`builder` stage** (`ghcr.io/rust-cross/rust-musl-cross:x86_64-musl`): Compiles the Rust binary targeting `x86_64-unknown-linux-musl` (static musl binary, no glibc dependency).
3. **`alpine:3.23`**: Copies only the compiled binary, `docker/sniffer.toml` and `docker/docker-entrypoint.sh`. Final image is ~25 MB.

**Why `rust-musl-cross` and not `rust:latest`?** Two problems arise with the standard Rust Docker image:
- The `musl` target has `dynamic-linking=false`, which means proc-macro crates (like `async-trait`) cannot be compiled because they produce `.so` shared libraries. The `rust-musl-cross` image uses a glibc host for proc-macro compilation while targeting musl for the final binary.
- ZeroMQ is compiled from C++ source. Debian's native `g++` emits references to glibc-internal symbols (`__libc_single_threaded`, etc.) that do not exist in musl. `rust-musl-cross` provides `x86_64-linux-musl-g++` — a C++ cross-compiler that targets musl and avoids these symbols.

**Dependency caching layer:** The Dockerfile copies only `Cargo.toml`/`Cargo.lock` first, compiles stubs (with `|| true` to ignore expected errors), then copies the real source. Docker caches the dependency compilation layer; it is only invalidated when `Cargo.toml` or `Cargo.lock` changes, not when application source changes.

### `Makefile`

Convenience targets. The ones you will use every day:

| Target | What it runs |
|--------|-------------|
| `make build` | `cargo build` (debug, native host) |
| `make build-full` | Rebuild WebUI, then `cargo build` |
| `make run` | `cargo run --bin lorawan-sniffer -- -c sniffer.toml` |
| `make configfile` | Prints the annotated configuration template |
| `make webui-dev` | Vite dev server with proxy to localhost:8888 |
| `make webui-test` | `npm test` in `webui/`: the history logic and the shared filter vectors |
| `make test` | `make webui-test`, then `cargo test` |
| `make fmt`, `make clippy` | `cargo fmt`, `cargo clippy` |

The release targets, described in the sections below:

| Target | What it does |
|--------|-------------|
| `make build-x86_64-unknown-linux-musl`, `make build-aarch64-unknown-linux-musl` | Build the WebUI, then `cross build --release` for one target |
| `make build-all` | Build the WebUI once, then cross-compile both targets |
| `make dist` | `build-all`, then wipe `dist/`, run `packaging/package.sh` for both targets, `make deb`, and write `dist/SHA256SUMS` over the archives and the packages |
| `make deb` | `packaging/package-deb.sh` on every `dist/lorawan-sniffer-*-linux-*.tar.gz` (no rebuilding; needs `dpkg-deb`) |
| `make deb-check` | Compare the Debian maintainer scripts with debhelper's own output, in a `debian:12` container (needs Docker and network) |
| `make docker-build` | `docker build` from source, for local testing (x86_64 only) |
| `make docker-build-release` | Release image for the host architecture, from the `dist/` binaries |
| `make docker-buildx` | `make dist`, then build and push the multi-arch image (amd64 + arm64) |

Variables you can set: `SOURCE_DATE_EPOCH` (see below), `DIST_DIR` (default `dist`), `DEB_CHECK_ARCHIVE` and `DEB_CHECK_IMAGE` (what `deb-check` checks, and in which image), and `DOCKER_IMAGE` / `DOCKER_TAG`.

### Reproducible builds

Building the same commit twice gives the same archives, and the same packages when the same `dpkg-deb` builds them. One variable carries the commit's time through the whole chain:

1. The `Makefile` takes `SOURCE_DATE_EPOCH` from `git log -1 --format=%ct` (unless you set it, for example when building from a tree without git history) and exports it. The workflow does the same.
2. `Cross.toml` lets it into the build container, where `rust-embed` stamps the embedded WebUI files with it instead of with the moment they were built.
3. `package.sh` uses it as the archive's `--mtime`, with names sorted, owner 0 and `gzip -n`, so the archive records no user, no machine and no build time.
4. `package-deb.sh` uses it for every timestamp in the package, with fixed owners, modes and file order, and runs the compressor on one thread, because the number of threads changes the compressed bytes.

If a release is not reproducible, look for a step that stamps the current time.

### Release archives: `packaging/package.sh`

`packaging/package.sh TARGET [OUTDIR]` (run from the repository root, after the cross build) turns `target/TARGET/release/lorawan-sniffer` into `OUTDIR/lorawan-sniffer-VERSION-linux-ARCH.tar.gz`, where `VERSION` is read from `lorawan-sniffer/Cargo.toml` and `ARCH` is what `uname -m` prints on the target (`x86_64`, `aarch64`). It first refuses a binary that would not work where the archive says it does: the wrong architecture, one that is not statically linked, or one with no WebUI embedded (it looks for the `assets/index-` file name of the Vite bundle, since a release built without `npm run build` would serve a blank page). The archive holds one folder with the binary, `install.sh`, the configuration templates (`sniffer.toml`, `sniffer.env`), the systemd unit, `concentratord.conf`, `README.md`, `CHANGELOG.md` and `LICENSE`. It also writes the same binary to `OUTDIR/docker/amd64/` or `arm64/`, where `docker/Dockerfile.release` looks for it.

### Debian packages: `packaging/package-deb.sh` and `packaging/deb/`

`packaging/package-deb.sh ARCHIVE [OUTDIR]` builds `lorawan-sniffer_VERSION-1_ARCH.deb` (`amd64` or `arm64`) **from a release archive**, not from the build tree, so the package and the archive can only ever hold the same binary (the script reads the package back and compares the binary's bytes). It needs only POSIX shell tools and `dpkg-deb` 1.21 or newer, no debhelper. In order:

1. Unpack the archive (refusing one with paths outside its folder) and repeat the safety checks: architecture, static linking, a shipped `sniffer.toml` that neither turns the login on nor carries a token (it is a conffile that every install gets), a `copyright` file that mentions the holder named in `LICENSE`, and exactly one `Documentation=` line in the unit, which becomes the package's `Homepage`.
2. Stage the files: the binary in `/usr/bin`, the unit in `/lib/systemd/system` with **one line changed** (`ExecStart=/usr/local/bin/...`, where `install.sh` puts the program, becomes `/usr/bin/...`; the script insists that exactly one line matches), the two configuration files as conffiles in `/etc/lorawan-sniffer/` (`sniffer.toml` mode 0644, `sniffer.env` mode 0600 because it holds secrets), the README and changelog, `concentratord.conf` under the examples, and a one-entry Debian changelog.
3. Assemble the maintainer scripts. `packaging/deb/preinst`, `postinst`, `prerm` and `postrm` are hand-written and each has one `#DEBHELPER#` line; the script replaces it with debhelper's systemd snippets from `packaging/deb/autoscripts/`, filled in the way `dh_installsystemd` fills them in. The hand-written parts are small: `preinst` refuses to install next to a program that `install.sh` put in `/usr/local/bin` or a unit written by hand in `/etc/systemd/system` (it would take precedence over the packaged one and the service would keep running the old program), and `postrm` deletes the database on `purge` only (dpkg removes just the files it installed, and `purge` means "forget this package entirely").
4. Fill `control` (`@VERSION@`, `@ARCH@`, `@INSTALLED_SIZE@`, `@HOMEPAGE@`), write `md5sums`, build with `dpkg-deb --root-owner-group -Zxz --threads-max=1`, and check the result.

The package version is the release version plus `-1`: `2.2.0` becomes `2.2.0-1`, and a pre-release `2.2.0-rc.1` becomes `2.2.0~rc.1-1`, which sorts before `2.2.0-1` (a plain hyphen would sort after it). `package-deb.sh --version VERSION` prints that mapping; the workflow uses it, and `dpkg --validate-version` double-checks it. The file name keeps the release version because GitHub rewrites `~` in asset names; dpkg reads the version from inside the package.

**Why the snippets are copied, and how they are checked.** The systemd parts of the scripts (enable and start on install, restart after an upgrade, stop on removal, clean up on purge) are exactly what a debhelper-built package would have. Rather than depend on debhelper at build time, `packaging/deb/autoscripts/` holds the five snippets of debhelper 13.11.4 (the version in Debian 12) unchanged, with a `SHA256SUMS` file that `package-deb.sh` verifies before using them; its `README.md` records where they came from and their (public-domain) licence. To prove the assembled scripts equal what debhelper would generate, `make deb-check` runs `packaging/deb/check-autoscripts.sh` in a `debian:12` container, with `packaging/` and the archive's folder mounted read-only. The script installs debhelper (and stops unless it is 13.11.4), compares the kept templates with the installed ones, builds a `.deb` with `package-deb.sh`, builds a scratch source package with the real `dh` (compatibility level 13, default options) from the same unit and the same four hand-written scripts, and compares the four maintainer scripts and their modes byte for byte. It prints a diff and fails on any difference, or if debhelper made a control file the package lacks. It takes the x86_64 archive in `dist/` that sorts last (or `DEB_CHECK_ARCHIVE`). Run it after any change in `packaging/deb/` or in the assembly code of `package-deb.sh`; the release workflow does not run it.

### Service and installer files

These are shared by the archive install and the package, so a change affects both.

- **`packaging/lorawan-sniffer.service`** — a hardened unit: `DynamicUser=yes` with `StateDirectory=lorawan-sniffer` (the database lives in `/var/lib/lorawan-sniffer`, the only place it can write), no capabilities (the ports are above 1024), `ProtectSystem=strict`, a system-call filter, `LimitNOFILE=4096` (one UDP socket per gateway in proxy mode), `Type=exec`, `TimeoutStopSec=20s` (SIGTERM makes the sniffer save its queue and exit within seconds), and `EnvironmentFile=-/etc/lorawan-sniffer/sniffer.env` so secrets can be referenced from `sniffer.toml` as `"$NAME"`. The `ExecStart` path is `/usr/local/bin` here and rewritten for the package.
- **`packaging/concentratord.conf`** — a systemd drop-in for Concentratord mode. The sniffer talks to Concentratord and to the gateway bridge over ZeroMQ sockets in `/tmp`; the default unit's private `/tmp` and throwaway user would see none of them, so the drop-in shares the real `/tmp`, runs as root and relaxes the system-call filter for the two calls ZeroMQ and SQLite make in this mode.
- **`packaging/install.sh`** — installs, upgrades or (`--uninstall`) removes the archive install: binary to `/usr/local/bin`, unit to `/etc/systemd/system`, configuration to `/etc/lorawan-sniffer`. It never starts or restarts the service (it prints the command, so an upgrade on a busy server happens when the operator chooses), keeps an existing `sniffer.toml` and writes the new template next to it as `sniffer.toml.new`, and puts a freshly generated login token in whichever file it writes (from `/dev/urandom`, staged root-only and renamed into place so the token is never world-readable under a temporary name). It runs the binary with `--version` first, so an archive for the wrong architecture fails before anything is copied.
- **`packaging/sniffer.toml`** and **`packaging/sniffer.env`** — templates of the system install's configuration (set up to run next to the LNS on the same machine) and of its secrets file. `install.sh` substitutes the generated token for the one empty `    token = ""` line of `sniffer.toml` and stops if there is not exactly one, and `package-deb.sh` refuses a `sniffer.toml` that turns the login on or carries a token, because the file is a conffile that every install gets.

### Release images: `docker/` and `Dockerfile`

`docker/Dockerfile.release` builds the image that GitHub publishes. It compiles nothing: it copies the static binary from `dist/docker/<arch>/` (written by `package.sh`), so the image holds the same binary as the archives and a multi-arch build needs no emulation. `docker/sniffer.toml` is the image's configuration, a file of `"$SNIFFER_..."` and `$GATEWAY_...` references that the sniffer's environment expansion fills in; `docker/docker-entrypoint.sh` gives each variable its default (decided before the upstream server gets its fallback value, so an image with no upstream starts in listen mode) and then `exec`s the sniffer, which makes it PID 1 and the receiver of `docker stop`'s SIGTERM. The README lists the variables. Adding a configuration setting to the image means a line in all three places (see "Adding a configuration setting" in section 12). `docker-compose-udp.yml` and `docker-compose-concentratord.yml` are examples.

### The release workflow: `.github/workflows/release.yml`

Pushing a tag `vX.Y.Z` builds, packages and publishes a release. Running the workflow by hand (Actions > Release > Run workflow) is a dry run: everything is built and packaged, the image is built but not pushed, and the archives and packages are kept as the `release-files` artifact of the run; nothing is released. A second run for the same ref waits for the first and a publishing run is never cancelled. There are four jobs:

1. **`check`** — decides what is being released and refuses anything inconsistent before spending minutes on builds. It reads the version from `lorawan-sniffer/Cargo.toml` (`-` in it makes a pre-release), works out the Debian version with `package-deb.sh --version`, and, on a tag, fails unless the tag is exactly `v` plus that version, the tagged commit is an ancestor of the default branch, and `CHANGELOG.md` has a section for the version (it becomes the release notes).
2. **`test`** — builds the WebUI, runs `npm test`, `cargo fmt --check` and `cargo test --locked`.
3. **`build`** — a matrix over the two musl targets. It builds the WebUI, builds `cross` from a pinned git revision (cached), runs `cross build --release --locked` with `SOURCE_DATE_EPOCH` set, packages the archive and the `.deb` as `make dist` does, and reads the finished package back (`dpkg-deb --field`) to check its version and architecture. The result is uploaded as an artifact.
4. **`publish`** — the only job that can write anything (`contents: write`, `packages: write`). It checks that exactly the expected files exist (two archives, two packages, with the right fields), writes `SHA256SUMS`, then builds the multi-arch image from `docker/Dockerfile.release` (tagged `X.Y.Z`, `X.Y` and, for a final release only, `latest`) and, on a tag run, logs in to `ghcr.io`, pushes it and creates the GitHub Release with `gh release create --verify-tag` (the archives, the packages, `SHA256SUMS`, and the changelog section as the notes; `--prerelease` for a version with a suffix).

When you change what `make dist` produces (a new file in the archive, another architecture), change the workflow's `build` and `publish` checks with it: they look for exact file names. The README's [Releases](../README.md#releases) section has the steps for cutting one.

### `tools/`

Two scripts for development and load testing, not part of any release:

- `tools/gwsim.py` — a Semtech UDP gateway simulator and fake LNS in one Python 3 file (standard library only, a single `selectors` loop, so a few hundred simulated gateways fit in one process). Each simulated gateway has its own EUI and, like a real packet forwarder, separate push and pull sockets. Options generate JoinRequests (`--joins`), a new DevAddr per uplink (`--random-devaddr`) and malformed frames (`--malformed-ratio`), which is how the label and malformed-frame features are tried by hand. The usage examples are at the top of the file.
- `tools/procstat.sh` — samples the peak and current memory (`VmHWM`, `VmRSS`) and the open file descriptors of a running process from `/proc`, in POSIX shell, for example `tools/procstat.sh "$(pidof lorawan-sniffer)" 10`.

---

## 11. Design decisions

### Single binary, self-contained

The React SPA is compiled into the binary at build time (`rust-embed`). There is no separate web server, no static file directory to deploy, no nginx config. One binary + one config file + one SQLite file = complete deployment.

### Protocol-agnostic `MessageRecord`

Rather than having separate storage and WebSocket formats for UDP and Concentratord, both proxy backends produce the same `MessageRecord`. This means the API, WebSocket handler, and storage layer are protocol-agnostic — they have no knowledge of UDP or ZMQ. Adding a third gateway protocol in the future requires only a new file in `proxy/`.

### First-match-wins filter semantics

The filter engine stops at the first matching rule. This is the same model as iptables, Wireshark display filters, and most rule-based systems. Operators can express priority by ordering rules. An "allow" rule before a broader "block-all" rule acts as an exception.

### `mpsc` for storage, `broadcast` for WebSocket

These two channels have different fan-out characteristics:
- One storage writer (single consumer) → `mpsc` is the natural fit.
- N WebSocket clients (variable consumers) → `broadcast` is the natural fit.

Using `try_send` for the storage channel means the proxy is never blocked by a slow disk. The channel buffer (1024 records) gives the writer time to catch up during bursts. At LoRaWAN data rates (typically < 10 packets/second), the buffer is never approached.

### ZMQ threads isolated from the async runtime

ZeroMQ is fundamentally incompatible with async runtimes — its blocking model would deadlock a tokio task. Dedicated OS threads with `AtomicBool` shutdown signals are the cleanest solution. The threads participate in the same data pipeline (channels, broadcast) as the async tasks; only the ZMQ I/O itself is synchronous.

### Mutex poisoning is an error, not a panic

When a thread panics while holding a `Mutex`, the mutex becomes "poisoned". The default Rust behaviour is to propagate the panic to every subsequent `lock()` call. The code converts poisoning into a recoverable `SnifferError::LockPoisoned` — the operation fails but the sniffer keeps running and can serve other requests.

### WAL mode SQLite

Without WAL (Write-Ahead Logging), SQLite uses a global write lock that blocks all readers during a write. WAL allows concurrent reads while a write is in progress. Since the storage writer is constantly inserting records while the API server is reading them, WAL is essential for query latency.

### A second, read-only connection for history

A device text cannot use an index, so a filtered history query can scan every record in the range, which on a million records takes hundreds of milliseconds. On the writer's connection that scan would hold the mutex every insert needs. In WAL mode a second connection reads a snapshot without touching the writer, so history gets its own. On the development machine, with queries running back to back on the reader, the writer's longest wait for the lock was under 0.01 ms. When the database is not in WAL mode there is no reader, and the sniffer refuses filters rather than run the scan on the writer. `Storage::open` therefore checks the journal mode it ended up with instead of assuming it.

### A marker in the storage queue, not a timestamp

The WebUI must not miss a record after a reconnect. Replaying from the newest timestamp fails in three ways: a record can be broadcast and still be waiting in the 1,024-record queue when the query reads the database; two gateway tasks can broadcast in the opposite order to their timestamps; a lagging client has no reconnect to trigger the replay. The design uses orderings instead of timing: storage is queued before the broadcast, the `/ws` subscription exists before the handshake is answered, and a `Settle` marker travels through the same queue as the records, so the writer answers it only after everything before it. The marker's single permit means at most one is ever queued, even when a request gave up waiting. Every reconnect then reloads the whole range, up to 5,000 rows, rather than a gap whose edges would have to be guessed.

### One LNS-facing socket per gateway

A proxy with a single upstream socket has to remember which gateway each reply belongs to, and the Semtech protocol makes that hard: ACKs repeat the token of the request they answer, but a PULL_RESP carries a fresh token chosen by the server, so a token table cannot route it. Giving every gateway its own socket connected to the LNS removes the question: whatever arrives on a socket belongs to the gateway that owns it, nothing has to be looked up or rewritten, and the LNS sees one source port per gateway, as it would without the sniffer. The cost is one file descriptor per gateway (the unit sets `LimitNOFILE=4096` for the default `max_gateways = 100`), and sessions are released after `session_timeout` of silence so a gateway that went away gives its socket back.

### One writer for labels

Saving a label and telling the other browsers about it are two steps, and if request handlers did both themselves, two requests could commit in one order and publish in the other, or a browser that closes the page could cancel a handler between the two steps. A single task (`LabelWriter`) takes commands one at a time, commits, publishes and then answers, so publish order equals commit order and a change that was queued is saved and published whether or not anyone still waits. The WebUI never writes the result of its own `PUT` into its state; it follows the event stream, which is always in the order the server saved.

### Stateless sessions

The login needs no session table. A session is a value signed (HMAC-SHA256) with a key derived from the token and from a random secret in the database, so it survives a restart, changing the token or starting on a new database ends every session, and a guessed token is not enough to forge a cookie, which means cookies need not be counted by the guessing limit. The cost is that a single session cannot be revoked on its own; "log out" clears the cookie in that browser, and changing the token logs everyone out. For a tool with one or two users that is the right trade for having nothing to clean up.

### Debian packages are built from the release archive

The `.deb` is assembled from the `.tar.gz` that was already built, not from the build tree. The two can then only ever contain the same binary, `package-deb.sh` can check that byte for byte, and building a package needs no compiler, no debhelper and no source tree. The price is that the maintainer scripts' systemd parts are debhelper's snippets, copied and checked, instead of generated; `make deb-check` exists to prove the copy is faithful.

---

## 12. Common maintenance tasks

### Adding a new field to `MessageRecord`

1. Add the field to `lorawan-sniffer/src/core/metadata.rs` (`MessageRecord` struct), and to the key lists of the `record_json_keys` test there, which pins the JSON shape (optional fields are serialised as `null`, never omitted).
2. Add a **new entry at the end of `MIGRATIONS`** in `lorawan-sniffer/src/core/storage.rs` with the `ALTER TABLE messages ADD COLUMN ...`. Never edit `INIT_SQL` or an entry that has shipped, and never insert one before the end: databases are upgraded by comparing `PRAGMA user_version` with the position in the list. Update the version list in the module comment.
3. Add the column to `SELECT_COLS` and `INSERT_SQL` (keep the placeholders numbered), pass the value in `insert_record`, and read it in `row_to_record` (columns are read by position, so the order in `SELECT_COLS` matters).
4. Populate the field in `proxy/udp/mod.rs` and `proxy/concentratord.rs` (the `build_*_record` functions), and wherever a test builds a `MessageRecord` literal.
5. Add the field to the TypeScript type in `webui/src/types/messages.ts`.
6. Display the field in `webui/src/components/MessageTable.tsx`, and add a column choice in `ColumnPicker.tsx` (new columns default to hidden, because the table already fills 800 px). If it belongs in the CSV export, append it to the end of the `HEADERS` list in `App.tsx`, so tools that read the export by column position keep working.
7. If a filter should use it, see "Modifying a history filter" below.
8. Document it in the README's "Message records" table.

**Important:** the migration is what upgrades existing databases; there is no manual `ALTER TABLE` step. Add a storage test that opens a database at the previous version and checks that the new column arrives (see the existing migration tests in `storage.rs`).

### Adding a new filter field

1. Add the field to `FilterConfig` in `config/mod.rs`.
2. Add the field to `FilterRule` in `lorawan-sniffer/src/core/filter.rs`.
3. Update `FilterRule::matches()` to check the new field. If the value is not always known (the gateway, the type of a malformed frame), decide what an unknown value does: for the existing ones, a rule that names specific values never matches an unknown one.
4. Update `build_filters()` in `cmd/root.rs` to map the config field, failing with a message that names the rule if a value does not parse.
5. Pass the value through `classify()` in `proxy/mod.rs` and the callers in both backends if `evaluate` needs more input.
6. Add documentation to the configfile template in `cmd/configfile.rs` and the README's "Blocking rules".

### Adding a configuration setting

1. Add the field to its struct in `config/mod.rs`, with a default in the `Default` impl (every struct is `#[serde(default)]`, so existing files keep working). If it holds a secret, do not derive `Debug` for the struct.
2. If it can be wrong in a way that would only fail late and quietly, check it in `validate`, with a message that names the setting and does not quote a secret.
3. Add it, with a comment, to the template in `cmd/configfile.rs`, and to `packaging/sniffer.toml` if a system install should show it.
4. For Docker, add a `"$NAME"` line to `docker/sniffer.toml`, a default in `docker/docker-entrypoint.sh` (and its `export` line), and a row in the README's environment variable table.
5. Add a test of the default and of the validation, next to the existing ones in `config/mod.rs`.

### Adding support for a new gateway protocol

1. Create `lorawan-sniffer/src/proxy/newprotocol.rs`.
2. Implement a `pub async fn run(config, filters, outputs, cancel) -> Result<()>`.
3. Use `super::{frame_fields, classify, log_message, Outputs}`: `frame_fields` decodes a frame (pass the CRC status for an uplink, `None` for a downlink), `classify(listen, filters, direction, rule_mtype, gateway)` applies the rules to `FrameFields::rule_mtype()` (pass `listen = true` only if your backend has a mode that never forwards), and `Outputs::emit` sends the record to storage, `/ws` and MQTT.
4. Add `Protocol::NewProtocol` to `config/mod.rs` (the setting), and a variant to `core::metadata::Protocol` (what a record says it came from) with its text in `protocol_str` and `row_to_record` in `core/storage.rs`, and in the `Protocol` type of `webui/src/types/messages.ts`.
5. Add a match arm in `cmd/root.rs`.
6. Declare the module in `proxy/mod.rs`.

### Modifying a history filter

The rule that decides whether a record matches the device text, type, status, gateway, malformed flag or time range exists twice: in `core/history.rs` and `Storage::query_history` (stored records) and in `webui/src/lib/history.ts` (live records, and the query string). Change both, and add cases to `webui/test/history-vectors.json`. The Node test checks the query string the browser builds and the records its rule keeps; a Rust test (`api/message_tests.rs`) sends each case's query through the real `/api/messages` router and compares the ids and `X-Total-Count`. Neither side may let a record through that the other would filter out. A change to the time bounds also touches `from` / `before` in `api/messages.rs` (`parse`) and the day functions of `lib/history.ts`.

### Running tests

```bash
cargo test                          # all tests
cargo test core::                   # only core/ module tests
cargo test api::                    # the API tests (router, login, labels, history)
cargo test decode_join_request      # tests matching this name
cargo test --test shutdown          # the integration test that starts the real binary
cargo test -- --nocapture           # show println! output during tests
cd webui && npm test                # the WebUI's history logic and the shared vectors
make test                           # npm test, then cargo test
```

Most `core::storage` tests use in-memory SQLite (`Storage::open_in_memory()`) — no file system setup required. The history tests run each case on an in-memory database and on a file database (`TempDb`), since only the file one has the read-only connection. The UDP and MQTT tests, and the router tests of the API, use loopback sockets and in-process fakes, with a timeout on every wait so that a bug fails a test instead of hanging it. A process has one global logger, so a test that needs log lines uses `test_support::capture_logger()` (any thread) or the `warn_line!` / `error_line!` macros with `linelog::start_capture()` (the test's own thread).

`large_db_history_queries` is ignored by default. It builds a database of 1,000,000 records and measures the history queries, the writer's lock wait and the size of the log file; run it in release mode on a real disk (`TMPDIR=/path/on/disk cargo test --release -- --ignored large_db_history --nocapture`). `HISTORY_ROWS` and `HISTORY_DAYS` change the size.

### Updating a dependency

1. Edit the version in the root `Cargo.toml` `[workspace.dependencies]` section.
2. Run `cargo update -p <crate-name>` to update `Cargo.lock`.
3. Run `cargo test` and `cargo clippy` to verify nothing broke.

The release workflow builds with `--locked`, so commit the changed `Cargo.lock` with the change. Mind the two features that exist for cross-compilation (`rumqttc` without its default TLS provider, `rustls` with `ring`): a dependency update that pulls in `aws-lc-rs` or another crate that needs CMake or NASM breaks the musl builds, which show only in `make build-all` or in the workflow, not in `cargo test`.

### Modifying the WebSocket message formats

The record format is defined in `make_ws_json` in `api/mod.rs`:

```rust
struct Envelope<'a> {
    #[serde(rename = "type")]
    kind: &'static str,   // always "message"
    data: &'a MessageRecord,
}
```

If you change this structure, update the corresponding TypeScript parsing in `webui/src/App.tsx` (the `/ws` consumer) and `webui/src/types/messages.ts`. The live-identifier listener (`note_sighting` in `cmd/root.rs`) also parses this envelope, reading only `data.dev_addr` and `data.dev_eui`, and the MQTT publisher sends the record without the envelope. The label formats are built by `make_labels_json` (the full set) and `make_label_json` (one change), both in `api/mod.rs`, and parsed by `webui/src/hooks/useLabels.ts`; a client that falls behind gets a fresh full set, so the snapshot must always be able to replace the client's state entirely.

### Cutting a release

The README's [Releases](../README.md#releases) section has the steps. In short: set the version in `lorawan-sniffer/Cargo.toml` and run `cargo check` so `Cargo.lock` follows, run `npm version X.Y.Z --no-git-tag-version` in `webui/` (a test in `api/mod.rs` fails if the two versions differ), move the `[Unreleased]` entries of `CHANGELOG.md` into a `## [X.Y.Z] - YYYY-MM-DD` section (its text becomes the release notes), commit and push to the default branch, then tag that commit `vX.Y.Z` and push the tag. To try the pipeline without releasing anything, run the workflow by hand: it is a dry run. Before tagging, `make dist` shows locally what the workflow will build, and `make deb-check` is worth running if anything in `packaging/deb/` changed.

### Changing the Debian package

- A file the package installs: add it to the release archive in `packaging/package.sh`, then to the staging in `packaging/package-deb.sh`, which reads files only from the archive.
- Control data (`control`, `conffiles`, `copyright`, `lintian-overrides`): edit it in `packaging/deb/`. `control` keeps its `@PLACEHOLDERS@`, and `package-deb.sh` stops if one is left unfilled. If `LICENSE` changes its holder, `copyright` must follow, or `package-deb.sh` stops.
- Maintainer scripts: edit the hand-written parts of `preinst`, `postinst`, `prerm`, `postrm`, and keep the single `#DEBHELPER#` line each one has, on a line of its own. Never edit the files in `autoscripts/`: they are debhelper's, and `package-deb.sh` checks them against `SHA256SUMS`. To move to a newer debhelper, replace the five snippets, update `SHA256SUMS` and `DH_VERSION` in `package-deb.sh` and `WANT_DH` in `check-autoscripts.sh`, and run `make deb-check`.
- After any of this, run `make dist` (or `make deb` on archives you already have) and `make deb-check`.
