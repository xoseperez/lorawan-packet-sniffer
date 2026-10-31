# LoRaWAN Packet Sniffer v2

A transparent MITM monitoring proxy for LoRaWAN gateways.

Sits between one or more gateways and a LoRaWAN Network Server (LNS), capturing and decoding all uplinks and downlinks in real time, with an optional selective message-blocking engine. It can also run without any LNS, just recording what the gateways send.

## Features

- **Dual-protocol support** — Semtech UDP Packet Forwarder and ChirpStack Concentratord (ZMQ/Protobuf)
- **Full MITM proxy** — captures both uplinks and downlinks without modifying them
- **Multiple gateways** — one sniffer instance can sit in front of an LNS serving a fleet of Semtech UDP gateways; every uplink, downlink and ACK reaches the right gateway, and the LNS sees each gateway as its own UDP peer
- **Listen mode** — no LNS needed: the sniffer records what any number of gateways send (optionally acknowledging them) and forwards nothing
- **Rich message metadata** — every record carries the gateway EUI, plus JoinEUI and DevNonce on join requests and the FCtrl flags (ADR, ADRACKReq, ACK, ClassB / FPending) and FOptsLen on data frames
- **Selective blocking** — block messages by MType, direction and, optionally, gateway; optional stealth mode (suppresses ACKs)
- **MQTT publishing** — optionally publish every captured message as JSON to an MQTT broker over TCP or TLS (including mutual TLS), with a configurable topic template
- **Self-forwarding protection** — refuses to start with an upstream that is its own port, and cuts any packet loop that still happens at runtime
- **Persistent storage** — SQLite database with configurable retention; databases from earlier versions are upgraded in place
- **Real-time WebUI** — React 19 SPA with WebSocket feed, live/pause toggle, client-side filters (device, gateway, MType, direction, status, time range), sortable table, column visibility, color-coded signal quality (SNR thresholds are spreading-factor-aware), dark/light theme, CSV export
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

## Docker

```bash
# Build image for the host architecture
make docker-build

# Build and push a multi-arch image (amd64 + arm64) to a registry
DOCKER_IMAGE=ghcr.io/yourname/lorawan-sniffer DOCKER_TAG=latest make docker-buildx
```

## Docker Environment Variables

When running with Docker, the sniffer can be configured entirely via environment variables in your `docker-compose.yml` (or `docker run -e`). All variables have sensible defaults, so the service starts even if none are set.

| Variable | Description | Default |
|---|---|---|
| `SNIFFER_LOG_LEVEL` | Log verbosity (`TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR`) | `INFO` |
| `SNIFFER_API_BIND` | Address and port for the WebUI / HTTP API | `0.0.0.0:8888` |
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
  # cors_origins = ["https://example.com"]   # default: allow all origins

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
| `mtype` | string[] | LoRaWAN MTypes to match (empty = match all) |
| `suppress_ack` | bool | Also suppress protocol-level ACKs (stealth mode) |
| `gateways` | string[] | Optional. Gateway EUIs the rule is limited to (empty or omitted = all gateways) |

Valid MType values: `JoinRequest`, `JoinAccept`, `UnconfirmedDataUp`, `UnconfirmedDataDown`, `ConfirmedDataUp`, `ConfirmedDataDown`, `RejoinRequest`, `Proprietary`.

A rule with a `gateways` list only matches messages received from, or addressed to, one of those gateways, so you can block traffic through one gateway and leave the others alone. EUIs are accepted with or without colons or dashes, in any case (`AA:BB:CC:DD:00:11:22:33`, `aabbccdd00112233`); an invalid EUI stops the sniffer at startup, naming the rule. A message whose gateway is unknown never matches a rule that lists gateways. Rules have no effect in UDP listen mode.

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
| `GET` | `/ws` | WebSocket — real-time message feed |
| `GET` | `/api/messages` | REST — historical messages (`since`, `limit` query params) |
| `GET` | `/*` | Embedded React SPA |

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

FCtrl bit 4 is `class_b` on uplinks and `f_pending` on downlinks; flags that do not apply to a frame's direction or type are `null`, not `false`. The `status` field can now also be `Captured` (UDP listen mode, in addition to `Forwarded` and `Blocked`).

The WebUI has matching Gateway (last 8 hex digits, full EUI in the tooltip), JoinEUI, DevNonce, flags and FOptsLen columns in the column picker, a gateway filter, and the new fields in the CSV export.

### CORS

By default all origins are allowed (the sniffer is a local/private network tool). To restrict access:

```toml
[sniffer.api]
  cors_origins = ["https://dashboard.example.com"]
```

## Known limitations

- **Gateway EUIs cannot be trusted.** The Semtech UDP protocol has no authentication, so anyone who can reach the sniffer's UDP port can claim any gateway EUI, and in proxy mode redirect that gateway's downlinks to themselves. Protect the port at the network level (firewall, VPN, private network); do not expose it to untrusted networks. This is a research and debugging tool.
- Only Semtech UDP supports several gateways; Concentratord mode handles the one gateway it runs on. Semtech Basic Station is not supported.
- In proxy mode all gateways are forwarded to the one configured LNS.
- Listen mode never sends downlinks and cannot block anything.
- FOpts MAC commands are not decoded; only their length is exported.

## Testing

```bash
cargo test
```

The tests include loopback tests for multi-gateway routing, listen mode and the loop protection, and MQTT tests against an in-process fake broker.

Two helper scripts live in `tools/`:

- `tools/gwsim.py` — a Semtech UDP simulator (Python 3, standard library only): N simulated gateways, each with separate push and pull sockets like a real packet forwarder, plus a fake LNS that sends ACKs and downlinks. Use it to try the sniffer without hardware or to run a 100-gateway load test; the usage examples are at the top of the file.
- `tools/procstat.sh` — prints the peak and current memory (`VmHWM`, `VmRSS`) and the open file descriptors of a running process, for example `tools/procstat.sh "$(pidof lorawan-sniffer)" 10`.

## License

MIT
