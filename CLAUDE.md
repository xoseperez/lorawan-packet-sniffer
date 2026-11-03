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
cd webui && npm test        # the WebUI's history logic and the filter vectors (make webui-test); make test runs both

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
  - **`src/core/`** — protocol-agnostic logic: LoRaWAN PHY decoder, filter engine, SQLite storage, metadata types, ToA calculator, device-label validation (`labels.rs`), the live-identifier list (`seen.rs`) and the history query types (`history.rs`: `HistoryQuery`, `HistoryFilter`, `HistoryPage`, label case folding) and the storage queue's item type (`writer.rs`), and random bytes that can fail (`random.rs`: `RandomSource`, `generate_token`).
  - **`src/api/`** — HTTP/WebSocket server (axum): Host check, the optional login (`auth.rs`: sessions, the lockout, the `require_login` layer, login and logout), the `/api/messages` history handler (`messages.rs`), label endpoints and the `LabelWriter` task.
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
2. Each packet is decoded by `core::decoder` into a `DecodedFrame` (MType, DevAddr/DevEUI, FCnt, FPort). The decoder also takes the gateway's CRC status and classifies the frame: CRC failed (`crc`), then reserved or Major MHDR bits (`mhdr`), then a length that does not fit the type (`length`). A malformed frame comes back with only its type, size and reason, so no fake identifier or header field exists downstream; the backends' record builders just copy the fields. Type rules see no type for a malformed frame (`filter::evaluate` gets `None`).
3. `core::filter::evaluate()` checks compiled `FilterRule`s — first match wins; no match = forward.
4. Results are assembled into a `MessageRecord` (the canonical cross-protocol representation defined in `core::metadata`).
5. `Outputs::emit` queues the record for storage (`mpsc` of `WriterItem`, drained by the writer task into `Storage`) and then sends it on the `broadcast` channel to all connected WebSocket clients. Storage comes first on purpose: a record anyone could have seen live must already be queued ahead of any later `Settle` marker (step 8). Neither send waits.
6. The axum API server (`src/api/mod.rs`) handles `/ws` (live feed), `/ws/labels` (device labels: full set on connect, then one event per change), `/api/messages` (REST history: `from`, `before`, `newest`, `count`, `settle`, the filters, and the `X-*` headers, see step 8), `/api/labels` (`GET`, `PUT`/`DELETE` on `/api/labels/{device}`), `/api/session` (`{"login": true|false}`), `/api/login` and `/api/logout` (these two only with login on), and `/*` (embedded React SPA). A Host check (`src/api/host.rs`, `[sniffer.api] allowed_hosts`) is the outermost layer for every request, then CORS (only when `cors_origins` is set), then, with `[sniffer.api] login = true`, the login layer `require_login` (`src/api/auth.rs`) just outside the routes: everything under `/ws` and `/api/` except `/api/login` and `/api/logout` needs a valid session cookie or `Authorization: Bearer <token>`, else `401` (plain text, no redirect, no `WWW-Authenticate`); the WebUI bundle stays public. A token that is presented once 10 wrong ones have been compared in the current fixed one-minute window gets `429` with `Retry-After`, unexamined (`Lockout::attempt`, the only place a token is compared); session cookies are neither counted nor locked out. A session is `v1.<issued>.<nonce>.<mac>`, an HMAC under a key derived from the token and the 32-byte secret in the database (`session_secret` table, created at the first start with login on, read-only afterwards); nothing is kept per session. Login and logout accept only a request with exactly one `Content-Type` that is `application/json` and an `Origin`, if any, with the request's host and port (`same_host_json`). With login off none of this exists: no cookie, no `401`, and `/api/login` and `/api/logout` are unknown paths. A `/ws` client that lags, or whose socket stops accepting data for 10 s, is closed with code 1013.
7. A listener on the same `broadcast` channel notes which DevAddr/DevEUI values were seen live (`core/seen.rs`), so rows that never reached the database can still be labelled.
8. History reads. `Storage` holds a writer connection and, when the database is in WAL mode, a second read-only connection that answers `query_history`, so a long scan never holds the mutex the writer needs. Without WAL there is no reader: a time-only query still runs on the writer, a query with a row filter fails with `HistoryUnavailable` (503 `no_wal`). The handler lets one query run at a time (the permit moves into the blocking task, so a dropped request cannot stack a second one behind it), holds at most three response bodies in memory, and cancels a running scan when the client goes away. `settle=true` puts a `WriterItem::Settle` marker in the storage queue and waits up to 5 s until the writer, which handles the queue in order, reaches it; `X-History-Settled` says whether it did. The writer counts failed inserts, and every `/api/messages` response reports the count in `X-Storage-Write-Failures`.
9. The WebUI loads the newest 5,000 matching records (`/api/messages?...&newest=true&count=true&settle=true&limit=5000`) on page load, on a range or filter change and on every `/ws` open, replaces its rows with them and merges live records by `id`. For a selected day the request carries `from` and `before`, the day's two local midnights computed in the browser (the server knows nothing about days or time zones). The request rules are pure functions in `webui/src/lib/history.ts`; `hooks/useHistory.ts` only runs them. The same filter rules exist twice, in `core/history.rs` / `Storage::query_history` and in `lib/history.ts`, and `webui/test/history-vectors.json` is run against both.

