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
10. [Build and toolchain files](#10-build-and-toolchain-files)
11. [Design decisions](#11-design-decisions)
12. [Common maintenance tasks](#12-common-maintenance-tasks)

---

## 1. What the software does

The sniffer is a **transparent man-in-the-middle (MITM) proxy** for LoRaWAN gateways. It inserts itself between a gateway and a LoRaWAN Network Server (LNS):

```
Gateway  <──>  lorawan-sniffer  <──>  LNS / Gateway Bridge
```

Every packet that flows through is:

1. **Decoded** — LoRaWAN header fields (MType, DevAddr, DevEUI, FCnt, FPort) are extracted without requiring session keys.
2. **Filtered** — configurable rules can block specific packet types in specific directions.
3. **Persisted** — every packet is written to a SQLite database with a configurable retention window.
4. **Broadcast** — every packet is immediately pushed over WebSocket to any connected browser.

A React web application (compiled into the binary) provides a real-time dashboard.

Two gateway protocols are supported:

- **Semtech UDP Packet Forwarder** — the most common open protocol, used by nearly all LoRa gateways.
- **ChirpStack Concentratord** — a ZeroMQ/Protobuf protocol used by ChirpStack-based gateway software.

---

## 2. Repository layout

```
lorawan-packet-sniffer-v2/
├── Cargo.toml                  # Workspace root (no code here, just glue)
├── Cargo.lock                  # Exact dependency versions (committed for binaries)
├── rust-toolchain.toml         # Pins the Rust compiler version
├── Makefile                    # Convenience build/run commands
├── Dockerfile                  # Multi-stage Docker build
├── docker-compose.yml          # Docker Compose for quick deployment
├── sniffer.toml                # Example configuration file
│
├── lorawan-sniffer/            # The single crate — library logic + executable
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs             # Entry point: CLI parsing and logger setup
│       ├── core/               # Protocol-agnostic core logic (no async, no network)
│       │   ├── mod.rs          # Module declarations
│       │   ├── error.rs        # Unified error type
│       │   ├── metadata.rs     # Core data types (MessageRecord, MType, etc.)
│       │   ├── decoder.rs      # LoRaWAN PHY payload decoder
│       │   ├── filter.rs       # Filter rule evaluation engine
│       │   ├── history.rs      # What a history request asks for; label case folding
│       │   ├── storage.rs      # SQLite persistence layer (writer + read-only connection)
│       │   ├── writer.rs       # WriterItem: what the storage queue carries
│       │   ├── random.rs       # Random bytes that can fail: RandomSource, generate_token
│       │   └── toa.rs          # Time-on-Air calculator
│       ├── config/
│       │   └── mod.rs          # Configuration structs and TOML loader
│       ├── cmd/
│       │   ├── mod.rs          # Module re-exports
│       │   ├── root.rs         # Main run loop: spawns all async tasks
│       │   └── configfile.rs   # `configfile` subcommand (prints template, with a generated login token)
│       ├── api/
│       │   ├── mod.rs          # HTTP/WebSocket server (axum), router and layer order
│       │   ├── auth.rs         # Optional WebUI login: sessions, lockout, require_login, login/logout
│       │   └── messages.rs     # GET /api/messages: message history
│       └── proxy/
│           ├── mod.rs          # Shared proxy utilities
│           ├── udp.rs          # Semtech UDP MITM proxy
│           ├── gw.rs           # Protobuf message definitions (hand-written)
│           └── concentratord.rs# ChirpStack Concentratord ZMQ MITM proxy
│
└── webui/                      # React + TypeScript frontend
    ├── package.json
    ├── vite.config.ts
    └── src/
        ├── main.tsx            # React app entry point
        ├── App.tsx             # The login gate, then the sniffer page (SnifferApp)
        ├── session.ts          # Login's WebUI side: checkSession, logIn, logOut, reportUnauthorized
        ├── types/
        │   └── messages.ts     # TypeScript types mirroring MessageRecord
        ├── lib/
        │   └── history.ts      # Pure logic: filter rules, merging, counts, request rules
        ├── components/
        │   ├── MessageTable.tsx # Packet table with virtual scrolling
        │   ├── FilterBar.tsx   # Time range / MType / status / gateway / device controls
        │   ├── ColumnPicker.tsx # ⚙ Settings menu: columns, Export CSV, Log out
        │   ├── LoginPage.tsx   # The token field and Log in button
        │   ├── HistoryNotice.tsx # The strip under the table: cap, errors, database behind
        │   └── StatusBar.tsx   # Connection and message count indicator
        ├── hooks/
        │   ├── useWebSocket.ts # WebSocket connection hook
        │   └── useHistory.ts   # Loads the table from /api/messages and merges live records
        └── test/               # (next to src/) history.test.mjs and history-vectors.json: npm test
```

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

The binary runs four concurrent task groups:

| Task | What it does |
|------|-------------|
| **Proxy** (UDP or ZMQ threads) | Receives packets, decodes them, applies filters, forwards or blocks |
| **Storage writer** | Receives decoded records over an async channel and writes them to SQLite |
| **API server** | Serves HTTP REST + WebSocket + the embedded React SPA |
| **Retention cleanup** | Periodically deletes rows older than the configured TTL |

All tasks share these handles:
- `mpsc::Sender<WriterItem>` — the storage queue (one sender per proxy, one receiver in the writer task). An item (`WriterItem`, in `core/writer.rs`) is a record to insert, or a `Settle` marker that history requests use to wait for the writer.
- `broadcast::Sender<Arc<String>>` — the WebSocket fan-out (one sender, N receivers — one per connected browser)
- `CancellationToken` — a cooperative shutdown signal

---

## 5. Data flow end-to-end

Here is the journey of a single uplink packet from gateway to database:

```
1.  Gateway sends a UDP PUSH_DATA packet to port 1700.

2.  proxy/udp.rs receives it in the tokio event loop (recv_from).

3.  The binary header is parsed:
      [version(1)] [token(2)] [0x00=PUSH_DATA] [gateway_eui(8)] [JSON...]

4.  The JSON body is deserialized into PushDataPayload { rxpk: Vec<RxPk>, ... }.

5.  For each RxPk (one RF packet):
      a. decoder::decode_phy(payload, crc) base64-decodes the PHY payload and
         applies the malformed-frame rules (CRC failed, bad MHDR, wrong
         length). A frame that passes has its LoRaWAN header parsed to
         extract MType, DevAddr, FCnt, FPort; a malformed one keeps only its
         MType, size and reason.
      b. filter::evaluate() checks all configured rules in order.
         First match wins. If no rule matches → Forwarded. A malformed frame
         has no type for the rules, so rules listing MTypes skip it.
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

7.  The storage writer takes one item at a time. For a record it calls
    tokio::task::spawn_blocking(|| storage.insert(&record))
    to run the SQLite write on a thread-pool thread (not blocking the async event loop),
    and waits for it before taking the next item. An insert that fails, or panics,
    is logged and counted. A Settle marker is answered at once: everything queued
    before it has been processed.
```

### Reading history

The other direction is the WebUI asking the database what it holds. `GET /api/messages` accepts a lower bound (`from`), the filters of the WebUI and `count=true`, and answers with the newest N matching records, oldest first, and their exact count. `api/messages.rs` parses the request into a `HistoryQuery`, and `Storage::query_history` runs it on a **second, read-only SQLite connection** (WAL mode only), so a scan of a large range never holds the mutex the writer needs. The README documents the parameters and headers; `.sdd/architecture.md` has the bounds (one query at a time, three response bodies in memory).

The WebUI reloads the selected range every time `/ws` opens, and that reload must not miss a record that was broadcast during the outage, whatever the timing. Four orderings in the code give that guarantee, and none of them is a delay:

1. `Outputs::emit` queues a record for storage **before** it broadcasts it, and looks for listeners only after queueing, so a client that subscribes in between either gets the record live or has it ahead of its marker.
2. `/ws` takes its broadcast subscription in the handler, **before** the `101` answer, so everything broadcast after the browser's `open` event reaches the socket.
3. The browser sends the reload after `open`, with `settle=true`: the handler puts a `WriterItem::Settle` marker in the storage queue and waits (up to 5 s) for the answer.
4. The writer handles the queue in order and answers a marker only after everything before it.

A record saved in the range was therefore queued before the marker, and is in the reload, or was broadcast after the subscription, and reached the browser live. If you change `emit`, the `/ws` handler, the writer loop or the reload, keep all four. "Settled" means processed (saved, or its failure logged and counted in the shared `write_failures` counter), not saved; the counter is reported in `X-Storage-Write-Failures`.

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

`Storage` is cheap to clone because internally it is just `Arc<Mutex<Connection>>` — cloning it just increments a reference counter, it does not copy the database connection.

### 6.2 `Arc<T>` — shared ownership across threads

`Arc` stands for **Atomically Reference Counted**. It is a smart pointer that wraps a value and keeps track of how many places hold a reference to it. When the last `Arc` is dropped, the inner value is freed.

`Arc` is the safe way to share data between threads in Rust. You will see it used in two places:

1. `Arc<Mutex<rusqlite::Connection>>` — the SQLite connection shared between all tasks.
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

Rust encourages message-passing rather than shared mutable state. Two channel types are used:

**`mpsc`** (multiple-producer, single-consumer): Used for the storage queue. Multiple proxy tasks send records; one storage writer task receives them.

```rust
let (tx, rx) = mpsc::channel::<MessageRecord>(1024);
```

**`broadcast`**: Used for WebSocket fan-out. One proxy task sends a JSON string; every connected WebSocket client's task receives a copy.

```rust
let (tx, _) = broadcast::channel::<Arc<String>>(512);
let rx = tx.subscribe(); // each WebSocket client gets its own receiver
```

---

## 7. File-by-file reference — `core/`

The `core/` directory holds the protocol-agnostic logic: no async runtime, no network code, no HTTP. These modules could be tested in complete isolation even if the surrounding binary were removed.

The entry point is `core/mod.rs`, which simply declares the submodules:

```rust
pub mod decoder;
pub mod error;
pub mod eui;
pub mod filter;
pub mod history;
pub mod labels;
pub mod metadata;
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

Three variants belong to history reads: `Cancelled` (the query stopped because whoever asked went away), `HistoryUnavailable` (a row filter was asked of a database that is not in WAL mode) and `Busy` (the reader's 1 s busy timeout ran out). The API turns the last two into `503` answers with a `code`.

**Design note:** The `thiserror` crate (`#[derive(Error)]`) generates the `std::error::Error` trait implementation automatically. The `#[error("...")]` attribute defines the human-readable message. `{0}` refers to the first field of the variant.

`#[from] rusqlite::Error` tells Rust: "if someone applies `?` to a `rusqlite::Error` in a function that returns `Result<_, SnifferError>`, automatically wrap it in `SnifferError::Database`." This means the calling code does not need explicit conversions.

**Why a single error type?** Having one enum with named variants lets callers match on specific error kinds without needing to know about internal dependencies (e.g., rusqlite).

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

**`Status`** — whether the packet was forwarded (`Forwarded`) or dropped by a filter rule (`Blocked`).

**`MessageRecord`** — the canonical, protocol-agnostic struct that represents one captured packet. It is:
- Stored in SQLite (via `storage.rs`)
- Sent over WebSocket as JSON (via `api/mod.rs`)
- Returned by the REST API

Every field has a comment explaining its semantics. `Option<T>` fields are absent in situations where the data is not available (e.g., `rssi` is `None` for downlinks because RSSI is a receive-side measurement).

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

**EUI byte order:** LoRaWAN stores EUIs in little-endian order over the air, but displays them big-endian. The `format_eui_le` function reverses the byte order before formatting.

The file has a comprehensive test suite (`#[cfg(test)] mod tests`) with crafted byte arrays covering every MType.

---

### `core/filter.rs`

The filter engine. Deliberately simple: a linear scan through rules, first match wins.

**`FilterRule`** — a compiled filter rule:
- `enabled: bool` — rules can be present in config but disabled without removal
- `direction: Option<Direction>` — `None` matches both directions
- `mtypes: Vec<MType>` — empty list matches any MType
- `suppress_ack: bool` — stealth mode flag

**`FilterDecision`** — the result of evaluating rules against one packet:
- `blocked_by: Option<String>` — `None` means forwarded; `Some(name)` means blocked by the named rule
- `suppress_ack: bool` — whether to suppress the protocol-level acknowledgement

**`evaluate(rules, direction, mtype, gateway) -> FilterDecision`** — iterates the rules slice. The order in the config file is the order they are evaluated. `mtype` is an `Option<&MType>`: `None` for a malformed frame, whose type is only its first byte read as an MHDR. A rule with a non-empty `mtypes` list never matches `None`; a rule with no list still does. Any change to rule evaluation must keep that.

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

- `HistoryQuery` — an optional lower bound (`LowerBound::After` for the old `since`, exclusive; `LowerBound::From` for `from`, inclusive), a `HistoryFilter`, a `limit` of at most `MAX_HISTORY_ROWS` (5,000), `newest` (keep the newest `limit` rather than the oldest) and `count`.
- `HistoryFilter` — device text, message types, status, gateway and `hide_malformed`. `has_row_filter()` says whether the request asks for anything beyond the time bound; that is what decides whether it may run when there is no read-only connection.
- `HistoryPage` — the records oldest first, the number of matches, and the number of malformed matches (`None` when it was not computed).
- `fold_case` and `devices_with_matching_label` — the label rule. A label is free text, so SQL cannot compare it; it is folded with `str::to_lowercase`, which maps characters the way JavaScript's `toLowerCase` does (`ÑANDÚ` to `ñandú`, the final sigma, `İ`), and matched in memory. The identifiers of the matching labels go to SQL as one JSON array. A label belongs to the record's identifier, the DevAddr if it has one, else the DevEUI.

The browser applies the same rule to live records in `webui/src/lib/history.ts`. `webui/test/history-vectors.json` lists records, labels and cases, and both sides run it.

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

**Schema:** A single `messages` table with one row per captured packet. Enum values are stored as text strings (e.g., `"up"`, `"down"`, `"JoinRequest"`) for readability. Timestamps are RFC 3339 text, which is lexicographically sortable and understood by SQLite's date functions.

Three partial indexes are created for the most common query patterns: by timestamp, by DevAddr, and by DevEUI.

**History reads.** `query_history(&HistoryQuery, cancel)` answers a history request, on the reader when there is one:

- The reader is opened only if `PRAGMA journal_mode` reads back `wal`, read-only and with a 1 s busy timeout. In WAL mode a reader sees a consistent snapshot and neither blocks the writer nor is blocked by it. Without WAL a reader would block every commit and a scan on the writer would block every insert, so there is no reader: a time-only query still runs on the writer, a query with a row filter returns `HistoryUnavailable`, and a WARN at startup says so. `open_in_memory()` (tests only) runs everything on its one connection.
- One deferred read transaction covers the labels, the counts and the rows, so they come from one snapshot. A progress handler reads the `cancel` flag every 10,000 VM steps and interrupts the statement; a guard removes the handler on every exit path, so a cancelled query cannot affect the next one on the connection.
- Every range query names `INDEXED BY idx_messages_timestamp`. A device text cannot use an index, but the scan follows the timestamp index and reads only `rowid` and the filter columns: it counts every match and keeps the first N rowids, then reads those N rows in full. Without the hint, the planner chose `idx_messages_mtype` plus a temporary B-tree sort for a type filter.
- Timestamps are stored as `to_rfc3339()` text (`+00:00` and 0, 3, 6 or 9 fraction digits) and sort as text. Bounds are rendered with the same function. The module's doc comment explains why that orders correctly.
- All `EXPLAIN QUERY PLAN` shapes are checked by tests in `core/storage/history_tests.rs`, which run each case on an in-memory and on a file database. `large_db_history_queries` (`#[ignore]`) builds a database of 1,000,000 records and measures the queries, the writer's lock wait and the log size.

**Blocking calls in async context:** All `Storage` methods (`insert`, `query_history`, etc.) are synchronous — they block the calling thread. The callers in `cmd/root.rs` wrap every call in `tokio::task::spawn_blocking` to avoid blocking the async event loop.

**The session secret.** Migration 4 creates `session_secret`, a table with at most one row (`id` is `CHECK (id = 1)`, `secret` a 32-byte blob). `Storage::session_secret(fresh)` first reads the row with no transaction and no write, which is what every start after the first does and works on a read-only file. Only if there is no row does it `BEGIN IMMEDIATE`, read again (another process may have created it while this one waited for the lock), call `fresh` once and `INSERT`; a lock held past the busy timeout (5 s) is an error, never a secret of its own that the other process does not share. A row that is not 32 bytes is `SecretError::Damaged`, reported and never replaced, because replacing it would log every browser out. No `SecretError` value holds secret bytes. Older versions ignore the table, so their databases stay compatible, but they do not enforce the login.

**`INSERT OR REPLACE`:** If a record with the same UUID is inserted twice (e.g., due to a retry), the second insert replaces the first without error. In practice UUIDs are unique, but the `OR REPLACE` makes the operation idempotent.

---

## 8. File-by-file reference — `lorawan-sniffer`

---

### `lorawan-sniffer/src/main.rs`

The binary entry point. Its responsibilities are minimal:

1. **Parse the CLI** using `clap` — the `#[derive(Parser)]` attribute generates argument parsing from the struct definition.
2. **Load configuration** — calls `config::get()` and exits with a message on error.
3. **Handle the `configfile` subcommand** — prints a template and exits.
4. **Set up logging** — either `simple_logger` (stdout) or `syslog` depending on config.
5. **Delegate to `cmd::root::run()`** — everything else happens there.

**Rust note on `#[macro_use] extern crate log`:** The `log` crate provides macros (`info!`, `warn!`, `error!`, `debug!`, `trace!`) that work like `printf` but route to whatever logging backend is configured at runtime. The `extern crate` syntax is an older Rust idiom; in modern Rust (edition 2018+) it is usually not needed, but the macros still require explicit import via `#[macro_use]`.

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

**Sequence of operations:**

1. Build `Vec<FilterRule>` from `config.filters` (the `build_filters` helper).
2. Open the SQLite database (`Storage::open`).
3. Create the `mpsc` channel (storage queue, carrying `WriterItem`s) and the `broadcast` channel (WebSocket fan-out).
4. Create a `CancellationToken`.
5. `tokio::spawn` the storage writer task (`run_writer`).
6. `tokio::spawn` the API server task (`run_api`).
7. `tokio::spawn` the retention cleanup task (`run_retention`).
8. Run the proxy (UDP or Concentratord) in the main task.
9. On SIGINT (`ctrl_c()`), call `cancel.cancel()` to signal shutdown to all tasks.

**`tokio::select!` for graceful shutdown:**

```rust
tokio::select! {
    result = proxy::udp::run(...) => { result?; }
    _ = tokio::signal::ctrl_c() => {
        cancel.cancel();
    }
}
```

This waits for whichever completes first: the proxy exits normally, or the user presses Ctrl-C. Either way, `cancel.cancel()` propagates the shutdown signal to all other tasks.

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

Uses `tokio::time::interval` to tick every 5 minutes. `MissedTickBehavior::Skip` means if the system is slow and a tick is missed, it is simply skipped — no burst of back-to-back runs.

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

**The login (`api/auth.rs`).** `ApiContext.login` is `Option<Arc<Auth>>`: `cmd/root.rs` builds it (`start_login`) only when `[sniffer.api] login` is true, and the router adds the login routes and the layer only when it is `Some`. With it `None` the router is the one that existed before the login, plus `/api/session`.

- **`Auth`** holds the session key (`session_key(secret, token)` = `HMAC(secret, label ‖ 0x00 ‖ token)` used as an HMAC key), a `Comparer` for tokens, the `RandomSource`, two `RateLimitedLog`s (failed logins; a login whose `Origin` and `Host` differ) and the `Lockout`. It has no `Debug`. `Auth::new` draws a 32-byte per-process key from the random source and keeps the token only as its HMAC under that key; `hmac::verify` is the comparison, so it takes the same time wherever the inputs first differ. `Auth::with_parts` (tests only) swaps the clock and the comparison.
- **A session** is `v1.<unix seconds>.<16 random bytes in hex>.<HMAC-SHA256 in hex>` and nothing is stored per session. `session_valid` parses the public shape first (a wrong shape reveals only that it is wrong), then `hmac::verify`. The cookie is `lorawan_sniffer_session; HttpOnly; SameSite=Strict; Path=/`, with no `Max-Age`, `Expires` or `Secure`; logout sends the same cookie with `Max-Age=0`.
- **`Lockout::attempt(compare)`** is the only way a token is compared (`Auth::check_token` wraps it). One `std::sync::Mutex<Window { start, wrong, warned }>` is held across reading the clock, rolling the window over, checking `wrong >= LIMIT` (10), calling `compare`, and counting a wrong one. A single lock is the point: with check and count in separate steps, many requests that arrive together would all pass the check before any of them added to the count. The window is 60 s and fixed, so up to 20 wrong tokens can be compared across a boundary. `Locked(left)` is answered without comparing, a right token is never counted, a panicking `compare` leaves the count unchanged (the poisoned lock is recovered), and nothing in it `.await`s. The clock is `Clock`/`TokioClock`, so tests with a paused Tokio clock move the window.
- **`credential(headers)`** returns `Bearer`, `Cookie` or `None`: a `Bearer` authorization header wins (and an unusable one, with no token, spaces in it or repeated, is `None` and does not fall back to a cookie); an `Authorization` of any other scheme is ignored, so a proxy's `Basic` does not hide the cookie; else the session cookie.
- **`require_login`** lets `is_public(path)` paths through (anything outside `/ws`, `/ws/…`, `/api/…`, plus `/api/login` and `/api/logout`), refuses no credential and an invalid cookie with a plain `401` (the cookie failure is logged, not counted), and sends a Bearer token through `check_token`: `Right` passes, `Wrong` is `401`, `Locked` is `429` with `Retry-After`. The prefix test is on the path as the router sees it; odd spellings either match and are refused, or reach no data route.
- **`same_host_json`** runs first in `login` and `logout`: exactly one `Content-Type` that `mime` parses as `application/json` with no `+suffix` and well-formed parameters (else `415`), then `origin_allowed(headers, uri, &[])`, an `Origin` with the request's host and port (else `403`; `cors_origins` are not consulted and `X-Forwarded-*` is never read). Values from the client reach the log only through `log_value` (64 characters, then `…`; unprintable bytes shown as `?`).
- **Startup** (`start_login` in `cmd/root.rs`): `Storage::session_secret(|| random_bytes::<32>(..))` reads the stored secret with no write, or creates it in a `BEGIN IMMEDIATE` transaction; then `Auth::new`. Each failure (random source, database, damaged row) is an error that stops startup.
- **Tests.** `api/auth_tests.rs` has the unit tests (sessions, the lockout with a manual clock and on real threads, `credential`, the parsing helpers) and `api/login_tests.rs` the router tests through the built router with `Setup::login(token)`. `crate::test_support` holds the capturing logger that the log test and the MQTT test share, because a process has one global logger.

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

**REST handler (`/api/messages`, in `api/messages.rs`):**

The README lists every parameter, header and status. In outline, `messages_handler` does this:

1. `parse` turns the query string into a `HistoryQuery`. `since` and `limit` keep their old meaning; a bad new parameter is a `400` with `{"error": ...}`. `newest` defaults to `true` unless a lower bound was parsed.
2. For `settle=true` only: it takes `history_settle`, sends a `Settle` marker and waits for the answer, all under one 5 s timeout. A timeout is not an error: the query runs anyway and the answer says `X-History-Settled: false`, with a rate-limited WARN (`Storage writer is behind; history served without waiting for it`, or `Storage writer stopped; …` when the queue is closed). A request without `settle` skips this.
3. It takes a `history_bodies` permit, then the `history` permit, and runs `Storage::query_history` in `spawn_blocking`, with a `CancelOnDrop` flag. If the client disconnects, axum drops the handler, the flag is set, and the scan stops at its next progress tick (DEBUG: `History query cancelled: the client went away`).
4. It answers with the records and the count headers. `HistoryUnavailable` and `Busy` become `503` with `code` `no_wal` and `busy`; anything else is a `500` with no body.

The marker wait comes before the two permits so that requests that settle wait for the writer together, within the same few seconds. If a request took a body place first, only three could wait for the writer at a time, and the fourth to tenth would wait for a place and then for their own marker, in waves of up to 5 s.

A response layer on the route adds `X-Storage-Write-Failures` to every response, including axum's own `Query` rejections, which are produced before the handler runs.

**Embedded frontend (`rust-embed`):**

```rust
#[derive(RustEmbed)]
#[folder = "../webui/dist"]
struct Frontend;
```

The `RustEmbed` derive macro reads all files in `webui/dist/` at **compile time** and includes them as byte arrays in the binary. At runtime, `Frontend::get("index.html")` retrieves the file from memory. No file system access at runtime — the binary is self-contained.

The `frontend_handler` implements SPA routing: if the requested path is not found as a static file, it falls back to `index.html`. This allows the React router to handle client-side navigation.

**CORS:** If `cors_origins` is empty the API sends no CORS headers, so only the sniffer's own pages can use it from a browser. Listed origins get CORS for `GET`, `PUT`, `DELETE` and `Content-Type`, and the four history headers are exposed to them (`expose_headers`), since a script cannot read a header that is not exposed.

---

### `lorawan-sniffer/src/proxy/mod.rs`

Shared utilities used by both proxy backends. These are declared `pub(super)` — visible to sibling modules (`udp.rs`, `concentratord.rs`) but not outside the `proxy` module.

**`frame_fields`:** Converts an `Option<DecodedFrame>` and the CRC status into a flat `FrameFields`, filling fields with `None` / `MType::Unknown` if decoding failed. `FrameFields::rule_mtype()` is the type the filter sees: `None` for a malformed frame.

**`classify`:** Decides forward / block / capture for a message from the filters, the direction, the type (`None` for a malformed frame) and the gateway. The returned `Verdict` gives the `Status` and the name of the rule that blocked it, for `MessageRecord`.

**`Outputs::emit`:** The one place a record leaves the proxy. It logs the record, then does only non-blocking hand-offs:

1. queues the record for MQTT with `try_send`, if MQTT is on;
2. queues a `WriterItem::Record` for storage with `try_send`;
3. serialises the `/ws` JSON, but only if a WebSocket client is subscribed (`receiver_count() > 0`), and broadcasts it. The check comes after the storage queue on purpose: a subscriber that arrives between the two would otherwise get neither the broadcast nor a place ahead of its marker.

If a queue is full (which should never happen at LoRaWAN data rates) the record is dropped for that output and a rate-limited warning is logged. Using `try_send` instead of `.await send` means the proxy loop never stalls waiting for the storage writer. Storage is queued before the broadcast so that any record a browser could have seen live is already ahead of a later `Settle` marker; see "Reading history" in section 5.

**`log_message`:** Formats a compact one-line log entry:
```
[FORWARDED] UP   UnconfirmedDataUp 01ABCDEF fcnt=1234 fport=1  868.100MHz SF7BW125 size=23 toa=62ms rssi=-55 snr=8.0
```

---

### `lorawan-sniffer/src/proxy/udp.rs`

The Semtech UDP Packet Forwarder MITM proxy. This is the most complex file in the binary.

**Protocol overview:** The Semtech protocol uses a 4-byte header on every UDP datagram:
```
[protocol_version(1)] [token(2)] [message_type(1)] [... body ...]
```

The 2-byte token is a random number chosen by the sender. The receiver echoes it back in the acknowledgement so the sender can match responses to requests.

**Two UDP sockets:**
- `gw_sock` (`downstream`) — bound to port 1700 (or configured address). The gateway sends to this port.
- `lns_sock` (`upstream`) — an ephemeral port, connected to the real LNS. The sniffer forwards to the LNS here.

The naming convention follows standard networking: *downstream* is towards the gateway (the client), *upstream* is towards the LNS (the server).

**Token routing:** PUSH_DATA and PULL_DATA include the token in the request and expect the matching ACK from the LNS. The sniffer maintains a `HashMap<[u8; 2], SocketAddr>` mapping tokens to the gateway address that sent them, so ACKs can be routed back correctly.

**PULL_RESP special case:** Downlink packets arrive as PULL_RESP. Per the Semtech spec, PULL_RESP uses a fresh random token. It cannot be routed via the token map. Instead, the sniffer tracks the most recent gateway address from the last PULL_DATA (`gw_pull_addr`) and routes PULL_RESP there. This works for single-gateway deployments.

**Blocking behaviour:**

| Scenario | Action |
|----------|--------|
| Uplink blocked, stealth off | Drop PUSH_DATA to LNS, send synthetic PUSH_ACK to gateway |
| Uplink blocked, stealth on | Drop PUSH_DATA, no PUSH_ACK |
| Downlink blocked, stealth off | Drop PULL_RESP to gateway, send synthetic TX_ACK to LNS |
| Downlink blocked, stealth on | Drop PULL_RESP, no TX_ACK |

"Non-stealth" ACKs keep the gateway and LNS from retrying indefinitely, creating a cleaner interception. "Stealth" mode causes timeouts on both ends, which may be detectable but avoids revealing the interception.

**`process_push_data`:** A single PUSH_DATA can contain multiple `rxpk` entries (if multiple gateways hear the same uplink and the packet forwarder batches them). The function processes all entries. If **any** is blocked, the entire PUSH_DATA is withheld. ACK is suppressed only if **all** blocked entries request stealth.

**`build_uplink_record` / `build_downlink_record`:** Return `(MessageRecord, FilterDecision)` as a tuple. Returning the decision alongside the record avoids calling `filter::evaluate()` twice — the decision is needed both to set `Status` inside the record and to determine whether to send an ACK to the gateway.

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
cancel.cancelled().await;  // tokio side: wait for SIGINT
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

**ZMQ topology:**

```
Event channel (PUB/SUB):
  concentratord  ─[PUB]──► sniffer SUB ─► decode+filter ─► sniffer PUB ─► gateway bridge

Command channel (REQ/REP):
  gateway bridge ─[REQ]──► sniffer REP ─► decode+filter ─► sniffer REQ ─► concentratord
                                                          ◄─ response ◄──
```

**REQ/REP constraint for stealth mode:** The ZMQ REQ/REP pattern requires that every request receives exactly one reply. The bridge blocks waiting for a reply. This means the concentratord proxy cannot simply discard a downlink command — it must send *something* back. When a downlink is blocked, the sniffer sends a synthetic `DownlinkTxAck` with `IGNORED` status. The bridge receives a well-formed reply but the concentratord never transmits the packet.

**Joining the threads:** On shutdown, the tokio runtime waits for both threads to finish by joining them inside a `spawn_blocking` call:

```rust
tokio::task::spawn_blocking(move || {
    event_handle.join().ok();
    cmd_handle.join().ok();
}).await?;
```

This is necessary because `JoinHandle::join()` is a blocking call (it waits for the thread to exit). Calling it directly in async code would block the event loop.

---

## 9. File-by-file reference — `webui`

The WebUI is a standard React + TypeScript SPA built with Vite. It communicates with the backend via:
- **WebSocket** at `/ws` — real-time live feed, and `/ws/labels` for device labels
- **REST GET `/api/messages`** — the table's history: on page load, on every change of the time range or a filter, and on every `/ws` open. The server decides what a range and a filter contain; the WebUI replaces its rows with the newest 5,000 matching records and merges live records into them.

### `webui/src/types/messages.ts`

TypeScript interfaces that mirror the Rust `MessageRecord` struct and its enums. These are the types used throughout the frontend. Keeping them in one file makes it easy to update when the Rust types change. `deviceId()` now lives in `lib/history.ts` and is re-exported from here.

### `webui/src/lib/history.ts`

The history logic as plain functions: no React, no timers, no `fetch`. It may only use `import type`, because `npm test` transpiles this one file and loads it on its own.

- **The filter rules.** `matchesFilter` says whether a record belongs in the table (device text against the DevAddr, the DevEUI and the label; type; status; gateway; time; malformed). `historyParams` builds the query string that asks the server for the same thing, with every value percent-encoded. `toHistoryFilter` turns the filter bar's state and a clock reading into a lower bound. These must agree with `core/history.rs` and `Storage::query_history`.
- **Merging.** `mergeRecords` de-duplicates by `id`, keeps the newest 10,000 and lets the loaded copy win. `mergeAfterLoad` builds the table from a load's answer plus the live records it lacks, by membership in the answer and never by comparing timestamps.
- **Counts.** `olderAfterLoad`, `matchingCount` and `malformedAfterLoad` give the footer's numbers from the response headers and the live records.
- **The request state machine.** `nextRequests(state, event, now)` takes an event (filter change, typing, labels, socket open, response, failure, pause, resume) and returns the next state and the actions to run (send a load, abort, start or cancel a timer, apply, hold). One generation counter covers every load; an answer from an older generation is dropped.
- **Back-off.** `nextReconnect` gives the reconnect delay and `nextRetryDelay` the retry delay, both jittered. Feed `nextReconnect` the previous *nominal* delay, not the jittered one, or the series drifts.

### `webui/src/hooks/useHistory.ts`

Runs what `nextRequests` decides: timers, one `AbortController` per request, a 30 s timeout, the retry timers, and `fetch` with the headers read back into counts. It keeps the loaded rows, a log of the live records received since each load was sent (the carry-over needs them), and the pause buffer. It returns the rows, the counts, `busy`, `behind`, `error` and `writeFailures`, and `pause()` / `resume()`.

### `webui/src/hooks/useWebSocket.ts`

A React hook that manages one WebSocket connection (connect, receive, reconnect after a drop). It hands each parsed frame to a callback and calls `onOpen` every time the socket opens, which is how `/ws` triggers a reload of the range. Reconnect waits grow from about 3 s to 60 s and are randomised; a connection that lasted a minute resets them. A browser cannot read why a handshake was refused, so on every close the hook first calls `checkSession()`: `login` means it reports the end of the session and stops; anything else, `unreachable` included, reconnects as before (a close with code 1013 for falling behind is not a session end).

### `webui/src/session.ts`

The login's side in the browser, with no React: `checkSession()` asks `GET /api/session` and answers `open` (login off), `logged-in`, `login` (a 401) or `unreachable` (anything else, a network error included); `logIn(token)` and `logOut()` `POST` JSON with `Content-Type: application/json`, which the server insists on for both; and a small bus, `onUnauthorized(listener)` and `reportUnauthorized()`, that carries "the session is gone" from wherever a 401 is seen (a history load, a label save, a refused WebSocket) to the gate in `App.tsx`. Only relative URLs, and the cookie is never read or stored by script.

### `webui/src/App.tsx`

The root component, in two layers. `App` is the gate: it asks `checkSession()` on load and shows "Connecting…", "Cannot reach the sniffer. Retrying…" (asking again every 3 s), `LoginPage`, or `SnifferApp`; a call to `reportUnauthorized()` sends it back to the login, which removes `SnifferApp` and its sockets. `SnifferApp` is the page itself. Connects the hooks to the display components: `useHistory` for the rows, `useLabels` for the labels, `useWebSocket` for the live feed. It filters the held rows with `matchesFilter` at render time, on a 5 s tick so that rows leave the range without new traffic, and works out Matching, Shown and Blocked.

### `webui/src/components/MessageTable.tsx`

The main data grid. Displays one row per `MessageRecord`. Implements virtual scrolling (only renders visible rows) for performance with large message counts. Each row has `data-id` with the record's `id`.

### `webui/src/components/FilterBar.tsx`

Filter controls (time range, MType, status, gateway, device, and the "Show malformed" checkbox). They set what the WebUI asks `/api/messages` for, and they filter the live records; they do not affect what the sniffer captures or stores.

### `webui/src/components/ColumnPicker.tsx`

The **⚙ Settings** popover: it shows or hides individual columns, then, after a separator, holds the actions, which close it when chosen: **Export CSV** (the rows shown; `App` passes `onExport` and the row count) and, when the login is on, **Log out** (`onLogout`). The header has no room for buttons of their own at 800 px.

### `webui/src/components/LoginPage.tsx`

The login page: a password field, **Log in**, Enter submits. It does not send a blank field, trims the token, ignores a second submit while one is out (every wrong token counts against everyone's attempts), keeps the text after an error, and shows the server's message (wrong token, the lockout, a proxy that changed `Host`) or, when the sniffer accepted the token but a new check still finds no session, a line about allowing cookies.

### `webui/src/components/HistoryNotice.tsx`

The one-line strip between the table and the footer. It joins, in this order, the messages that apply: a failed load, the count of records the database failed to store, "the database is behind", and the cap ("Showing the newest 5,000 of N matching messages in …"). It renders nothing when there is nothing to say.

### `webui/src/components/StatusBar.tsx`

Displays Matching, Shown, Blocked and the malformed count, the WebSocket connection state, and the live/pause toggle. Matching reads "Loading…" while a load is waiting or in flight.

### `webui/test/`

`history.test.mjs` is the test behind `npm test`: a plain Node script that transpiles `lib/history.ts` with the `typescript` package the project already has. It covers the request state machine, merging, counts and back-off, and the cases of `history-vectors.json`, which the Rust side also reads (see "Modifying a history filter" below).

---

## 10. Build and toolchain files

### `rust-toolchain.toml`

Pins the entire team to the same Rust version:

```toml
[toolchain]
  channel = "stable"
  components = ["rustfmt", "clippy"]
  targets = ["x86_64-unknown-linux-musl"]
```

When anyone runs `cargo build`, `rustup` automatically downloads and uses the toolchain specified here. The `x86_64-unknown-linux-musl` target is pre-installed so Docker cross-compilation works without extra setup.

### `Cargo.lock`

Records the exact version of every dependency (including transitive dependencies) used in the last successful build. Committed to version control for binaries (unlike libraries, which omit `Cargo.lock`). This ensures reproducible builds — `cargo build` on any machine produces the same binary.

### `Dockerfile`

A three-stage build:

1. **`webui` stage** (`node:20-alpine`): Runs `npm ci` + `npm run build`. Produces `webui/dist/`.
2. **`builder` stage** (`ghcr.io/rust-cross/rust-musl-cross:x86_64-musl`): Compiles the Rust binary targeting `x86_64-unknown-linux-musl` (static musl binary, no glibc dependency).
3. **`alpine:3.20`**: Copies only the compiled binary. Final image is ~25 MB.

**Why `rust-musl-cross` and not `rust:latest`?** Two problems arise with the standard Rust Docker image:
- The `musl` target has `dynamic-linking=false`, which means proc-macro crates (like `async-trait`) cannot be compiled because they produce `.so` shared libraries. The `rust-musl-cross` image uses a glibc host for proc-macro compilation while targeting musl for the final binary.
- ZeroMQ is compiled from C++ source. Debian's native `g++` emits references to glibc-internal symbols (`__libc_single_threaded`, etc.) that do not exist in musl. `rust-musl-cross` provides `x86_64-linux-musl-g++` — a C++ cross-compiler that targets musl and avoids these symbols.

**Dependency caching layer:** The Dockerfile copies only `Cargo.toml`/`Cargo.lock` first, compiles stubs (with `|| true` to ignore expected errors), then copies the real source. Docker caches the dependency compilation layer; it is only invalidated when `Cargo.toml` or `Cargo.lock` changes, not when application source changes.

### `Makefile`

Convenience targets. Key ones:

| Target | What it runs |
|--------|-------------|
| `make build` | `cargo build` (debug, native host) |
| `make build-full` | Rebuild WebUI, then `cargo build` |
| `make run` | `cargo run -- -c sniffer.toml` |
| `make webui-dev` | Vite dev server with proxy to localhost:8888 |
| `make webui-test` | `npm test` in `webui/`: the history logic and the shared filter vectors |
| `make test` | `make webui-test`, then `cargo test` |
| `make build-all` | Cross-compile for x86\_64 and aarch64 musl |
| `make docker-build` | `docker build` for local testing |

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

---

## 12. Common maintenance tasks

### Adding a new field to `MessageRecord`

1. Add the field to `lorawan-sniffer/src/core/metadata.rs` (`MessageRecord` struct).
2. Add the corresponding column to `INIT_SQL` in `lorawan-sniffer/src/core/storage.rs`.
3. Add the column to `SELECT_COLS` and `INSERT_SQL`.
4. Update `row_to_record` to read the new column.
5. Update `insert` to write the new field.
6. Populate the field in `proxy/udp.rs` and `proxy/concentratord.rs`.
7. Add the field to the TypeScript type in `webui/src/types/messages.ts`.
8. Display the field in `webui/src/components/MessageTable.tsx`.

**Important:** The SQLite schema uses `CREATE TABLE IF NOT EXISTS`, so the new column will NOT be added to existing databases automatically. For an existing database, run: `ALTER TABLE messages ADD COLUMN <name> <type>;`

### Adding a new filter field

1. Add the field to `FilterConfig` in `config/mod.rs`.
2. Add the field to `FilterRule` in `lorawan-sniffer/src/core/filter.rs`.
3. Update `FilterRule::matches()` to check the new field.
4. Update `build_filters()` in `cmd/root.rs` to map the config field.
5. Add documentation to the configfile template in `cmd/configfile.rs`.

### Adding support for a new gateway protocol

1. Create `lorawan-sniffer/src/proxy/newprotocol.rs`.
2. Implement a `pub async fn run(config, filters, outputs, cancel) -> Result<()>`.
3. Use `super::{frame_fields, classify, log_message, Outputs}`: `frame_fields` decodes a frame (pass the CRC status for an uplink, `None` for a downlink), `classify` applies the rules to `FrameFields::rule_mtype()`, and `Outputs::emit` sends the record to storage, `/ws` and MQTT.
4. Add `Protocol::NewProtocol` to `config/mod.rs`.
5. Add a match arm in `cmd/root.rs`.
6. Declare the module in `proxy/mod.rs`.

### Modifying a history filter

The rule that decides whether a record matches the device text, type, status, gateway, malformed flag or time range exists twice: in `core/history.rs` and `Storage::query_history` (stored records) and in `webui/src/lib/history.ts` (live records, and the query string). Change both, and add cases to `webui/test/history-vectors.json`. The Node test checks the query string the browser builds and the records its rule keeps; a Rust test sends each case's query through the real `/api/messages` router and compares the ids and `X-Total-Count`. Neither side may let a record through that the other would filter out.

### Running tests

```bash
cargo test                          # all tests
cargo test core::                   # only core/ module tests
cargo test decode_join_request      # tests matching this name
cargo test -- --nocapture           # show println! output during tests
cd webui && npm test                # the WebUI's history logic and the shared vectors
make test                           # npm test, then cargo test
```

Most `core::storage` tests use in-memory SQLite (`Storage::open_in_memory()`) — no file system setup required. The history tests run each case on an in-memory database and on a file database (`TempDb`), since only the file one has the read-only connection.

`large_db_history_queries` is ignored by default. It builds a database of 1,000,000 records and measures the history queries, the writer's lock wait and the size of the log file; run it in release mode on a real disk (`TMPDIR=/path/on/disk cargo test --release -- --ignored large_db_history --nocapture`). `HISTORY_ROWS` and `HISTORY_DAYS` change the size.

### Updating a dependency

1. Edit the version in the root `Cargo.toml` `[workspace.dependencies]` section.
2. Run `cargo update -p <crate-name>` to update `Cargo.lock`.
3. Run `cargo test` and `cargo clippy` to verify nothing broke.

### Modifying the WebSocket message format

The format is defined in `api/mod.rs`:

```rust
struct Envelope<'a> {
    #[serde(rename = "type")]
    kind: &'static str,   // always "message"
    data: &'a MessageRecord,
}
```

If you change this structure, update the corresponding TypeScript parsing in `webui/src/hooks/useWebSocket.ts`.
