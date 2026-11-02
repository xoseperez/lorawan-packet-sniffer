# LoRaWAN Packet Sniffer v2

A transparent MITM monitoring proxy for LoRaWAN gateways.

Sits between one or more gateways and a LoRaWAN Network Server (LNS), capturing and decoding all uplinks and downlinks in real time, with an optional selective message-blocking engine. It can also run without any LNS, just recording what the gateways send.

Version 2 is a rewrite in Rust. The original version 1 (v0.x releases) lives on the [`v1` branch](https://github.com/xoseperez/lorawan-packet-sniffer/tree/v1).

![The WebUI showing live traffic from three gateways, with device labels](docs/images/webui.png)

## Features

- **Dual-protocol support** — Semtech UDP Packet Forwarder and ChirpStack Concentratord (ZMQ/Protobuf)
- **Full MITM proxy** — captures both uplinks and downlinks without modifying them
- **Multiple gateways** — one sniffer instance can sit in front of an LNS serving a fleet of Semtech UDP gateways; every uplink, downlink and ACK reaches the right gateway, and the LNS sees each gateway as its own UDP peer
- **Listen mode** — no LNS needed: the sniffer records what any number of gateways send (optionally acknowledging them) and forwards nothing
- **Rich message metadata** — every record carries the gateway EUI, plus JoinEUI and DevNonce on join requests and the FCtrl flags (ADR, ADRACKReq, ACK, ClassB / FPending) and FOptsLen on data frames
- **Selective blocking** — block messages by MType, direction and, optionally, gateway; optional stealth mode (suppresses ACKs)
- **Malformed-frame detection** — frames that cannot be valid LoRaWAN (CRC failed, bad header, wrong length) are marked with the reason, carry no made-up DevAddr or DevEUI, and are hidden in the WebUI until you ask for them; forwarding is never affected
- **MQTT publishing** — optionally publish every captured message as JSON to an MQTT broker over TCP or TLS (including mutual TLS), with a configurable topic template
- **Self-forwarding protection** — refuses to start with an upstream that is its own port, and cuts any packet loop that still happens at runtime
- **Persistent storage** — SQLite database with configurable retention; databases from earlier versions are upgraded in place
- **Real-time WebUI** — React 19 SPA with WebSocket feed, live/pause toggle, client-side filters (device or label, gateway, MType, status, time range), sortable table, column visibility, color-coded signal quality (SNR thresholds are spreading-factor-aware), dark/light theme, CSV export
- **Device labels** — give a device a name from the WebUI ("Qingping T&H Havana" instead of `00E8A80B`); the label is stored in the database, shown on every row of that device, found by the device filter, included in the CSV export, and updated in every open WebUI at once
- **Single binary** — React frontend is embedded at compile time; no separate web server needed
- **Self-contained** — ZeroMQ and SQLite are compiled into the binary; no runtime system dependencies

## Quick Start (Docker)

```bash
# Copy and edit the example config
cp sniffer.toml my-sniffer.toml

# Build the image
docker build -t lorawan-sniffer .

# Run with Docker Compose
docker compose up -d

# Open the WebUI
xdg-open http://localhost:8888
```

Point your LoRaWAN gateway at `<host>:1700` instead of the LNS directly. Set `GATEWAY_UDP_UPSTREAM_SERVER` to your LNS to forward the traffic; leave it unset to run in [listen mode](#listen-mode-no-lns) instead.

## Install as a service (bare metal)

Each release has an archive for x86_64 and one for aarch64 (arm64) Linux, holding the static binary, a systemd service and an install script. This suits running the sniffer on the same machine as the LNS. Pick the archive that matches `uname -m` on the target:

```bash
VERSION=2.1.0
ARCH=$(uname -m)    # x86_64 or aarch64
BASE=https://github.com/xoseperez/lorawan-packet-sniffer/releases/download/v$VERSION
curl -LO $BASE/lorawan-sniffer-$VERSION-linux-$ARCH.tar.gz
curl -LO $BASE/SHA256SUMS
sha256sum --check --ignore-missing SHA256SUMS

tar xzf lorawan-sniffer-$VERSION-linux-$ARCH.tar.gz
cd lorawan-sniffer-$VERSION-linux-$ARCH
sudo ./install.sh
```

`make dist` builds the same archives locally, in `dist/`.

`install.sh` puts the files in place and reloads systemd; it never starts or restarts the service:

| Path | Contents |
|---|---|
| `/usr/local/bin/lorawan-sniffer` | the program |
| `/etc/systemd/system/lorawan-sniffer.service` | the service |
| `/etc/lorawan-sniffer/sniffer.toml` | configuration; an existing one is kept, and the new template is written next to it as `sniffer.toml.new` |
| `/etc/lorawan-sniffer/sniffer.env` | secrets, readable by root only; kept on upgrade |
| `/var/lib/lorawan-sniffer/` | the database, created by the service |

Review the configuration, then start the service:

```bash
sudoedit /etc/lorawan-sniffer/sniffer.toml
sudo systemctl enable --now lorawan-sniffer
journalctl -u lorawan-sniffer -f
```

The WebUI is then at `http://<host>:8888`. It has no authentication, so anyone who can reach that port sees all captured traffic and can edit [device labels](#device-labels); on a machine reachable from untrusted networks, set `bind = "127.0.0.1:8888"` in `[sniffer.api]` and use an SSH tunnel or a reverse proxy. Reach it by IP address or `localhost`; to use a host name, list the name in `allowed_hosts` first (see [Host check](#host-check)).

### Next to the LNS on the same machine

The installed configuration is set up for this without touching the LNS. The LNS keeps UDP port 1700; the sniffer listens on 1702 and forwards to `127.0.0.1:1700`:

```
gateway ──▶ host:1702 (sniffer) ──▶ 127.0.0.1:1700 (LNS)
```

Point a gateway's packet forwarder at port 1702 to sniff it; gateways still sending to 1700 reach the LNS directly, as before.

To sniff every gateway without reconfiguring any of them, swap the ports: move the LNS's UDP listener to a loopback port (for ChirpStack Gateway Bridge, `udp_bind = "127.0.0.1:1701"` in `[backend.semtech_udp]`), then set `bind = "0.0.0.0:1700"` in `[gateway.udp.downstream]` and `port = 1701` in `[gateway.udp.upstream]`. Either way the LNS sees each gateway as its own peer, coming from `127.0.0.1` with a port of its own.

### Secrets

`sniffer.toml` is readable by every local user. Put the MQTT password in `/etc/lorawan-sniffer/sniffer.env` instead, which systemd reads as root before starting the sniffer:

```bash
# /etc/lorawan-sniffer/sniffer.env (mode 0600)
MQTT_PASSWORD='p@ss"word\with$symbols'
```

and refer to it from `sniffer.toml` as `password = "$MQTT_PASSWORD"`. Single-quoted values are taken literally.

### About the service

- It runs as an unprivileged user that systemd creates on the fly, inside a sandbox: it can write only to `/var/lib/lorawan-sniffer`, holds no capabilities and sees a private `/tmp`. Ports below 1024 are therefore not available.
- `systemctl stop` (SIGTERM) makes the sniffer save the records still queued for the database and exit, within a few seconds.
- It is restarted automatically if it crashes.
- **Concentratord mode** needs Concentratord's ZeroMQ sockets in the real `/tmp` and permission to open them. Install the override shipped in the archive: `sudo mkdir -p /etc/systemd/system/lorawan-sniffer.service.d && sudo cp concentratord.conf /etc/systemd/system/lorawan-sniffer.service.d/`, then `sudo systemctl daemon-reload && sudo systemctl restart lorawan-sniffer`. It runs the sniffer as root with a shared `/tmp`.

### Upgrade and removal

```bash
# Upgrade: unpack the new archive, then
sudo ./install.sh
sudo systemctl restart lorawan-sniffer
diff /etc/lorawan-sniffer/sniffer.toml /etc/lorawan-sniffer/sniffer.toml.new   # new options, if any

# Remove the program and the service (configuration and database are kept)
sudo ./install.sh --uninstall
```

## Deployment (Semtech UDP)

There are two ways to place the sniffer between the gateways and the LNS, plus listen mode when there is no LNS at all. In every case the gateway's packet forwarder is configured to send to the sniffer's UDP port instead of the LNS.

### On the gateway (single gateway)

Run the sniffer next to the packet forwarder on the gateway itself. Point the forwarder at the sniffer (for example `127.0.0.1:1700`) and set the sniffer's upstream to the real LNS. Nothing changes for the LNS.

### In front of the LNS (many gateways)

Run the sniffer on or near the LNS and point every gateway at the sniffer's address instead of the LNS. In proxy mode the sniffer keeps one session per gateway EUI, and each session owns its own UDP socket towards the LNS. The LNS therefore sees every gateway as a separate peer (its own source port), exactly as if the gateways talked to it directly, and can address downlinks to each of them.

- Packets are forwarded byte for byte; the sniffer never rewrites tokens. PUSH_ACK, PULL_ACK and PULL_RESP go back to the gateway the LNS was talking to, even when two gateways use the same token at the same time.
- A session follows the gateway's EUI, not its address: a gateway that changes IP address or port (NAT, restart) keeps its session and its LNS-side peer.
- A gateway that has been silent for `session_timeout` (default `5m`) has its upstream socket and routing state released, and gets a fresh session when it sends again. Packet forwarders send a keep-alive every 10 seconds by default, so a live gateway never reaches the timeout.
- At most `max_gateways` (default `100`) gateways are handled at once. Packets from new gateways beyond the limit are dropped with a rate-limited warning; the gateways already connected are not affected.

Concentratord mode always talks to the one gateway it runs on; its gateway ID is recorded on every message and gateway-specific blocking rules apply to it.

### Listen mode (no LNS)

Set `mode = "listen"` in `[gateway.udp]` (the Docker image does this by itself when `GATEWAY_UDP_UPSTREAM_SERVER` is not set). The sniffer records, broadcasts and publishes whatever any number of gateways send, and sends nothing upstream.

- By default it answers each PUSH_DATA with a PUSH_ACK and each PULL_DATA with a PULL_ACK, so the gateways see a healthy link. Set `listen_acks = false` to stay completely silent. It never sends downlinks.
- Captured messages have the status `Captured` (a neutral grey dot in the WebUI, with its own status filter).
- Blocking rules have no effect in listen mode, since nothing is forwarded; a warning is logged at startup if any are configured.

### Self-forwarding protection

In proxy mode the sniffer refuses to start if the upstream address is its own gateway-facing port, reached through any of the machine's addresses (loopback or not), and says why. If a loop happens anyway, for example through NAT or Docker port forwarding, the looped packets are detected at runtime, dropped with an error log, and never recorded, published or forwarded again.

### Tested with

Multi-gateway routing follows the behavior of the Semtech reference packet forwarder (PUSH_DATA sent from one socket, PULL_DATA and TX_ACK from another); other packet forwarders have not been tested.

<!-- Gateway models, firmware and LNS used in field validation go here. -->

## Building from Source

**Requirements:** Rust (stable), Node.js 20+, npm

```bash
# Build WebUI + Rust binary (debug)
make build-full

# Run with example config
make run
```

## Cross-Compilation

Cross-compiled release binaries are produced with [`cross`](https://github.com/cross-rs/cross).

```bash
# Install cross (once)
cargo install cross --git https://github.com/cross-rs/cross

# Build for both targets (builds WebUI once first)
make build-all

# Or individually:
make build-x86_64-unknown-linux-musl
make build-aarch64-unknown-linux-musl
```

Output: `target/<triple>/release/lorawan-sniffer`

Binaries are statically linked against musl libc. ZeroMQ and SQLite are compiled from source and embedded; the binary has no runtime dependencies.

```bash
# Release archives for both targets, plus SHA256SUMS, in dist/
make dist
```

`make dist` builds the WebUI and both binaries, then packs each binary with the systemd service, the install script and the configuration templates from `packaging/` (see [Install as a service](#install-as-a-service-bare-metal)). The archives are reproducible: building the same commit twice gives identical files.

## Docker

```bash
# Build an image from source (x86_64 only; needs only Docker)
make docker-build

# Build the release image for the host architecture from the dist/ binaries
make docker-build-release

# Build and push the multi-arch release image (amd64 + arm64) to a registry
DOCKER_IMAGE=ghcr.io/yourname/lorawan-sniffer DOCKER_TAG=latest make docker-buildx
```

The release image (`docker/Dockerfile.release`) contains the same binaries as the release archives. It compiles nothing, so building the arm64 image on an x86_64 machine needs no emulation. `docker stop` stops the sniffer cleanly within a second or so.

## Releases

Releases are built by GitHub Actions (`.github/workflows/release.yml`) when a version tag is pushed.

1. Set the new version in `lorawan-sniffer/Cargo.toml`, run `cargo check` so `Cargo.lock` follows, and run `npm version X.Y.Z --no-git-tag-version` in `webui/` (updates `package.json` and `package-lock.json`).
2. In `CHANGELOG.md`, move the `[Unreleased]` entries into a new `## [X.Y.Z] - YYYY-MM-DD` section, keep an empty `[Unreleased]` above it, and update the link references at the bottom.
3. Commit and push to `master`, then tag that commit and push the tag: `git tag vX.Y.Z && git push origin vX.Y.Z`

The workflow refuses to release if the tag is not exactly `v` plus the version in `Cargo.toml`, if the tagged commit is not on `master`, if `CHANGELOG.md` has no section for the version, or if the tests fail. It then publishes:

- a GitHub Release `vX.Y.Z` whose notes are the changelog section, with `lorawan-sniffer-X.Y.Z-linux-x86_64.tar.gz`, `lorawan-sniffer-X.Y.Z-linux-aarch64.tar.gz` and `SHA256SUMS`;
- the multi-arch (amd64 + arm64) image `ghcr.io/xoseperez/lorawan-packet-sniffer` tagged `X.Y.Z`, `X.Y` and `latest`.

A version with a suffix (`2.2.0-rc.1`) is published as a pre-release and only gets its full-version image tag. To test the pipeline without releasing anything, run the workflow by hand (Actions > Release > Run workflow): it builds and packages everything, builds the image without pushing it, and keeps the archives as the `release-files` artifact of the run.

## Docker Environment Variables

When running with Docker, the sniffer can be configured entirely via environment variables in your `docker-compose.yml` (or `docker run -e`). All variables have sensible defaults, so the service starts even if none are set.

| Variable | Description | Default |
|---|---|---|
| `SNIFFER_LOG_LEVEL` | Log verbosity (`TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR`) | `INFO` |
| `SNIFFER_API_BIND` | Address and port for the WebUI / HTTP API | `0.0.0.0:8888` |
| `SNIFFER_API_ALLOWED_HOSTS` | Comma-separated host names (not IP addresses, which always work) the WebUI and API may be reached by, e.g. `sniffer.example.com,sniffer.lan`; see [Host check](#host-check) | empty |
| `SNIFFER_DATABASE_PATH` | Path to the SQLite database file | `/var/lib/lorawan-sniffer/sniffer.db` |
| `SNIFFER_DATABASE_RETENTION` | How long to keep messages (e.g. `30days`, `12hours`) | `30days` |
| `GATEWAY_PROTOCOL` | Backend protocol (`udp` or `concentratord`) | `udp` |
| `GATEWAY_UDP_DOWNSTREAM_BIND` | Address the gateway sends packets to | `0.0.0.0:1700` |
| `GATEWAY_UDP_MODE` | UDP mode: `proxy` (forward to the LNS) or `listen` (no LNS, record only) | `listen` when `GATEWAY_UDP_UPSTREAM_SERVER` is unset or empty, otherwise `proxy` |
| `GATEWAY_UDP_LISTEN_ACKS` | Listen mode only: answer PUSH_DATA / PULL_DATA with PUSH_ACK / PULL_ACK | `true` |
| `GATEWAY_UDP_SESSION_TIMEOUT` | Release a gateway's session after it has been silent this long | `5m` |
| `GATEWAY_UDP_MAX_GATEWAYS` | Maximum number of gateways handled at the same time | `100` |
| `GATEWAY_UDP_UPSTREAM_SERVER` | LNS hostname or IP to forward packets to. Leave it unset to run in listen mode | `localhost` (only used in proxy mode) |
| `GATEWAY_UDP_UPSTREAM_PORT` | LNS UDP port | `1700` |
| `GATEWAY_CONCENTRATORD_DOWNSTREAM_EVENT_URL` | ZMQ URL to subscribe to concentratord events | `ipc:///tmp/concentratord_event` |
| `GATEWAY_CONCENTRATORD_DOWNSTREAM_COMMAND_URL` | ZMQ URL to send commands to concentratord | `ipc:///tmp/concentratord_command` |
| `GATEWAY_CONCENTRATORD_UPSTREAM_EVENT_BIND` | ZMQ URL to publish events to upstream consumers | `ipc:///tmp/sniffer_event` |
| `GATEWAY_CONCENTRATORD_UPSTREAM_COMMAND_BIND` | ZMQ URL to accept commands from upstream consumers | `ipc:///tmp/sniffer_command` |
| `SNIFFER_MQTT_ENABLED` | Publish captured messages to an MQTT broker | `false` |
| `SNIFFER_MQTT_SERVER` | Broker hostname or IP | `localhost` |
| `SNIFFER_MQTT_PORT` | Broker port | `1883` |
| `SNIFFER_MQTT_TLS` | Connect over TLS | `false` |
| `SNIFFER_MQTT_CA_FILE` | PEM file with the CA(s) used to verify the broker; empty = bundled public CA roots | empty |
| `SNIFFER_MQTT_CLIENT_CERT_FILE` | PEM client certificate (mutual TLS; set together with the key) | empty |
| `SNIFFER_MQTT_CLIENT_KEY_FILE` | PEM client private key (mutual TLS; set together with the certificate) | empty |
| `SNIFFER_MQTT_CLIENT_ID` | MQTT client identifier | `lorawan-sniffer` |
| `SNIFFER_MQTT_USERNAME` | Broker username | empty |
| `SNIFFER_MQTT_PASSWORD` | Broker password | empty |
| `SNIFFER_MQTT_TOPIC` | Topic template (see [MQTT publishing](#mqtt-publishing)) | `lorawan-sniffer/{gateway_eui}/{direction}/{mtype}` |
| `SNIFFER_MQTT_QOS` | MQTT QoS: `0`, `1` or `2` | `0` |

You only need to set the variables relevant to your chosen protocol. For example, a minimal UDP setup:

```yaml
environment:
  GATEWAY_PROTOCOL: "udp"
  GATEWAY_UDP_UPSTREAM_SERVER: "10.0.0.1"
  GATEWAY_UDP_UPSTREAM_PORT: "1700"
```

Without `GATEWAY_UDP_UPSTREAM_SERVER` the image starts in listen mode, recording what the gateways send and forwarding nothing:

```yaml
environment:
  GATEWAY_PROTOCOL: "udp"
```

Values from environment variables are inserted into the configuration literally, so a password (or any other value) may contain any character, including `"`, `\` and `$`.

To use custom filter rules or advanced options not covered by these variables, volume-mount a full TOML config file to `/etc/lorawan-sniffer/sniffer.toml` (this overrides the built-in template).

## Configuration

The sniffer is configured via a TOML file. Run `lorawan-sniffer configfile` to print an annotated template.

```toml
[sniffer]
  log_level     = "INFO"   # TRACE | DEBUG | INFO | WARN | ERROR
  log_to_syslog = false

[sniffer.api]
  bind = "0.0.0.0:8888"
  # cors_origins = ["https://example.com"]   # default: only the sniffer's own WebUI
  # allowed_hosts = ["sniffer.example.com"]  # host names the WebUI may be reached by;
                                             # IP addresses and localhost always work

[sniffer.database]
  path      = "/var/lib/lorawan-sniffer/sniffer.db"
  retention = "30days"

[gateway]
  protocol = "udp"   # "udp" | "concentratord"

# --- UDP Packet Forwarder mode ---
[gateway.udp]
  mode            = "proxy"   # "proxy" | "listen" (no LNS, record only)
  listen_acks     = true      # listen mode: answer PUSH_DATA / PULL_DATA with ACKs
  session_timeout = "5m"      # release a gateway that has been silent this long
  max_gateways    = 100       # maximum number of gateways at once

[gateway.udp.downstream]
  bind = "0.0.0.0:1700"   # gateways send packets here

[gateway.udp.upstream]    # proxy mode only
  server = "localhost"
  port   = 1700            # real LNS address (must not be the sniffer's own port)

# --- Optional blocking rules (first match wins; no rules = forward everything) ---
# [[filters]]
#   name         = "Block confirmed uplinks"
#   enabled      = true
#   direction    = "up"               # "up" | "down" | "both"
#   suppress_ack = false              # true = stealth (suppress PUSH_ACK / PULL_ACK)
#   mtype        = ["ConfirmedDataUp"]
#   gateways     = ["AA:BB:CC:DD:00:11:22:33"]   # optional: only these gateways

# --- Optional MQTT publishing (disabled by default) ---
# [sniffer.mqtt]
#   enabled = true
#   server  = "localhost"
#   port    = 1883
#   topic   = "lorawan-sniffer/{gateway_eui}/{direction}/{mtype}"
```

See `sniffer.toml` for the full example including Concentratord IPC configuration and every MQTT key.

Configuration files written for earlier versions keep working unchanged: every new setting has a default, and the default mode is `proxy`.

### UDP settings

All keys live in `[gateway.udp]` and apply to the Semtech UDP backend.

| Key | Default | Description |
|---|---|---|
| `mode` | `"proxy"` | `"proxy"` forwards between the gateways and the LNS in `[gateway.udp.upstream]`; `"listen"` only records (see [Listen mode](#listen-mode-no-lns)) |
| `listen_acks` | `true` | Listen mode only: answer PUSH_DATA with PUSH_ACK and PULL_DATA with PULL_ACK. `false` = stay silent |
| `session_timeout` | `"5m"` | Idle time after which a gateway's session (upstream socket and routing state) is released |
| `max_gateways` | `100` | Maximum number of gateways handled at once (at least 1), in both modes |

### Blocking rules

Filter rules are evaluated in order; the first matching enabled rule wins. A packet with no matching rule is forwarded transparently.

| Field | Type | Description |
|---|---|---|
| `name` | string | Human-readable label shown in logs and the WebUI |
| `enabled` | bool | Toggle without removing the rule |
| `direction` | `"up"` \| `"down"` \| `"both"` | Which direction to match |
| `mtype` | string[] | LoRaWAN MTypes to match (empty = match all). A rule with a list never matches a [malformed frame](#malformed-frames) |
| `suppress_ack` | bool | Also suppress protocol-level ACKs (stealth mode) |
| `gateways` | string[] | Optional. Gateway EUIs the rule is limited to (empty or omitted = all gateways) |

Valid MType values: `JoinRequest`, `JoinAccept`, `UnconfirmedDataUp`, `UnconfirmedDataDown`, `ConfirmedDataUp`, `ConfirmedDataDown`, `RejoinRequest`, `Proprietary`.

A rule with a `gateways` list only matches messages received from, or addressed to, one of those gateways, so you can block traffic through one gateway and leave the others alone. EUIs are accepted with or without colons or dashes, in any case (`AA:BB:CC:DD:00:11:22:33`, `aabbccdd00112233`); an invalid EUI stops the sniffer at startup, naming the rule. A message whose gateway is unknown never matches a rule that lists gateways. Rules have no effect in UDP listen mode.

A rule that lists `mtype` values never matches a [malformed frame](#malformed-frames), because the type of such a frame is only its first byte read as a header, which means nothing for noise. Rules without an `mtype` list (direction and gateway only) still match malformed frames.

## MQTT publishing

When enabled, every captured message (both directions, forwarded or blocked, and in listen mode as well) is also published to an MQTT broker as JSON, with the same fields as a record from `/api/messages`. It works with both gateway protocols and is disabled by default.

```toml
[sniffer.mqtt]
  enabled          = false
  server           = "localhost"
  port             = 1883
  tls              = false
  ca_file          = ""
  client_cert_file = ""
  client_key_file  = ""
  client_id        = "lorawan-sniffer"
  username         = ""
  password         = ""
  topic            = "lorawan-sniffer/{gateway_eui}/{direction}/{mtype}"
  qos              = 0
```

| Key | Default | Description |
|---|---|---|
| `enabled` | `false` | Turn publishing on |
| `server`, `port` | `"localhost"`, `1883` | Broker address |
| `tls` | `false` | Connect over TLS |
| `ca_file` | `""` | PEM file with the CA certificate(s) used to verify the broker. Empty = the public CA roots bundled in the binary. When set, it replaces them |
| `client_cert_file`, `client_key_file` | `""` | PEM client certificate and private key for mutual TLS. Set both or neither |
| `client_id` | `"lorawan-sniffer"` | MQTT client identifier |
| `username`, `password` | `""` | Optional credentials. Neither is ever written to the logs |
| `topic` | `"lorawan-sniffer/{gateway_eui}/{direction}/{mtype}"` | Topic template |
| `qos` | `0` | `0`, `1` or `2` |

**Topic template.** These placeholders are replaced for every message:

| Placeholder | Value |
|---|---|
| `{gateway_eui}` | Gateway EUI, e.g. `AA:BB:CC:DD:00:11:22:33` |
| `{direction}` | `up` or `down` |
| `{mtype}` | Message type, e.g. `JoinRequest` |
| `{status}` | `Forwarded`, `Blocked` or `Captured` |
| `{dev_addr}` | Device address |
| `{dev_eui}` | Device EUI |

A value the message does not have (a DevAddr on a join request, for example) is replaced by `unknown`. A template without placeholders sends everything to one topic. An unknown placeholder or an MQTT wildcard (`+` or `#`) is a startup error, as are a QoS outside 0–2, an unreadable certificate or key file with TLS on, and a client certificate without its key (or the other way round).

**TLS.** Set `tls = true`. The broker certificate is always verified: against the bundled public CA roots by default, or against `ca_file` instead, which is what a broker with a private CA needs. Setting `client_cert_file` and `client_key_file` enables mutual TLS. There is no option to skip verification.

**Delivery.** Publishing is best effort and never slows down forwarding. Each record is handed to the MQTT client at most once; while the broker is connected and keeping up, it reaches the broker exactly once. When the broker is unreachable or too slow, the records that cannot be sent are dropped and counted instead of being queued without limit, a rate-limited warning is logged, and the sniffer keeps forwarding and reconnects on its own. The sniffer starts normally when the broker is down. Every connection uses a clean MQTT session, so nothing is replayed after a reconnect; with QoS 1 or 2 the broker may still receive a duplicate of a message that was in flight when the connection dropped, as MQTT allows.

## Concentratord Mode (ZMQ IPC)

Set `protocol = "concentratord"` in your config and configure the ZMQ URLs:

```toml
[gateway.concentratord.downstream]
  event_url   = "ipc:///tmp/concentratord_event"
  command_url = "ipc:///tmp/concentratord_command"

[gateway.concentratord.upstream]
  event_bind   = "ipc:///tmp/sniffer_event"
  command_bind = "ipc:///tmp/sniffer_command"
```

Configure your ChirpStack Gateway Bridge to connect to the sniffer's sockets instead of concentratord's directly.

Concentratord runs on the gateway itself, so this mode always handles one gateway; its gateway ID is recorded as the gateway EUI of every message.

When running in Docker, the sniffer container must share the host IPC namespace or a tmpfs volume with concentratord so the ZMQ IPC sockets are accessible. See the comments in `docker-compose.yml`.

## Malformed frames

Some gateways forward radio packets that are not LoRaWAN frames at all: noise the radio picked up, or packets whose CRC check failed. Decoded as if they were real, they appeared as every message type, with made-up DevAddrs, FCnts and DevEUIs, and cluttered the table and the device filter. The sniffer now recognises the frames that cannot be valid LoRaWAN 1.0 or 1.1, marks each one as **malformed** with the reason, and decodes nothing else from it. Forwarding does not change: every frame still reaches its destination byte for byte.

### What counts as malformed

A frame is malformed when any of these rules holds. The reason recorded is the first that applies, in this order:

| Reason | Rule |
|---|---|
| `crc` | The gateway reported a failed CRC (`stat = -1` in Semtech UDP, `BAD_CRC` in Concentratord) |
| `mhdr` | The MHDR (first byte) has non-zero RFU bits (4:2) or Major version bits (1:0). LoRaWAN 1.0 and 1.1 frames always have both at 0 |
| `length` | The length does not fit the type: a JoinRequest is not 23 bytes, a JoinAccept is not 17 or 33, a RejoinRequest is not 19 or 24, or a data frame (confirmed or unconfirmed, up or down) is shorter than 12 bytes plus its FOptsLen |

- **Proprietary frames** are exempt from the length rule only. A vendor frame whose first byte has non-zero RFU or Major bits is marked `mhdr` and hidden by default.
- The rules apply in both directions. A malformed downlink from the LNS is marked too, although CRC only applies to what a gateway receives.
- To classify a frame the sniffer reads the first byte and, for data frames of 6 bytes or more, the FOptsLen bits of FCtrl. Nothing else is decoded from a malformed frame.

What is **not** malformed:

- A frame whose CRC status is "no CRC". A source that does not report CRC must not have all its traffic hidden.
- A frame with FOpts and FPort 0. It breaks the protocol, but real devices send it.
- A valid frame received with a downlink type. The sniffer does not check the type against the direction.
- A payload that cannot be read at all (empty, or invalid base64 in Semtech UDP). It has no first byte, so the `mhdr` and `length` rules cannot apply. It is recorded as type `Unknown`, marked `crc` if the gateway reported a failed CRC and left unmarked otherwise, and has no identifier either way.

### What a malformed record holds

Every record has a `malformed` field: `null` for a valid frame, otherwise `"crc"`, `"mhdr"` or `"length"`. A malformed record keeps the type (the first byte read as an MHDR, so for noise it means nothing), the size, the airtime, the status, the gateway and the radio data (frequency, data rate, RSSI, SNR, CRC status). The twelve fields decoded from the frame body are `null`:

`dev_addr`, `dev_eui`, `join_eui`, `dev_nonce`, `fcnt`, `fport`, `adr`, `adr_ack_req`, `ack`, `class_b`, `f_pending` and `f_opts_len`.

Malformed records are stored, sent on `/ws`, returned by `/api/messages` and published to MQTT like any other record. The CSV export has a `malformed` column (`crc`, `mhdr`, `length` or empty) before the `label` column. At DEBUG level, the log line of a malformed frame ends with `malformed=<reason>`.

Because the identifiers are never produced, a malformed frame cannot put a made-up DevAddr or DevEUI in the device filter, in the list of identifiers seen live, in [device labels](#what-can-be-labelled) or in an MQTT topic.

### Forwarding and blocking rules

Forwarding and ACKs behave exactly as before for malformed frames, in both gateway protocols.

The one change to [blocking rules](#blocking-rules): a rule with an `mtype` list never matches a malformed frame. Without this, a rule blocking JoinRequests would block about one noise frame in eight. Rules with no `mtype` list (direction and gateway only) still match malformed frames. In Semtech UDP, when a rule blocks one frame of a PUSH_DATA the whole datagram is held back; that is unchanged, but noise can no longer cause a type rule to hold back the valid frames that share its datagram.

### In the WebUI

- Malformed rows are **hidden by default**. A **Show malformed** checkbox at the end of the Type menu shows them. It is off by default and the browser remembers it, like the other filters.
- Shown malformed rows are dimmed, and their type badge is dashed and struck through, with the hover text `Malformed: <reason>`. The Device cell, empty on these rows, says why in short words, so it can be read on a touch screen: `malformed: CRC failed`, `malformed: bad header` or `malformed: wrong length`. Hovering gives the full reason: "CRC check failed", "not a LoRaWAN 1.0/1.1 header" or "wrong length for a join request / join accept / rejoin request / data uplink / data downlink / frame".
- The status bar shows **Malformed hidden: N** while the box is off and **Malformed shown: N** while it is on: how many malformed rows there are among the loaded rows that pass the other filters. Total and Blocked count every loaded row, malformed ones included.
- The other filters (device, gateway, type, status, time) apply to malformed rows as well when they are shown, and the CSV export covers the rows shown.
- Filtering is done in the browser, which loads the newest 500 records and keeps at most 10,000. Hidden malformed rows still take their place in those limits, so on a noisy gateway fewer useful rows are loaded than the numbers suggest.

### MQTT consumers

Malformed records are published like any other. A consumer that does not want noise should skip the messages whose payload has a non-null `malformed`. The `{dev_addr}` and `{dev_eui}` topic placeholders render `unknown`, as for any missing value. An `{mtype}` topic still carries the first-byte type of noise, so a subscriber to one type's topic can receive malformed frames of that type; check `malformed` in the payload.

### Gateway settings that forward CRC-failed packets

A gateway passes on packets that failed their CRC check only when it is set to. Whether to turn that off is up to you; the sniffer only marks them.

- **Semtech packet forwarder:** in `gateway_conf` of `global_conf.json`, `forward_crc_error` makes the forwarder pass on packets with a bad CRC and `forward_crc_disabled` passes on packets sent without a CRC. Set them to `false` to stop forwarding them.
- **ChirpStack Concentratord:** `disable_crc_filter` in the `[concentratord]` section of its configuration. The default `false` drops frames whose CRC failed. `true` lets them through, together with frames that have no CRC.

### Existing databases: the first start after the upgrade

The database goes to schema version 3 on the first start. The upgrade runs in place and keeps every row; it adds the `malformed` column and an internal flag that says whether a row has been checked.

The sniffer then checks the rows already stored, **before it starts forwarding**, and marks those that are malformed:

- It can apply two rules to stored rows: `crc`, from the stored CRC status, and `length`, from the stored type, size and FOptsLen. It clears the body-derived fields of the rows it marks. The `mhdr` rule cannot be applied, because the first byte is not stored. Rows written by 2.0.0 have no FOptsLen, so only the 12-byte minimum applies to their data frames.
- **Time.** Every stored row is rewritten once, so the time grows with the database. For 1,000,000 rows (a 344 MB database) it took about 4 s on a development machine with NVMe storage. Slower storage, such as an SD card or eMMC, takes longer. Forwarding starts when the check is done.
- **Free disk space.** SQLite's write-ahead log grows while the check runs. Keep at least as much free space as the database file takes (1.0× its size) on the disk that holds it. This figure comes from the peak log size measured on 1,000,000 rows (0.64× the database), plus 50 % headroom.
- **Log.** Before the check starts the sniffer says how much it will do, so a long first start is not mistaken for a hang:

  ```
  Database schema upgraded from version 2 to 3
  Checking every stored row for malformed frames (about N rows); forwarding starts when this is done
  Checked N stored rows, M malformed, in T ms
  ```

  A new database logs "Database created at schema version 3" instead of the first line.
- **If the check fails** (a full disk, for example), the sniffer logs `Could not check stored rows for malformed frames: <error>; will retry at the next start`, starts and forwards anyway, and tries again on the next start. Until one succeeds, old rows keep their made-up identifiers in the WebUI, the API and the label checks.
- **If only the clean-up fails.** The check grows the write-ahead log and shrinks it again once it is saved. If the shrinking fails, the check stays done and is not repeated. The sniffer logs the warning `Checked the stored rows, but could not shrink the write-ahead log: <error>; it is removed when the sniffer next stops cleanly`, and the `-wal` file next to the database keeps its space until the sniffer next stops cleanly.
- Rows this version stores are checked as they are recorded, so every later start, including one after a crash or a power cut, has nothing to check and adds no measurable delay.

An identifier that appeared only in malformed rows has no stored record after the check. A label on it follows the normal [lifetime rule](#how-long-a-label-lives).

**After a rollback.** An older version can run on an upgraded database; it ignores the new columns. The rows it records meanwhile are not marked as checked, so they keep their made-up identifiers and have no `malformed` value. The next start of this version finds exactly those rows, however the table changed in between (rows deleted, `VACUUM`, several rollbacks), checks them the same way and logs `Checking N stored rows recorded by an older version for malformed frames`. Rolling back is therefore a matter of reinstalling the previous binary.

## Device labels

The WebUI shows a device only by its DevAddr (`00E8A80B`), or by its DevEUI on join requests. A label is a short name you attach to that identifier, such as "Qingping T&H Havana", so you do not have to remember which hex address belongs to which device. Labels are set from the WebUI and stored in the sniffer's database, so they survive restarts. They are not part of the captured records: `/api/messages`, the `/ws` feed and the MQTT payloads are unchanged, and nothing about labels touches forwarding.

### What can be labelled

A label belongs to the identifier the Device column shows:

- the **DevAddr** on data frames, uplinks and downlinks alike (`00E8A80B`);
- the **DevEUI** on join requests (`36:31:39:54:36:5D:05:39`).

A record never has both. Rows with no identifier have an empty Label cell that cannot be edited: join accepts, rejoin requests, proprietary frames and [malformed frames](#malformed-frames), which include join and data frames too short to be valid. A join accept is encrypted, so the sniffer cannot tell which DevEUI got which DevAddr, and it does not decode the DevEUI of rejoin requests.

A malformed frame has no identifier, so it never enters the list of identifiers seen live and never makes an identifier labellable: a `PUT` for the made-up DevAddr of a noise frame gets `404`.

### Editing a label

1. Switch the **Label** column on in the column picker. It is hidden by default, and the browser remembers the choice.
2. Click or tap the Label cell of a row. A dialog opens with the identifier, a text field with the current label, and Save, Delete (only when the device has a label) and Cancel buttons.
3. Press Enter or Save to save. Escape, Cancel or a click outside the dialog closes it without saving. Saving an empty label removes it. If saving fails (network error, invalid label, limit reached), the dialog stays open and says why.

Every row with that identifier changes at once, past and live, in this browser and in every other open WebUI, with no reload. A WebUI that loses its connection gets the current labels again when it reconnects. If two people edit the same label at almost the same time, the last write wins everywhere.

The **device filter** also matches labels: typing `havana` keeps the rows whose label, DevAddr or DevEUI contains it, in any case. The **CSV export** has a `label` column after the existing ones, filled for labelled rows even when the Label column is hidden. The label is always written in double quotes, with inner quotes doubled. A label that starts with `=`, `+`, `-` or `@` gets a leading apostrophe in the CSV (`'=1+1`), so a spreadsheet opens it as text and not as a formula; the apostrophe may be visible there. The label itself is not changed in the WebUI or the API.

Labels are not added to MQTT payloads. Other programs can read them from `GET /api/labels` (see [Label endpoints](#label-endpoints)).

### Limits

A label is Unicode text, trimmed, at most 64 characters and with no control characters. At most 10,000 labels exist at once. A label too long for its cell is cut off with an ellipsis, and the full text shows as a tooltip.

### How long a label lives

- A label is kept as long as its identifier has a record in the database.
- A label whose identifier has no stored record is kept while the identifier was seen in live traffic within the retention period (`retention` in `[sniffer.database]`).
- Otherwise the retention sweep removes it, and the removal reaches every open WebUI.
- Nothing is removed during the first retention period after the sniffer starts, so a restart never removes a label: each one gets a full retention period for its device to be stored or heard again. That period starts again with every start, so a sniffer restarted more often than once per retention period keeps labels whose identifier has no stored record indefinitely. The 10,000-label limit bounds how many of these can pile up.

An identifier can be labelled if it has a record in the database or was seen live within the retention period. The second case covers rows that reached the WebUI but were never saved, because the database writer drops records under load rather than slow forwarding down. To allow it, the sniffer keeps an in-memory list of the identifiers it has seen live, with the time each was last seen. The list is not saved, and it is best effort: under extreme load it can miss an identifier, which becomes labellable again with its next stored or observed record. It holds at most 100,000 identifiers; beyond that, new ones are not added until older ones expire, and a warning is logged. An identifier that is neither stored nor in the list is refused with `404`.

### Labels follow identifiers, not devices

An OTAA device gets a new DevAddr every time it joins, and a DevAddr can later be reused by another device. The label stays with the DevAddr, not with the physical device, so after a rejoin the device shows up unlabelled and has to be labelled again. A device's DevEUI label (on its join requests) and its DevAddr label (on its data frames) are also separate labels; set both if you want to see the name on both kinds of row.

### Who can edit labels

There is no authentication. Anyone who can open the WebUI can add, change and remove labels, just as they can already read all the captured traffic. The [Host and Origin checks](#cross-origin-access) only keep other web sites out; they are not a login.

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

All messages are normalized into a unified `MessageRecord` before storage and broadcast, regardless of the underlying protocol.

### API endpoints

| Method | Path | Description |
|---|---|---|
| `GET` | `/ws` | WebSocket — real-time message feed (`message` events only) |
| `GET` | `/ws/labels` | WebSocket — the device labels, then every change |
| `GET` | `/api/messages` | REST — historical messages (`since`, `limit` query params) |
| `GET` | `/api/labels` | REST — every device label |
| `PUT` | `/api/labels/{device}` | REST — set or replace a device label |
| `DELETE` | `/api/labels/{device}` | REST — remove a device label |
| `GET` | `/*` | Embedded React SPA |

Every request, including the WebSockets, must pass the [Host check](#host-check).

### Label endpoints

`{device}` is an identifier in the same text form as the records: `00E8A80B` for a DevAddr, `36:31:39:54:36:5D:05:39` for a DevEUI. On input, upper or lower case is accepted, and a DevEUI may be written with colons, dashes or no separators. Responses and the label list always use the record form.

```bash
# every label, as {"<device>": "<label>", ...}
curl http://localhost:8888/api/labels

# set or replace a label (an empty label removes it)
curl -X PUT -H 'Content-Type: application/json' \
     -d '{"label": "Qingping T&H Havana"}' \
     http://localhost:8888/api/labels/00E8A80B

# remove a label
curl -X DELETE http://localhost:8888/api/labels/00E8A80B
```

`PUT` takes a JSON body, `{"label": "..."}`. The label is trimmed and checked against the [limits](#limits). It answers:

| Status | Meaning |
|---|---|
| `200` | Saved. Body `{"device": "00E8A80B", "label": "Qingping T&H Havana"}`, with `"label": null` when an empty label removed it |
| `400` | Malformed identifier, a body that is not `{"label": "<text>"}`, or a label longer than 64 characters or containing control characters |
| `403` | The request's `Origin` names another site (see [Cross-origin access](#cross-origin-access)) |
| `404` | The identifier has no stored record and was not seen live within the retention period |
| `409` | 10,000 labels already exist and this identifier has none; replacing or removing a label always works |
| `415` | The request has no `Content-Type: application/json` |
| `500` | The database failed |

`DELETE` answers `204` with no body, also when the label did not exist, and `400` or `403` as above. Error bodies are `{"error": "<message>"}`; a refused Host is plain text instead.

**`/ws/labels`** keeps a WebUI in step with the database. Messages from the server:

```json
{"type": "labels", "data": {"00E8A80B": "Qingping T&H Havana", "36:31:39:54:36:5D:05:39": "Havana sensor"}}
{"type": "label",  "data": {"device": "00E8A80B", "label": "Qingping T&H Havana"}}
{"type": "label",  "data": {"device": "00E8A80B", "label": null}}
```

On connect it sends the full set as a `labels` message. After that it sends one `label` message per change, in the order the changes were saved, with `"label": null` for a removal. That includes changes made by the API and labels removed by the retention sweep. A change that reached the server is always published, even if the client that sent it gave up waiting for the answer. A client that falls behind receives a fresh `labels` message instead of the changes it missed, so it always ends up matching the database. The `/ws` feed is unchanged and carries no label messages.

### Message records

Every record returned by `/api/messages` and pushed on `/ws` carries these fields in addition to the existing ones. All of them are `null` when they do not apply to the message, and the existing fields are unchanged.

| Field | Present on | Format |
|---|---|---|
| `gateway_eui` | All records | Colon-separated uppercase hex, e.g. `AA:BB:CC:DD:00:11:22:33` |
| `join_eui` | JoinRequest | Same format as the DevEUI |
| `dev_nonce` | JoinRequest | Integer (16 bit) |
| `adr` | Data frames, both directions | Boolean |
| `adr_ack_req` | Uplink data frames | Boolean |
| `ack` | Data frames, both directions | Boolean |
| `class_b` | Uplink data frames | Boolean |
| `f_pending` | Downlink data frames | Boolean |
| `f_opts_len` | Data frames, both directions | Integer (0–15) |
| `malformed` | All records | `null` for a valid frame, otherwise `"crc"`, `"mhdr"` or `"length"` (see [Malformed frames](#malformed-frames)) |

FCtrl bit 4 is `class_b` on uplinks and `f_pending` on downlinks; flags that do not apply to a frame's direction or type are `null`, not `false`. On a malformed record, all twelve fields decoded from the frame body (`dev_addr`, `dev_eui`, `join_eui`, `dev_nonce`, `fcnt`, `fport`, the five flags and `f_opts_len`) are `null`. The `status` field can now also be `Captured` (UDP listen mode, in addition to `Forwarded` and `Blocked`).

The WebUI has matching Gateway (last 8 hex digits, full EUI in the tooltip), JoinEUI, DevNonce, flags and FOptsLen columns in the column picker, a gateway filter, and the new fields in the CSV export (`malformed` is the column before `label`).

### Cross-origin access

The API and the live feeds have no authentication, so by default only the sniffer's own WebUI can use them from a browser:

- The API sends no CORS headers, so other sites cannot read it.
- A WebSocket handshake (`/ws`, `/ws/labels`) or a label write (`PUT`, `DELETE`) must carry no `Origin`, an `Origin` that names the host the request was sent to (one of the sniffer's own pages), or one listed in `cors_origins`. Anything else is refused with 403.
- Writes use `PUT` (with a JSON body) and `DELETE`, which no HTML form can send; a `PUT` without `Content-Type: application/json` gets 415.

Without this, any web page open in a browser that can reach the sniffer could read the captured traffic or change labels. Clients that are not browsers (curl, scripts) send no `Origin`; they are accepted on purpose, and only the Host check below applies to them.

To let a dashboard on another origin use the API and both live feeds, list it. A listed origin may also write labels: the CORS response allows `GET`, `PUT` and `DELETE` with a JSON `Content-Type`.

```toml
[sniffer.api]
  cors_origins = ["https://dashboard.example.com"]
```

#### Host check

Those rules do not stop DNS rebinding: a web page points its own domain name at the sniffer's address, so the browser treats the page as same-origin and lets it read the captured traffic and change labels. The sniffer therefore also checks the `Host` of every request to the WebUI, the REST API and both WebSockets:

- An IP address (IPv4 or IPv6, any port) or `localhost` always works.
- A host name works only when it is listed in `allowed_hosts` in `[sniffer.api]`.
- Any other `Host` gets a `403` with a plain-text message that names the host and the setting to change, for example: `Host 'sniffer.lan:8888' is not allowed. Add "sniffer.lan" to [sniffer.api] allowed_hosts (SNIFFER_API_ALLOWED_HOSTS in Docker); IP addresses and localhost always work.`

```toml
[sniffer.api]
  allowed_hosts = ["sniffer.example.com", "sniffer.lan"]
```

In Docker, set `SNIFFER_API_ALLOWED_HOSTS=sniffer.example.com,sniffer.lan`; the setting also accepts a comma-separated string in the TOML file. Entries are bare host names, with no scheme, port or path. The port of a request is ignored, so `sniffer.lan` covers `sniffer.lan:8888`. An invalid entry stops the sniffer at startup.

**Upgrading:** versions before this one answered to any `Host`. If you open the WebUI by IP address or `localhost`, nothing changes. If you use a host name, add it to `allowed_hosts`.

`allowed_hosts = ["*"]` turns the check off, and with it the protection against DNS rebinding. It is **unsafe**: use it only when neither of the reverse-proxy set-ups below fits.

**Behind a reverse proxy**, what matters is which `Host` reaches the sniffer. There are two ways to do it, shown here for nginx.

*Set-up A: pass the original `Host` through and list the public name.* The Host the sniffer sees is the public name, which also matches the browser's `Origin`, so no `cors_origins` entry is needed.

```toml
[sniffer.api]
  allowed_hosts = ["sniffer.example.com"]
```

```nginx
server {
    listen 443 ssl;
    server_name sniffer.example.com;
    # ssl_certificate / ssl_certificate_key ...

    location / {
        proxy_pass http://127.0.0.1:8888;
        proxy_set_header Host $http_host;    # the client's Host, port included
    }

    location /ws {                           # also covers /ws/labels
        proxy_pass http://127.0.0.1:8888;
        proxy_set_header Host $http_host;
        proxy_http_version 1.1;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection "upgrade";
        proxy_read_timeout 1h;               # /ws/labels is silent between edits
    }
}
```

*Set-up B: let the proxy send an IP address as `Host` and list the public origin in `cors_origins`.* An IP `Host` always passes the check, and the browser's `Origin` no longer matches it, so the public origin has to be listed.

```toml
[sniffer.api]
  cors_origins = ["https://sniffer.example.com"]
```

```nginx
    location / {
        proxy_pass http://127.0.0.1:8888;
        proxy_set_header Host 127.0.0.1:8888;
    }

    location /ws {                           # also covers /ws/labels
        proxy_pass http://127.0.0.1:8888;
        proxy_set_header Host 127.0.0.1:8888;
        proxy_http_version 1.1;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection "upgrade";
        proxy_read_timeout 1h;
    }
```

These two `location` blocks replace the ones in set-up A's `server` block. Without a `proxy_set_header Host` line, nginx sends the address from `proxy_pass`, which is set-up B when that is an IP address; if `proxy_pass` uses a host name (a Docker service name, say), that name is what the sniffer sees and it has to be listed. In set-up B the sniffer sees only the IP address, so it can no longer tell names apart: the proxy must answer only for its own `server_name`, so keep this `server` block from being the `default_server`.

## Known limitations

- **Gateway EUIs cannot be trusted.** The Semtech UDP protocol has no authentication, so anyone who can reach the sniffer's UDP port can claim any gateway EUI, and in proxy mode redirect that gateway's downlinks to themselves. Protect the port at the network level (firewall, VPN, private network); do not expose it to untrusted networks. This is a research and debugging tool.
- Only Semtech UDP supports several gateways; Concentratord mode handles the one gateway it runs on. Semtech Basic Station is not supported.
- In proxy mode all gateways are forwarded to the one configured LNS.
- Listen mode never sends downlinks and cannot block anything.
- FOpts MAC commands are not decoded; only their length is exported.
- **Some noise stays visible.** The checks only catch frames that cannot be valid LoRaWAN. Non-LoRaWAN LoRa traffic with a good CRC, a first byte that reads as a valid header and a length that happens to fit looks like real traffic. The sniffer cannot check the MIC, which needs the session keys.
- **Old rows have no `mhdr` reason.** The first byte is not stored, so when the stored rows are checked after the upgrade only `crc` and `length` can be applied.
- **Label editing has no authentication.** Anyone who can open the WebUI can add, change and remove [device labels](#device-labels). The Host and Origin checks keep other web sites out, not other people. Restrict who can reach the port (bind to `127.0.0.1`, a firewall, a VPN, or a reverse proxy that asks for a login).

## Testing

```bash
cargo test
```

The tests include loopback tests for multi-gateway routing, listen mode and the loop protection, and MQTT tests against an in-process fake broker.

Two helper scripts live in `tools/`:

- `tools/gwsim.py` — a Semtech UDP simulator (Python 3, standard library only): N simulated gateways, each with separate push and pull sockets like a real packet forwarder, plus a fake LNS that sends ACKs and downlinks. Use it to try the sniffer without hardware or to run a 100-gateway load test; the usage examples are at the top of the file. `--joins N` makes each gateway also send a JoinRequest every N uplinks, from a fixed DevEUI per gateway that is printed at start, so there are DevEUI rows to label. `--random-devaddr` gives every data uplink a new random DevAddr, which fills the sniffer's list of identifiers seen live. The fake LNS counts the frames it receives by message type, so a run can show that the JoinRequests arrived. `--malformed-ratio R` (0 to 1, default 0) makes that fraction of the PUSH_DATA datagrams carry one more frame that breaks a LoRaWAN rule, taken in turn from a fixed list of ten that covers every reason, including one frame that breaks all three rules (counted as `crc`). The fake DevAddrs and DevEUI such frames would carry if they were trusted (`6874FA22`, `D47263A5`, `16D03003`, `16D03004`, `0BADADD0`, `16D03005` and `DE:AD:00:00:00:00:00:20`) are printed at start, so you can check that none of them shows up in a record or the device filter. The summaries count the frames by reason (`malformed crc / mhdr / length a / b / c`); the fake LNS keeps them out of its type and identifier tallies, never sends a downlink to their DevAddr, and reports the datagrams that are not Semtech UDP packets as `invalid datagrams` (it called them `malformed` before).
- `tools/procstat.sh` — prints the peak and current memory (`VmHWM`, `VmRSS`) and the open file descriptors of a running process, for example `tools/procstat.sh "$(pidof lorawan-sniffer)" 10`.

## License

MIT
