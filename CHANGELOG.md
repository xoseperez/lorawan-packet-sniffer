# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **`docs/MOBILE.md`** — touchscreen optimization report with recommendations
  for the 800×600 tablet target device (tap target sizes, collapsible filter
  row, `<details>` reliability, font sizing).

### Changed

- **WebUI — visual design overhaul** (pure CSS, no new dependencies):
  - Direction column color-coded: uplink ▲ blue, downlink ▼ amber.
  - MType column renders a colored pill badge per category:
    join/rejoin = purple, data-up = blue, data-down = amber, other = gray.
  - Status column replaced "F"/"B" letters with an 8 px colored dot
    (green = forwarded, red = blocked) with a `title` tooltip.
  - Blocked rows use a stronger red tint and a 3 px inset left-border accent.
  - Zebra striping on even rows for readability in dense data sets.
  - Header title split into accent-blue "LoRaWAN" + muted "Sniffer" with a
    vertical rule separating it from the filter controls.
  - Table column headers styled as uppercase/spaced labels in muted color to
    create a clear visual tier below the data.
  - Live/Pause toggle shows a pulsing dot when the feed is live.
  - WebSocket indicator replaced text icon with a colored dot + text label.
  - Footer copyright notice "© RAKwireless 2026" added to the left of the
    Live/Pause toggle.
- **SNR signal-quality coloring is now spreading-factor-aware.** Color is
  determined by the margin above the SF's minimum decodable SNR (SF7 = −7.5 dB
  … SF12 = −20 dB): ≥ floor+10 dB → green, ≥ floor+3 dB → amber, below →
  red. Falls back to SF7 floor if the datarate string cannot be parsed.
- Merged `libsniffer` library crate into `lorawan-sniffer` as an internal
  `core/` module (`src/core/{decoder,filter,metadata,storage,toa,error}.rs`).
  The two-crate workspace added complexity without benefit for a single-binary
  project with no external consumers; the `core/` module preserves the same
  architectural boundary enforced by convention rather than by the compiler.

## [2.0.0] - 2026-03-06

Complete rewrite of the Python v1 sniffer in Rust. The sniffer is now a full
MITM proxy supporting both directions (uplinks and downlinks) and both gateway
protocols (Semtech UDP and ChirpStack Concentratord).

### Added

- **UDP Packet Forwarder MITM proxy** — transparently proxies all six Semtech
  protocol v2 message types: `PUSH_DATA`, `PUSH_ACK`, `PULL_DATA`, `PULL_ACK`,
  `PULL_RESP`, `TX_ACK`.
- **ChirpStack Concentratord proxy** — ZeroMQ IPC proxy (SUB→PUB for events,
  REP→REQ for commands) with Protobuf decode/encode.
- **LoRaWAN PHY decoder** — decodes MHDR (MType), DevAddr/FCnt/FPort for data
  frames, and JoinEUI/DevEUI for Join Requests.
- **Selective message blocking** — configurable filter rules matched by MType
  and direction; first matching rule wins.
- **Stealth mode** — optional ACK suppression for blocked messages so the
  sender is unaware the message was received (`suppress_ack = true`).
- **SQLite persistence** — every captured message is stored with full RF and
  LoRaWAN metadata; WAL mode enabled for concurrent reads.
- **Configurable data retention** — periodic deletion of rows older than a
  humantime duration (e.g. `retention = "30days"`).
- **Time-on-Air calculation** — estimated ToA in milliseconds for each frame.
- **axum HTTP/WebSocket server** — serves the REST API and the real-time
  WebSocket feed from the same binary.
- **REST API** — `GET /api/messages` with `since` (ISO-8601) and `limit` query
  parameters for historical queries.
- **WebSocket feed** — real-time push of `MessageRecord` JSON envelopes to all
  connected clients.
- **React 19 WebUI** — single-page application embedded in the binary at
  compile time via `rust-embed`; no separate web server needed.
  - Virtual message table (`react-virtuoso`) with live auto-scroll and
    pause/resume.
  - Client-side filters: device (DevAddr/DevEUI), MType, direction, status,
    time range.
  - Column visibility toggle persisted to `localStorage`.
  - Sortable by timestamp (ascending/descending).
  - System dark/light theme via `prefers-color-scheme`.
  - Touch-friendly layout usable on 800×600 screens.
- **CORS support** — permissive by default; configurable `cors_origins` list.
- **Syslog support** — `log_to_syslog = true` routes logs to the system
  syslog daemon.
- **`configfile` subcommand** — prints an annotated TOML template to stdout.
- **Multi-arch release builds** — `x86_64-unknown-linux-musl` and
  `aarch64-unknown-linux-musl` via `cross`; statically linked, no runtime
  dependencies.
- **Docker support** — single-stage `Dockerfile`; `make docker-buildx` pushes
  a multi-arch image (amd64 + arm64).

[Unreleased]: https://github.com/xoseperez/lorawan-packet-sniffer-v2/compare/v2.0.0...HEAD
[2.0.0]: https://github.com/xoseperez/lorawan-packet-sniffer-v2/releases/tag/v2.0.0
