# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

> **Upgrade note:** a WebUI or API reached by host name, rather than by IP
> address or `localhost`, now needs that name in `allowed_hosts`. See
> "Changed" below.
>
> **Upgrade note:** the first start after the upgrade checks every stored row
> for malformed frames before it starts forwarding. On a large database this
> takes a while and needs free disk space for SQLite's write-ahead log; the
> README gives the time and the space measured on 1,000,000 rows.

### Added

- **Malformed-frame detection.** A frame that cannot be valid LoRaWAN 1.0 or
  1.1 is marked with the first reason that applies, in this order: `crc` (the
  gateway reported a failed CRC), `mhdr` (the RFU or Major bits of the first
  byte are not zero) or `length` (a JoinRequest that is not 23 bytes, a
  JoinAccept that is not 17 or 33, a RejoinRequest that is not 19 or 24, a
  data frame shorter than 12 bytes plus its FOptsLen). Proprietary frames
  have no length rule. It works in both gateway protocols and both directions;
  a source that reports no CRC is not marked for it. Forwarding is unchanged.
  - Every record has a new `malformed` field in the database, `/api/messages`,
    `/ws` and MQTT: `null` for a valid frame, otherwise `"crc"`, `"mhdr"` or
    `"length"`.
  - The WebUI hides malformed rows by default. A **Show malformed** checkbox
    at the end of the Type menu, remembered by the browser, shows them dimmed
    and marked, with the reason in the Device cell ("malformed: CRC failed",
    "malformed: bad header", "malformed: wrong length") and in full on hover.
    The status bar counts the malformed rows hidden or shown.
  - The CSV export has a `malformed` column before `label`.
  - The README has a "Malformed frames" section, including the gateway
    settings that make gateways forward CRC-failed packets
    (`forward_crc_error` in the Semtech packet forwarder, `disable_crc_filter`
    in Concentratord).
- **`tools/gwsim.py --malformed-ratio R`:** that fraction of the PUSH_DATA
  datagrams also carries one malformed frame, taken in turn from a list of ten
  that covers every reason. The fake identifiers those frames would carry if
  trusted are printed at start, and the summaries count the frames by reason.
- **Device labels.** Give a device a name from the WebUI, such as "Qingping
  T&H Havana" for `00E8A80B`. A label belongs to the identifier the Device
  column shows: the DevAddr on data frames, the DevEUI on join requests.
  - A **Label** column (hidden by default) shows it on every row with that
    identifier, past and live. Clicking a cell opens a dialog to set, change
    or remove the label (Enter saves, Escape cancels, an empty label removes).
  - Changes appear at once in every open WebUI, without a reload.
  - The device filter also matches labels.
  - The CSV export has a `label` column, always quoted; a label starting with
    `=`, `+`, `-` or `@` gets a leading apostrophe so a spreadsheet does not
    read it as a formula.
  - Labels are stored in the database (a new `device_labels` table, added to
    existing databases in place), at most 64 characters each and 10,000 in
    all. They live as long as their identifier has a stored record or was
    seen live within the retention period, and nothing is removed during the
    first retention period after a start, so a restart never removes a label.
  - The sniffer keeps a best-effort, in-memory list of identifiers seen live
    (at most 100,000), so rows that were never written to the database can be
    labelled too.
  - Labels follow identifiers, not devices: a device that rejoins gets a new
    DevAddr, and its DevEUI label and DevAddr label are separate. Labels are
    not added to MQTT payloads, and there is no authentication on label
    editing.
- **Label API and `/ws/labels`.** `GET /api/labels` returns every label,
  `PUT /api/labels/{device}` (JSON body `{"label": "..."}`) sets or replaces
  one and `DELETE /api/labels/{device}` removes it. The new WebSocket
  `/ws/labels` sends the full set on connect, then one event per change in the
  order the changes were saved, and a fresh full set to a client that falls
  behind. `/api/messages` and `/ws` are unchanged. Writes follow the same
  Origin rule as WebSocket handshakes, and origins listed in `cors_origins`
  may now also use `PUT` and `DELETE`.
