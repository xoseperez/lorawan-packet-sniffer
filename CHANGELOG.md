# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [2.1.0] - 2026-10-31

### Added

- **Multiple gateways in the UDP proxy.** The sniffer keeps one session per
  gateway EUI, each with its own upstream socket towards the LNS, so the LNS
  sees every gateway as a separate peer. A session follows the gateway's EUI
  when its address or port changes, is released after an idle timeout
  (`session_timeout`, default `5m`) and is capped (`max_gateways`, default
  `100`); packets from new gateways beyond the cap are dropped with a
  rate-limited warning without affecting existing ones.
- **UDP listen mode** (`[gateway.udp] mode = "listen"`) for running without an
  LNS: any number of gateways are recorded and nothing is forwarded. Optional
  PUSH_ACK / PULL_ACK replies (`listen_acks`, on by default); downlinks are
  never sent. A warning is logged at startup if blocking rules are configured,
  since they have no effect.
- **Gateway EUI on every record**, in both UDP and Concentratord modes
  (Concentratord's gateway ID is learned from its uplinks and from a
  `GetGatewayId` reply).
- **JoinEUI and DevNonce** on join requests, and the **FCtrl flags** (ADR,
  ADRACKReq, ACK, ClassB on uplinks; ADR, ACK, FPending on downlinks) and
  **FOptsLen** on data frames, in the database, REST API, WebSocket feed, CSV
  export and MQTT. Flags that do not apply to a frame are `null`, not `false`.
- **Gateway-specific blocking rules:** `[[filters]]` accept an optional
  `gateways` list of EUIs (with or without colons, any case). An invalid EUI is
  a startup error.
- **MQTT publishing** (`[sniffer.mqtt]`, disabled by default) over TCP or TLS,
  with a topic template (`{gateway_eui}`, `{direction}`, `{mtype}`, `{status}`,
  `{dev_addr}`, `{dev_eui}`), QoS 0–2, optional credentials and a CA file for
  private brokers (public CA roots are bundled in the binary); a client
  certificate and key enable mutual TLS. Publishing is best effort and never
  slows forwarding: when the broker is down or slow, messages are dropped and
  counted instead of queued without limit, and the client reconnects by itself.
  Credentials and keys are never logged.
- **Self-forwarding guard.** In proxy mode the sniffer refuses to start if its
  upstream is its own gateway-facing port, reached through any of the machine's
  addresses, and drops looped packets at runtime (NAT, Docker port forwarding)
  without recording or forwarding them again.
- **Database schema migrations** through `PRAGMA user_version`. Databases
  created by v2.0.0 are upgraded in place on startup and keep their rows, with
  the new fields empty.
- **`Captured` message status** for messages seen in listen mode, shown as a
  neutral grey dot in the WebUI and available in the status filter.
- **WebUI:** a gateway column (the last 8 hex digits of the EUI, full EUI in the
  tooltip) and a gateway filter, optional JoinEUI, DevNonce, flags and FOptsLen
  columns in the column picker, and the new fields in the CSV export.
- **Simulator tools:** `tools/gwsim.py` (simulated gateways and a fake LNS for
  functional and load testing) and `tools/procstat.sh` (peak memory and open
  descriptors of a running process).
- **`docs/MOBILE.md`** — touchscreen optimization report with recommendations
  for the 800×600 tablet target device (tap target sizes, collapsible filter
  row, `<details>` reliability, font sizing).

### Changed

- **The Docker image starts in listen mode when no upstream LNS is set**
  (`GATEWAY_UDP_UPSTREAM_SERVER` unset or empty), and in proxy mode when it is.
  New environment variables: `GATEWAY_UDP_MODE`, `GATEWAY_UDP_LISTEN_ACKS`,
  `GATEWAY_UDP_SESSION_TIMEOUT`, `GATEWAY_UDP_MAX_GATEWAYS` and
  `SNIFFER_MQTT_ENABLED`, `_SERVER`, `_PORT`, `_TLS`, `_CA_FILE`,
  `_CLIENT_CERT_FILE`, `_CLIENT_KEY_FILE`, `_CLIENT_ID`, `_USERNAME`,
  `_PASSWORD`, `_TOPIC`, `_QOS`.
- **Environment values are inserted into the configuration literally.** A value
  with quotes, backslashes or `$` (a password, for example) no longer breaks
  parsing or gets expanded a second time, and a variable whose name is a prefix
  of another no longer corrupts the longer one.
- **ACKs are routed per gateway session** (PUSH_ACK to the source of the
  gateway's last PUSH_DATA; PULL_ACK and PULL_RESP to the source of its last
  PULL_DATA) instead of through a token map shared by all gateways. Packets are
  still forwarded byte for byte.
- Synthetic TX_ACKs for blocked downlinks carry the EUI of the gateway the
  downlink was addressed to, instead of the last gateway seen.
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

### Fixed

- The default configuration forwarded packets to the sniffer itself, turning a
  single packet into tens of thousands of records. This is now refused at
  startup and cut at runtime.
- FPort was read from the MIC on data frames that have no FPort.
- The ACK token map was never pruned and could grow to 65,536 entries; it is
  gone, replaced by per-gateway session state that is released when idle.
- A single error on the LNS-facing UDP socket (for example connection refused
  while the LNS was down) stopped the whole proxy; errors are now logged with
  rate limiting and the proxy keeps running.

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

[Unreleased]: https://github.com/xoseperez/lorawan-packet-sniffer-v2/compare/v2.1.0...HEAD
[2.1.0]: https://github.com/xoseperez/lorawan-packet-sniffer-v2/compare/v2.0.0...v2.1.0
[2.0.0]: https://github.com/xoseperez/lorawan-packet-sniffer-v2/releases/tag/v2.0.0
