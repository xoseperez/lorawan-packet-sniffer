# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Overview

LoRaWAN Packet Sniffer v2 is a transparent MITM proxy for LoRaWAN gateways, written in Rust. It sits between a gateway and a LoRaWAN Network Server (LNS), capturing and optionally blocking messages in real time, with a React WebUI served from the same binary.

## Common Commands

```bash
# Full build (WebUI + Rust binary)
make build-full

# Rust-only debug build (skips WebUI rebuild)
make build

# Run with example config
make run

# WebUI development server (proxies /ws and /api to localhost:8888)
make webui-dev

# Run tests
cargo test

# Run a single test
cargo test <test_name>

# Lint
cargo clippy
cargo fmt

# Cross-compile release binaries (requires `cross`)
make build-all                          # both x86_64 and aarch64 musl
make build-x86_64-unknown-linux-musl
make build-aarch64-unknown-linux-musl

# Docker
make docker-build
DOCKER_IMAGE=ghcr.io/yourname/lorawan-sniffer DOCKER_TAG=latest make docker-buildx
```

## Workspace Structure

This is a Cargo workspace with a single crate:

- **`lorawan-sniffer/`** — the binary, organised internally into:
  - **`src/core/`** — protocol-agnostic logic: LoRaWAN PHY decoder, filter engine, SQLite storage, metadata types, ToA calculator, device-label validation (`labels.rs`) and the live-identifier list (`seen.rs`).
  - **`src/api/`** — HTTP/WebSocket server (axum): Host check, label endpoints and the `LabelWriter` task.
  - **`src/proxy/`** — UDP and Concentratord MITM backends.
  - **`src/cmd/`** — run loop and `configfile` subcommand.
  - **`src/config/`** — TOML configuration structs and loader.
- **`webui/`** — React 19 + TypeScript SPA (Vite). Built output at `webui/dist/` is embedded into the Rust binary at compile time via `rust-embed`.

## Architecture

```
Gateway ──UDP/ZMQ──▶  Sniffer proxy  ──UDP/ZMQ──▶  LNS / Gateway Bridge
                           │
                     decode + filter
                           │
                   ┌───────┴────────┐
                   ▼                ▼
               SQLite DB       WebSocket
             (persistence)    (live feed)
                                   │
                               React WebUI
```

### Data flow

1. A proxy backend (`proxy/udp.rs` or `proxy/concentratord.rs`) receives raw packets.
2. Each packet is decoded by `core::decoder` into a `DecodedFrame` (MType, DevAddr/DevEUI, FCnt, FPort).
3. `core::filter::evaluate()` checks compiled `FilterRule`s — first match wins; no match = forward.
4. Results are assembled into a `MessageRecord` (the canonical cross-protocol representation defined in `core::metadata`).
5. The record is sent to two async channels: `mpsc` → `Storage` (SQLite persistence) and `broadcast` → all connected WebSocket clients.
6. The axum API server (`src/api/mod.rs`) handles `/ws` (live feed), `/ws/labels` (device labels: full set on connect, then one event per change), `/api/messages` (REST history), `/api/labels` (`GET`, `PUT`/`DELETE` on `/api/labels/{device}`), and `/*` (embedded React SPA). A Host check (`src/api/host.rs`, `[sniffer.api] allowed_hosts`) is the outermost layer for every request.
7. A listener on the same `broadcast` channel notes which DevAddr/DevEUI values were seen live (`core/seen.rs`), so rows that never reached the database can still be labelled.

### Proxy backends

- **UDP** (`proxy/udp.rs`): Implements the Semtech UDP Packet Forwarder protocol (PUSH_DATA, PULL_DATA, PULL_RESP, etc.). Runs entirely in async tokio tasks.
- **Concentratord** (`proxy/concentratord.rs`): Implements ChirpStack Concentratord via ZeroMQ IPC + Protobuf. ZMQ calls are dispatched to `spawn_blocking` because the `zmq` crate is synchronous.

### Key types (`core/`)

