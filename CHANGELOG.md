# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- WebUI login: optional shared token (`[sniffer.api] login`, `token`), login page, signed session cookies, Bearer tokens for scripts, lockout after 10 wrong tokens a minute.
- Time-range history: the time selector loads the newest 5,000 matching records of the range from the database, with a notice when more match.
- Server-side filters: device or label, type, status, gateway and malformed are applied by the database.
- `/api/messages` parameters `from`, `newest`, `count`, `settle`, `device`, `mtype`, `status`, `gateway` and `malformed`, and headers `X-Total-Count`, `X-Malformed-Count`, `X-History-Settled` and `X-Storage-Write-Failures`.
- Full reload of the WebUI after every WebSocket reconnect, with no missing records.
- WebUI unit tests (`cd webui && npm test`).
- Malformed-frame detection (`crc`, `mhdr`, `length`), with a `malformed` field on every record and a "Show malformed" option in the WebUI.
- Device labels: Label column and dialog, label API (`/api/labels`) and live label feed (`/ws/labels`).
- `allowed_hosts` setting against DNS rebinding.
- Release archives for x86_64 and aarch64 Linux (`make dist`), with `SHA256SUMS`.
- systemd service and `install.sh` (install, upgrade, `--uninstall`).
- Release Docker image built from the release binaries.
- Automated releases on `vX.Y.Z` tags: GitHub Release and multi-arch image on GHCR.
- Startup warning for configuration values that reference an unset environment variable.
- `tools/gwsim.py` options `--malformed-ratio`, `--joins` and `--random-devaddr`.
- Configuration example for running next to the LNS on the same machine.

### Changed

- **Breaking:** a WebUI or API reached by host name needs that name in `allowed_hosts`.
- **Breaking:** the API is same-origin by default; other sites must be listed in `cors_origins`.
- Database schema 4; the first start after upgrading checks every stored row for malformed frames before forwarding (time and disk space in the README).
- ⚙ Settings menu replaces ⚙ Cols and holds the column choices, CSV export and Log out.
- Time column shows the date (`MM-DD HH:MM:SS.ddd`).
- Footer shows Matching, Shown, Blocked and Malformed instead of Total, and the version.
- Malformed records carry no fields decoded from the frame body.
- Blocking rules with an `mtype` list no longer match malformed frames.
- History reads use a separate read-only database connection (WAL mode).
- SQLite write-ahead log capped at 8 MiB.
- WebSocket clients that fall behind are disconnected with code 1013.
- WebUI reconnects with growing, randomised pauses (3–60 s).
- Storage insert failures are logged and counted.
- Per-packet log lines only at DEBUG.
- Direction filter removed from the WebUI.
- Gateway filter keeps every gateway seen in the session.
- `tools/gwsim.py` "malformed" datagram counter renamed to "invalid datagrams".
- Docker images based on Alpine 3.23.
- Project moved to `github.com/xoseperez/lorawan-packet-sniffer`.

### Fixed

- Configuration syntax errors no longer print the offending line, which could hold a password or token.
- Time ranges show the whole range, not a filter over the first 500 records.
- Short noise frames no longer get identifiers.
- WebUI opens at the newest rows.
- Upstreams given by name prefer IPv4.
- SIGTERM stops the sniffer cleanly.
- Records waiting to be written are saved at shutdown.
- `make docker-buildx` builds a working arm64 image.

## [2.1.0] - 2026-10-31

### Added

- Multiple gateways in the UDP proxy (`session_timeout`, `max_gateways`).
- UDP listen mode (`mode = "listen"`).
- Gateway EUI on every record.
- JoinEUI, DevNonce, FCtrl flags and FOptsLen fields.
- Gateway-specific blocking rules (`gateways` in `[[filters]]`).
- MQTT publishing (`[sniffer.mqtt]`), with TLS and mutual TLS.
- Self-forwarding guard.
- Database schema migrations (`PRAGMA user_version`).
- `Captured` message status for listen mode.
- WebUI gateway column and filter, and optional JoinEUI, DevNonce, flags and FOptsLen columns.
- `tools/gwsim.py` and `tools/procstat.sh`.
- `docs/MOBILE.md` touchscreen report.

### Changed

- Docker image starts in listen mode when no upstream is set; new environment variables for the UDP and MQTT settings.
- Environment values are inserted into the configuration literally.
- ACKs are routed per gateway session.
- Synthetic TX_ACKs carry the EUI of the downlink's gateway.
- WebUI visual redesign: direction colours, type badges, status dots, zebra rows.
- SNR colouring depends on the spreading factor.
- `libsniffer` merged into `lorawan-sniffer` as `core/`.

### Fixed

- Default configuration no longer forwards packets to the sniffer itself.
- FPort no longer read from the MIC.
- ACK token map no longer grows without bound.
- One LNS socket error no longer stops the proxy.

## [2.0.0] - 2026-03-06

Complete rewrite of the Python v1 sniffer in Rust, as a MITM proxy for both directions and both gateway protocols.

### Added

- Semtech UDP packet forwarder proxy.
- ChirpStack Concentratord proxy (ZeroMQ).
- LoRaWAN PHY decoder.
- Message blocking rules.
- Stealth mode (`suppress_ack`).
- SQLite persistence with retention.
- Time-on-air calculation.
- REST API (`GET /api/messages`) and WebSocket feed.
- React WebUI embedded in the binary.
- CORS support (`cors_origins`).
- Syslog support.
- `configfile` subcommand.
- Static musl builds for x86_64 and aarch64.
- Docker support.

[Unreleased]: https://github.com/xoseperez/lorawan-packet-sniffer/compare/v2.1.0...HEAD
[2.1.0]: https://github.com/xoseperez/lorawan-packet-sniffer/compare/v2.0.0...v2.1.0
[2.0.0]: https://github.com/xoseperez/lorawan-packet-sniffer/releases/tag/v2.0.0
