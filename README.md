# LoRaWAN Packet Sniffer v2

A transparent MITM monitoring proxy for LoRaWAN gateways.

Sits between a gateway and a LoRaWAN Network Server (LNS), capturing and decoding all uplinks and downlinks in real time, with an optional selective message-blocking engine.

## Features

- **Dual-protocol support** — Semtech UDP Packet Forwarder and ChirpStack Concentratord (ZMQ/Protobuf)
- **Full MITM proxy** — captures both uplinks and downlinks without modifying them
- **Selective blocking** — block messages by MType and direction; optional stealth mode (suppresses ACKs)
- **Persistent storage** — SQLite database with configurable retention
- **Real-time WebUI** — React 19 SPA with WebSocket feed, live/pause toggle, client-side filters (device, MType, direction, status, time range), sortable table, column visibility, color-coded signal quality (SNR thresholds are spreading-factor-aware), dark/light theme, CSV export
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

Point your LoRaWAN gateway at `<host>:1700` instead of the LNS directly.

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
| `GATEWAY_UDP_UPSTREAM_SERVER` | LNS hostname or IP to forward packets to | `localhost` |
| `GATEWAY_UDP_UPSTREAM_PORT` | LNS UDP port | `1700` |
| `GATEWAY_CONCENTRATORD_DOWNSTREAM_EVENT_URL` | ZMQ URL to subscribe to concentratord events | `ipc:///tmp/concentratord_event` |
| `GATEWAY_CONCENTRATORD_DOWNSTREAM_COMMAND_URL` | ZMQ URL to send commands to concentratord | `ipc:///tmp/concentratord_command` |
| `GATEWAY_CONCENTRATORD_UPSTREAM_EVENT_BIND` | ZMQ URL to publish events to upstream consumers | `ipc:///tmp/sniffer_event` |
| `GATEWAY_CONCENTRATORD_UPSTREAM_COMMAND_BIND` | ZMQ URL to accept commands from upstream consumers | `ipc:///tmp/sniffer_command` |

You only need to set the variables relevant to your chosen protocol. For example, a minimal UDP setup:

```yaml
environment:
  GATEWAY_PROTOCOL: "udp"
  GATEWAY_UDP_UPSTREAM_SERVER: "10.0.0.1"
  GATEWAY_UDP_UPSTREAM_PORT: "1700"
```

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
[gateway.udp.downstream]
  bind = "0.0.0.0:1700"   # gateway sends packets here

[gateway.udp.upstream]
  server = "localhost"
  port   = 1700            # real LNS address

# --- Optional blocking rules (first match wins; no rules = forward everything) ---
# [[filters]]
#   name         = "Block confirmed uplinks"
#   enabled      = true
#   direction    = "up"               # "up" | "down" | "both"
#   suppress_ack = false              # true = stealth (suppress PUSH_ACK / PULL_ACK)
#   mtype        = ["ConfirmedDataUp"]
```

See `sniffer.toml` for the full example including Concentratord IPC configuration.

### Blocking rules

Filter rules are evaluated in order; the first matching enabled rule wins. A packet with no matching rule is forwarded transparently.

| Field | Type | Description |
|---|---|---|
| `name` | string | Human-readable label shown in logs and the WebUI |
| `enabled` | bool | Toggle without removing the rule |
| `direction` | `"up"` \| `"down"` \| `"both"` | Which direction to match |
| `mtype` | string[] | LoRaWAN MTypes to match (empty = match all) |
| `suppress_ack` | bool | Also suppress protocol-level ACKs (stealth mode) |

Valid MType values: `JoinRequest`, `JoinAccept`, `UnconfirmedDataUp`, `UnconfirmedDataDown`, `ConfirmedDataUp`, `ConfirmedDataDown`, `RejoinRequest`, `Proprietary`.

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

### CORS

By default all origins are allowed (the sniffer is a local/private network tool). To restrict access:

```toml
[sniffer.api]
  cors_origins = ["https://dashboard.example.com"]
```

## License

MIT