- `MessageRecord` — the normalized struct stored, broadcast, and returned by the REST API.
- `MType` — LoRaWAN message type decoded from MHDR bits [7:5].
- `FilterRule` / `FilterDecision` — compiled filter rule and its evaluation result.
- `Storage` — thin `Clone`able wrapper around an `Arc<Mutex<rusqlite::Connection>>`.
- Device labels live outside `MessageRecord`: the `device_labels` table is keyed by identifier (`dev_addr`, else `dev_eui`, in the record's text form), and the WebUI joins labels to rows by that key. Labels never touch the forwarding path. Every label change goes through the `LabelWriter` task in `api/labels.rs`, which commits and then publishes on `/ws/labels`.

## WebUI Development

The Vite dev server proxies `/ws` (which also covers `/ws/labels`) and `/api` to `localhost:8888`, so you can run the binary with a real config and iterate on the frontend independently. The Host the sniffer sees is `localhost:5173`, which passes the Host check:

```bash
# Terminal 1: run the sniffer
make run

# Terminal 2: start Vite dev server with hot reload
make webui-dev
```

After any frontend change that needs to be embedded in the binary, run `make build-full`.

### WebUI Component Structure

| File | Role |
|---|---|
| `src/App.tsx` | Layout shell; localStorage hooks; filtering/sorting logic (the device filter also matches labels); label dialog state; CSV export (with the last `label` column) |
| `src/components/MessageTable.tsx` | `TableVirtuoso` table; signal-quality coloring; MType badges; Label column (`LabelCell`, clickable only on rows that have an identifier) |
| `src/components/LabelDialog.tsx` | Modal to set, change or remove the label of one identifier (DevAddr or DevEUI): text field limited to 64 characters, Save / Delete (only when a label exists) / Cancel, Enter saves, Escape or a click outside cancels, shows the server's error |
| `src/components/FilterBar.tsx` | Device input (also matches labels), MType multi-select, status/gateway/time selects |
| `src/components/ColumnPicker.tsx` | Column visibility toggle (persisted to `localStorage`); the Label column is hidden by default |
| `src/components/StatusBar.tsx` | Row counts, WS status indicator, version and copyright notice (version from `package.json` via Vite `define`), live/pause toggle |
| `src/hooks/useWebSocket.ts` | WebSocket connection with auto-reconnect, generic over the path and the event handler (`/ws` for messages, `/ws/labels` for labels) |
| `src/hooks/useLabels.ts` | Label state (`Record<identifier, label>`), fed only by `/ws/labels` (a snapshot replaces it, an event updates one entry); `saveLabel` / `removeLabel` call `PUT` / `DELETE /api/labels/{device}` and return success or the API's error message, never writing state themselves, so a late HTTP response cannot overwrite a newer event |
| `src/styles/index.css` | **All styling** — pure CSS, no framework |
| `src/types/messages.ts` | TypeScript types (`MessageRecord`, `WsMessage`, label snapshot and event types, etc.) and `deviceId()`, the identifier of a row (`dev_addr` ?? `dev_eui` ?? none) |

### CSS Design System

All theming is done via CSS custom properties in `index.css`. Key variables:

- `--dir-up` / `--dir-down` — uplink (blue) and downlink (amber) direction colors
- `--mtype-{join,up,down,other}-{bg,fg}` — MType badge palette (join=purple, up=blue, down=amber)
- `--blocked-bg` / `--blocked-border` — blocked row tint and left-accent color
- `--signal-good` / `--signal-fair` / `--signal-poor` — RSSI/SNR quality colors
- `--row-alt` — zebra-stripe even-row background
- `--font-mono` — monospace stack for all data cells

Dark mode is handled entirely via `@media (prefers-color-scheme: dark)` overriding the same variables.

### Signal Quality Coloring

**RSSI** (`rssiClass` in `MessageTable.tsx`):
- ≥ −100 dBm → `signal-good` (green)
- ≥ −110 dBm → `signal-fair` (amber)
- < −110 dBm → `signal-poor` (red)

**SNR** (`snrClass` in `MessageTable.tsx`) — thresholds are **spreading-factor-aware**:
- SF minimum decodable SNR: SF7 = −7.5 dB, SF8 = −10, SF9 = −12.5, SF10 = −15, SF11 = −17.5, SF12 = −20 dB
- Margin ≥ 10 dB above floor → `signal-good`
- Margin ≥ 3 dB above floor → `signal-fair`
- Below → `signal-poor`
- Falls back to SF7 floor if datarate string cannot be parsed.

## Configuration

Config is TOML, passed via `-c <file>` (can be repeated to merge). Print an annotated template:

```bash
cargo run --bin lorawan-sniffer -- configfile
```

Key sections: `[sniffer]`, `[sniffer.api]`, `[sniffer.database]`, `[gateway]` (protocol = `"udp"` | `"concentratord"`), and optional `[[filters]]` blocking rules.
