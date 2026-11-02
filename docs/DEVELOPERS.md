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
│       │   ├── storage.rs      # SQLite persistence layer
│       │   └── toa.rs          # Time-on-Air calculator
│       ├── config/
│       │   └── mod.rs          # Configuration structs and TOML loader
│       ├── cmd/
│       │   ├── mod.rs          # Module re-exports
│       │   ├── root.rs         # Main run loop: spawns all async tasks
│       │   └── configfile.rs   # `configfile` subcommand (prints template)
│       ├── api/
│       │   └── mod.rs          # HTTP/WebSocket server (axum)
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
        ├── App.tsx             # Root component + WebSocket connection
        ├── types/
        │   └── messages.ts     # TypeScript types mirroring MessageRecord
        ├── components/
        │   ├── MessageTable.tsx # Packet table with virtual scrolling
        │   ├── FilterBar.tsx   # Direction / MType / status filter controls
        │   ├── ColumnPicker.tsx # Show/hide column selector
        │   └── StatusBar.tsx   # Connection and message count indicator
        └── hooks/
            └── useWebSocket.ts # WebSocket connection hook
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

All tasks share three handles:
- `mpsc::Sender<MessageRecord>` — the storage channel (one sender per proxy, one receiver in the writer task)
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
      e. The record is serialized to JSON and sent on the broadcast channel
         (Outputs::emit, which also queues it for storage and MQTT)
         → API server → all WebSocket clients.
      f. The same call sends it on the mpsc channel → storage writer.

6.  If any rxpk was blocked, the PUSH_DATA is NOT forwarded to the LNS.
    Instead, a synthetic PUSH_ACK is sent back to the gateway so it
    does not retry (unless suppress_ack = true, stealth mode).

7.  The storage writer receives the record and calls
    tokio::task::spawn_blocking(|| storage.insert(&record))
    to run the SQLite write on a thread-pool thread (not blocking the async event loop).
```

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
pub mod filter;
pub mod metadata;
pub mod storage;
pub mod toa;
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

### `core/storage.rs`

The SQLite persistence layer.

**The central design:** `Storage` wraps a `rusqlite::Connection` inside `Arc<Mutex<Connection>>`:

```rust
pub struct Storage {
    conn: Arc<Mutex<Connection>>,
}
```

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

**Schema:** A single `messages` table with one row per captured packet. Enum values are stored as text strings (e.g., `"up"`, `"down"`, `"JoinRequest"`) for readability. Timestamps are RFC 3339 text, which is lexicographically sortable and understood by SQLite's date functions.

Three partial indexes are created for the most common query patterns: by timestamp, by DevAddr, and by DevEUI.

**Blocking calls in async context:** All `Storage` methods (`insert`, `query_recent`, etc.) are synchronous — they block the calling thread. The callers in `cmd/root.rs` wrap every call in `tokio::task::spawn_blocking` to avoid blocking the async event loop.

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

---

### `lorawan-sniffer/src/cmd/root.rs`

The application's main orchestration layer. Called by `main.rs` after logging is set up.

**Sequence of operations:**

1. Build `Vec<FilterRule>` from `config.filters` (the `build_filters` helper).
2. Open the SQLite database (`Storage::open`).
3. Create the `mpsc` channel (storage queue) and `broadcast` channel (WebSocket fan-out).
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
loop {
    tokio::select! {
        Some(record) = rx.recv() => {
            let s = storage.clone();
            tokio::task::spawn_blocking(move || s.insert(&record)).await
        }
        _ = cancel.cancelled() => {
            // drain remaining records before exit
            while let Ok(record) = rx.try_recv() { ... }
            break;
        }
    }
}
```

On cancellation, the writer drains any records still in the channel before stopping. This ensures no records are lost on clean shutdown.

**Retention task (`run_retention`):**

Uses `tokio::time::interval` to tick every 5 minutes. `MissedTickBehavior::Skip` means if the system is slow and a tick is missed, it is simply skipped — no burst of back-to-back runs.

---

### `lorawan-sniffer/src/api/mod.rs`

The HTTP/WebSocket server, built on the `axum` web framework.

**Three routes:**

| Route | Handler | Description |
|-------|---------|-------------|
| `GET /ws` | `ws_handler` | WebSocket upgrade for live feed |
| `GET /api/messages` | `messages_handler` | REST query for historical messages |
| `GET /*` | `frontend_handler` | Serve the embedded React SPA |