- **`allowed_hosts`** in `[sniffer.api]` (`SNIFFER_API_ALLOWED_HOSTS` in
  Docker): the host names the WebUI and API may be reached by. IP addresses and
  `localhost` always work. `["*"]` turns the check off, and with it the
  protection against DNS rebinding, so it is unsafe. The README describes both
  ways to set this up behind a reverse proxy, with nginx examples.
- **Release archives** for x86_64 and aarch64 Linux (`make dist`), with a
  `SHA256SUMS` file. Each holds the static binary, a systemd service, an
  install script and the configuration templates, and is reproducible.
- **systemd service** for bare-metal installs (`packaging/`). The sniffer runs
  as an unprivileged, sandboxed user and keeps its database in
  `/var/lib/lorawan-sniffer`. Secrets such as the MQTT password go in a
  root-only environment file and are referenced from the configuration as
  `$NAME`. An override (`concentratord.conf`) enables Concentratord mode.
- **`install.sh`** installs or upgrades the service without overwriting an
  existing configuration, never restarts it on its own, and removes it with
  `--uninstall` (keeping configuration and database).
- **Configuration for running next to the LNS** on the same machine: the
  sniffer takes UDP port 1702 and forwards to the LNS on `127.0.0.1:1700`.
- **Release Docker image** (`docker/Dockerfile.release`, `make
  docker-build-release`), built from the same binaries as the archives, so
  the multi-arch image needs no emulation to build.
- **Automated releases.** Pushing a `vX.Y.Z` tag on `master` builds the x86_64
  and aarch64 Linux archives, publishes them with a `SHA256SUMS` file as a
  GitHub Release (notes taken from this changelog), and pushes a multi-arch
  (amd64 + arm64) image to `ghcr.io/xoseperez/lorawan-packet-sniffer`.
  Versions with a suffix such as `-rc.1` are published as pre-releases and do
  not move the `latest` image tag. The workflow can also be run by hand as a
  dry run that publishes nothing.
- **Warning for unset environment variables:** a configuration value that is
  exactly `$NAME` with no such variable set is logged at startup by key (never
  by value), instead of the text being used silently, for example as an MQTT
  password.
- **`tools/gwsim.py --joins N` and `--random-devaddr`:** simulated gateways
  that also send JoinRequests (a fixed DevEUI per gateway, printed at start)
  and data uplinks from a new random DevAddr each time. The fake LNS now
  counts the frames it receives by message type.

### Changed

- **A malformed record has `null` in the twelve fields decoded from the frame
  body:** `dev_addr`, `dev_eui`, `join_eui`, `dev_nonce`, `fcnt`, `fport`,
  `adr`, `adr_ack_req`, `ack`, `class_b`, `f_pending` and `f_opts_len`. They
  used to hold values read from noise, such as made-up DevAddrs. The type, the
  size, the radio data, the status and the gateway are kept. A malformed frame
  therefore no longer produces a made-up identifier for the device filter, the
  list of identifiers seen live, device labels or an MQTT topic.
- **A blocking rule with an `mtype` list no longer matches a malformed
  frame.** The type of such a frame is only its first byte read as a header,
  so a rule blocking JoinRequests blocked about one noise frame in eight, and
  in Semtech UDP held back the valid frames in the same PUSH_DATA. Rules with
  no `mtype` list (direction and gateway only) still match malformed frames,
  and the whole-datagram policy is unchanged.
- **Database schema 3, and the stored rows are checked on the first start.**
  The upgrade adds a `malformed` column and a per-row flag, and keeps every
  row. Before forwarding starts, the sniffer marks the stored rows with a
  failed CRC (`crc`) or a length that does not fit their type (`length`) and
  clears their body-derived fields; `mhdr` cannot be applied, because the
  first byte is not stored. It logs how many rows it will check first. If the
  check fails, the sniffer starts anyway and tries again at the next start.
  Rows an older version records after a rollback are checked at the next
  start of this version. Older versions keep working on the upgraded
  database.