### Proxy backends

- **UDP** (`proxy/udp.rs`): Implements the Semtech UDP Packet Forwarder protocol (PUSH_DATA, PULL_DATA, PULL_RESP, etc.). Runs entirely in async tokio tasks.
- **Concentratord** (`proxy/concentratord.rs`): Implements ChirpStack Concentratord via ZeroMQ IPC + Protobuf. ZMQ calls are dispatched to `spawn_blocking` because the `zmq` crate is synchronous.

### Key types (`core/`)

- `MessageRecord` — the normalized struct stored, broadcast, and returned by the REST API. Its `malformed: Option<Malformed>` (`crc` | `mhdr` | `length`) is `None` for a valid frame; on a malformed one the twelve body-derived fields are `None`.
- `MType` — LoRaWAN message type decoded from MHDR bits [7:5].
- `FilterRule` / `FilterDecision` — compiled filter rule and its evaluation result.
- `Storage` — `Clone`able wrapper around the writer connection and, in WAL mode, a read-only connection for history reads (each an `Arc<Mutex<rusqlite::Connection>>`).
- `HistoryQuery` / `HistoryFilter` / `HistoryPage` (`core/history.rs`) — what a history request asks for and what comes back (records, the count of matches, the count of malformed matches). `Storage::query_history` answers it.
- `Auth` (`api/auth.rs`) — what the login needs once set up: the session key, the token comparison, the random source and the `Lockout`. Built by `cmd/root.rs` (`start_login`) only when `login = true`, after the session secret is read or created; held as `ApiContext.login: Option<Arc<Auth>>`. It has no `Debug` on purpose, and neither have `Api` or `Configuration`, so the token cannot be printed by accident. `Storage::session_secret(fresh)` returns the stored secret without any write, or makes it with `BEGIN IMMEDIATE`.
- `WriterItem` (`core/writer.rs`) — what the storage queue carries: a `Record` to insert, or a `Settle` marker that the writer answers once everything queued before it has been processed.
- Device labels live outside `MessageRecord`: the `device_labels` table is keyed by identifier (`dev_addr`, else `dev_eui`, in the record's text form), and the WebUI joins labels to rows by that key. A malformed record has no identifier, so it never enters the live list and can never be labelled. Labels never touch the forwarding path. Every label change goes through the `LabelWriter` task in `api/labels.rs`, which commits and then publishes on `/ws/labels`.

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
| `src/App.tsx` | The gate (`App`): asks `checkSession()` on load and shows `GateStatus` while checking or unreachable (3 s retry), `LoginPage`, or `SnifferApp`, and goes back to the login when `reportUnauthorized()` fires (`document.body.dataset.session` holds the state for scripts). `SnifferApp` is the layout shell below, with `loginEnabled` for the Log out item; localStorage hooks, the stored filters passed through `normaliseFilters`; runs `useHistory`; at render time keeps the held rows that pass `matchesFilter` (the device filter also matches labels; malformed rows are hidden unless `showMalformed`) and sorts them, on a 5 s tick so rows age out of the range without traffic; Matching, Shown and Blocked (over the rows shown); label dialog state; CSV export of the rows shown (ending `…, malformed, label`), started from the ⚙ Settings menu |
| `src/components/MessageTable.tsx` | `TableVirtuoso` table; signal-quality coloring; MType badges; Label column (`LabelCell`, clickable only on rows that have an identifier); malformed rows (`row-malformed`, dashed badge, short reason in the Device cell); `data-id` on every row, which scripts read to compare the table with the database |
| `src/session.ts` | The login's WebUI side, no React: `checkSession()` (`GET /api/session`: `open` / `logged-in` / `login` / `unreachable`), `logIn(token)` and `logOut()` (`POST` with `Content-Type: application/json`, which the server demands), and the small bus `onUnauthorized` / `reportUnauthorized` that carries "the session is gone" from wherever a 401 is seen to the gate. Relative URLs only |
| `src/components/LoginPage.tsx` | The login page: one password field, Log in, Enter submits; a blank field is not sent ("Enter the token"), the token is trimmed, the server's message (wrong token, lockout, proxy `Host`) is shown with the text kept, and a login that the server accepts but the browser did not keep says to allow cookies |
| `src/components/LabelDialog.tsx` | Modal to set, change or remove the label of one identifier (DevAddr or DevEUI): text field limited to 64 characters, Save / Delete (only when a label exists) / Cancel, Enter saves, Escape or a click outside cancels, shows the server's error |
| `src/components/FilterBar.tsx` | Device input (also matches labels), MType multi-select with the "Show malformed" checkbox at the end of its menu (the header has no room for it at 800 px), status/gateway selects, and the time select (`filter-time`, 110 px): Last 5m / 15m / 1h / 24h, the chosen day as an entry of its own showing its full date (`FilterState.day`, `YYYY-MM-DD`), and "Select day" (value `pick`, never stored). "Select day" calls `showPicker()` on the invisible date input `input[data-day-picker]` laid over the select, synchronously inside the select's change so the click's user activation still counts; the input's native `change` goes through `pickDay`. When `showPicker()` throws, the input is shown in the select's place (`data-fallback="true"`) until a day is picked, the input loses focus or Escape is pressed |
| `src/components/ColumnPicker.tsx` | The **⚙ Settings** menu: column visibility (persisted to `localStorage`; the Label column is hidden by default), then a separator and 36 px actions that close the menu: **Export CSV** (`data-action="export-csv"`, disabled with no rows) and, with login on (`onLogout` given), **Log out** (`data-action="logout"`). The header has no other room at 800 px |
| `src/components/StatusBar.tsx` | Matching ("Loading…" while a load is busy, "–" before the first), Shown, Blocked, "Malformed hidden / shown: N" ("–" when unknown), each with `data-stat` and `data-value` for scripts; WS status indicator; version and copyright notice (version from `package.json` via Vite `define`); live/pause toggle |
| `src/components/HistoryNotice.tsx` | The strip between table and footer (`#history-notice`, `role="status"`, one line, full text in the title): a failed load (error style), the count of records the database failed to store, "the database is behind", and the cap ("Showing the newest 5,000 of N matching messages in the last …" or "… on YYYY-MM-DD", the range words from `rangeWords`, passed as `cap.range`). Absent when it has nothing to say |
| `src/hooks/useWebSocket.ts` | WebSocket connection, generic over the path and the event handler (`/ws` for messages, `/ws/labels` for labels); an `onOpen` callback on every open, which `/ws` uses to reload the range; reconnects wait about 3 s, doubling to 60 s, each randomised between half and all of it (`nextReconnect`, which wants the previous *nominal* delay back). A browser cannot see why a handshake was refused, so on every close it calls `checkSession()` before reconnecting: `login` means `reportUnauthorized()` and no more attempts; any other answer reconnects as before (a 1013 close is not a session end) |
| `src/hooks/useHistory.ts` | Runs the request state machine (`nextRequests`) with timers, `fetch` (one `AbortController` per request, 30 s timeout) and the retry series; a 401 (`classifyFailedLoad`) is reported with `reportUnauthorized()` and ends the load and its retries with no error strip; holds the rows, the live-record log and the pause buffer; exposes the rows, `older`, `malformed`, `writeFailures`, `busy`, `behind`, `error`, `knownGateways`, `pause()` / `resume()` |
| `src/lib/history.ts` | The history logic as pure functions with only `import type` imports (the Node test transpiles this one file alone): `matchesFilter` and `historyParams` (the filter rules and the query string that asks the server for the same thing; a filter has an exclusive upper bound `untilMs`, sent as `before`, `null` for a span), `dayBounds` (a local day's two midnights, 23 or 25 hours on a daylight-saving day), `todayKey`, `pickDay` (a valid day not after today, else `null`), `normaliseFilters` (a saved range the selector no longer offers, or a bad day, becomes `'5m'`), `rangeWords` (how the range reads in the cap), `mergeRecords` / `mergeAfterLoad`, the count arithmetic, `nextRequests` (with its `unauthorized` event) and `classifyFailedLoad` (a failed load's status: `unauthorized` for 401, `noWal`, `busy` or `failed`), and the retry / reconnect back-off. Change a rule here and in `core/history.rs` together |
| `src/hooks/useLabels.ts` | Label state (`Record<identifier, label>`; `synced` is true after the first snapshot), fed only by `/ws/labels` (a snapshot replaces it, an event updates one entry); `saveLabel` / `removeLabel` call `PUT` / `DELETE /api/labels/{device}` and return success or the API's error message, never writing state themselves, so a late HTTP response cannot overwrite a newer event. A 401 calls `reportUnauthorized()` and returns "Your session has ended. Log in again." |
| `src/styles/index.css` | **All styling** — pure CSS, no framework (`.history-notice` and its `--notice-*` variables included) |
| `test/history.test.mjs`, `test/history-vectors.json` | `npm test`: the Node test runs `lib/history.ts` (request rules, merging, counts, back-off), and checks the shared vectors (records, labels and filter cases) that a Rust test also sends through the `/api/messages` router. No test framework |
| `src/types/messages.ts` | TypeScript types (`MessageRecord`, `WsMessage`, label snapshot and event types, etc.), `deviceId()` (re-exported from `lib/history.ts`), the identifier of a row (`dev_addr` ?? `dev_eui` ?? none), and `malformedReason()` / `malformedShort()`, the full and the short reason of a malformed row |

### CSS Design System

All theming is done via CSS custom properties in `index.css`. Key variables:

- `--dir-up` / `--dir-down` — uplink (blue) and downlink (amber) direction colors
- `--mtype-{join,up,down,other}-{bg,fg}` — MType badge palette (join=purple, up=blue, down=amber)
- `--blocked-bg` / `--blocked-border` — blocked row tint and left-accent color
- `--malformed-border` — dashed border of the malformed type badge (`.mtype-malformed`); `.row-malformed` dims the row
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

`[sniffer.api] login` (default `false`; Docker `SNIFFER_API_LOGIN`) and `token` (Docker `SNIFFER_API_TOKEN`) turn on the WebUI login. With login on, `validate` requires a token that passes `token_shape_ok` (at least 32 hexadecimal characters, at least 8 distinct), which is a format check and never proof of randomness; no error message quotes the token. `configfile` prints a freshly generated token (16 bytes from `RandomSource`, as 32 hex characters) and never the loaded one. The sniffer never writes its configuration: `packaging/install.sh` writes the generated token into a fresh `sniffer.toml` (staged root-only, renamed, then `0644`) and into `sniffer.toml.new` on an upgrade. The token, the stored secret and session values never go into a log line, a format string or a `Debug`, and every comparison of one is `ring::hmac::verify`.