**`AppState`:** axum passes shared state to every handler via the `State` extractor. `AppState` holds the broadcast sender (to subscribe new WebSocket clients) and the `Storage` handle (to query history).

```rust
#[derive(Clone)]
pub struct AppState {
    broadcast_tx: broadcast::Sender<Arc<String>>,
    storage: Storage,
}
```

Both fields are cheap to clone — `broadcast::Sender` is already internally reference-counted, and `Storage` is `Arc<Mutex<...>>`.

**WebSocket handler:**

Each WebSocket connection gets its own `broadcast::Receiver` (via `broadcast_tx.subscribe()`). The handler loops in a `tokio::select!`, waiting for either a new message on the broadcast channel or a close frame from the client. If the client lags too far behind (> 512 messages buffered), `Lagged` is returned and a warning is logged — the client misses those messages but the connection continues.

The JSON format sent to clients:
```json
{ "type": "message", "data": { ...MessageRecord fields... } }
```

**REST handler (`/api/messages`):**

Accepts two optional query parameters:
- `since`: ISO-8601 timestamp. If provided, returns messages after that time (ascending). Used for incremental polling.
- `limit`: max rows (default 500, capped at 5000).

The handler calls `storage.query_*()` inside `spawn_blocking` (because `Storage` is synchronous) and returns the result as a JSON array.

**Embedded frontend (`rust-embed`):**

```rust
#[derive(RustEmbed)]
#[folder = "../webui/dist"]
struct Frontend;
```

The `RustEmbed` derive macro reads all files in `webui/dist/` at **compile time** and includes them as byte arrays in the binary. At runtime, `Frontend::get("index.html")` retrieves the file from memory. No file system access at runtime — the binary is self-contained.

The `frontend_handler` implements SPA routing: if the requested path is not found as a static file, it falls back to `index.html`. This allows the React router to handle client-side navigation.

**CORS:** If `cors_origins` is empty, `CorsLayer::permissive()` allows all origins (useful during development). If origins are listed, only those are allowed.

---

### `lorawan-sniffer/src/proxy/mod.rs`

Shared utilities used by both proxy backends. These are declared `pub(super)` — visible to sibling modules (`udp.rs`, `concentratord.rs`) but not outside the `proxy` module.

**`frame_fields`:** Converts an `Option<DecodedFrame>` and the CRC status into a flat `FrameFields`, filling fields with `None` / `MType::Unknown` if decoding failed. `FrameFields::rule_mtype()` is the type the filter sees: `None` for a malformed frame.

**`decision_to_status`:** Converts a `FilterDecision` into `(Status, Option<String>)` for storing in `MessageRecord`.

**`send_record`:** Tries to send a record to the storage channel using `try_send` (non-blocking). If the channel is full (which should never happen at LoRaWAN data rates), the record is dropped and a warning is logged. Using `try_send` instead of `.await send` means the proxy loop never stalls waiting for the storage writer.

**`broadcast_record`:** Skips serialisation entirely if there are no WebSocket subscribers (`receiver_count() == 0`). A common optimisation when no browser is connected.

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
- **WebSocket** at `/ws` — real-time live feed
- **REST GET `/api/messages`** — historical data on initial load

### `webui/src/types/messages.ts`

TypeScript interfaces that mirror the Rust `MessageRecord` struct and its enums. These are the types used throughout the frontend. Keeping them in one file makes it easy to update when the Rust types change.

### `webui/src/hooks/useWebSocket.ts`

A React hook that manages the WebSocket connection lifecycle (connect, receive, auto-reconnect on disconnect). Returns the list of received messages and a connection status indicator.

### `webui/src/App.tsx`

The root component. Owns the message list state and connects the WebSocket hook to the display components.

### `webui/src/components/MessageTable.tsx`

The main data grid. Displays one row per `MessageRecord`. Implements virtual scrolling (only renders visible rows) for performance with large message counts.

### `webui/src/components/FilterBar.tsx`

Client-side filter controls (direction, MType, status). Filters apply in the browser — they do not affect what the sniffer captures or stores.

### `webui/src/components/ColumnPicker.tsx`

A popover that lets the user show or hide individual columns.

### `webui/src/components/StatusBar.tsx`

Displays the WebSocket connection state (connected/disconnected) and the total message count.

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

### Running tests

```bash
cargo test                          # all tests
cargo test core::                   # only core/ module tests
cargo test decode_join_request      # tests matching this name
cargo test -- --nocapture           # show println! output during tests
```

All `core::storage` tests use in-memory SQLite (`Storage::open_in_memory()`) — no file system setup required.

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