- **`tools/gwsim.py`:** the fake LNS counter of datagrams that are not
  Semtech UDP packets is now called "invalid datagrams" (it was "malformed",
  which now means a frame). The fake LNS counts malformed frames by reason,
  leaves them out of its type and identifier tallies, and never sends a
  downlink to their DevAddr.
- **New home: `github.com/xoseperez/lorawan-packet-sniffer`.** The Rust
  version now lives in the original project's repository, on `master`; the
  original version 1 is on the `v1` branch, with its full history and tags.
  The Docker image is `ghcr.io/xoseperez/lorawan-packet-sniffer`.
- **Breaking: a WebUI or API reached by host name, rather than by IP address
  or `localhost`, needs that name in `allowed_hosts`.** The sniffer now checks
  the `Host` header of every request (WebUI, REST and both WebSockets) to stop
  DNS rebinding, where a web page points its own domain name at the sniffer so
  that its requests look same-origin. A request for any other host name gets
  `403` with a plain-text message that names the setting. Access by IP address
  or `localhost` is not affected. Add the name under `[sniffer.api]`, or set
  `SNIFFER_API_ALLOWED_HOSTS` in Docker. Behind a reverse proxy, either pass
  the original `Host` through and list the public name, or let the proxy send
  an IP address as `Host` and list the public origin in `cors_origins`.
- **Same-origin API by default.** With `cors_origins` empty the API sends no
  CORS headers (it used to allow every origin), and the WebSocket feed refuses
  handshakes from other sites. Before, any web page open in a browser that
  could reach the sniffer could read the captured traffic. Origins listed in
  `cors_origins` are allowed for both.
- **One log line per packet only at DEBUG.** Captured messages and
  Concentratord commands are no longer logged at INFO, which filled the
  journal on busy servers; startup, gateway sessions and warnings still are.
- Docker images are based on Alpine 3.23 (3.20 is no longer supported).
- **WebUI filter bar without the direction filter**, so the filters fit an
  800-pixel-wide screen. The Dir column still shows each message's direction.
  A direction filter saved in the browser by an earlier version no longer
  applies.
- **The WebUI footer shows the version**, as
  "v2.1.0 | © Xose Pérez for RAKwireless 2026". The version comes from
  `webui/package.json`, and a test fails if it differs from the crate's.

### Fixed

- **Noise got identifiers and header fields.** A JoinRequest was decoded
  from 19 bytes up, although it is 23 bytes long, and a data frame from 8
  bytes up, although the shortest is 12 bytes plus its FOptsLen, so short noise
  got a DevEUI, JoinEUI or DevAddr. A JoinRequest is now decoded only at 23
  bytes and a data frame only at 12 bytes plus FOptsLen; anything else is
  marked malformed.
- **The WebUI opened on its oldest rows.** With the table sorted newest
  first (the default), loading the history scrolled it to the end, so the
  page showed the oldest loaded messages until scrolled back up. It now opens
  at the newest. Sorting oldest first also orders timestamps by time rather
  than as text, which misplaced some of them.
- An upstream given by name, such as `localhost`, could resolve to `::1`
  first, and the sniffer forwarded there even when the LNS or multiplexer
  only listened on IPv4, so nothing arrived. IPv4 addresses are now
  preferred; IPv6 is used when the name has no IPv4 address.
- SIGTERM, as sent by `systemctl stop` and `docker stop`, now stops the
  sniffer cleanly like Ctrl-C. Before, it killed the process outright, and
  under Docker, where the sniffer is PID 1, it was ignored until Docker's stop
  timeout killed it.
- Records still waiting to be written to the database are saved before the
  sniffer exits; before, the last ones could be lost on shutdown.
- `make docker-buildx` built its arm64 image around an x86_64 binary, so that
  image could not run; it now uses the cross-compiled binaries.

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

[Unreleased]: https://github.com/xoseperez/lorawan-packet-sniffer/compare/v2.1.0...HEAD
[2.1.0]: https://github.com/xoseperez/lorawan-packet-sniffer/compare/v2.0.0...v2.1.0
[2.0.0]: https://github.com/xoseperez/lorawan-packet-sniffer/releases/tag/v2.0.0
