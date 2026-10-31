WEBUI_DIR  := webui
WEBUI_DIST := $(WEBUI_DIR)/dist

DOCKER_IMAGE ?= lorawan-sniffer
DOCKER_TAG   ?= latest

DIST_DIR := dist

.PHONY: build build-full run fmt clippy test \
	webui-install webui-build webui-dev \
	build-x86_64-unknown-linux-musl \
	build-aarch64-unknown-linux-musl \
	build-all dist \
	docker-build docker-build-release docker-buildx

# ── WebUI ───────────────────────────────────────────────────────────────────

# Install Node dependencies (run once, or after package.json changes).
webui-install:
	cd $(WEBUI_DIR) && npm install

# Build the React app into webui/dist/ (embedded into the Rust binary).
webui-build: webui-install
	cd $(WEBUI_DIR) && npm run build

# Start the Vite dev server (proxies /ws and /api to localhost:8888).
webui-dev: webui-install
	cd $(WEBUI_DIR) && npm run dev

# ── Rust ────────────────────────────────────────────────────────────────────

# Native debug build (no webui rebuild; use build-full to include it).
build:
	cargo build

# Build webui first, then the Rust binary (embeds the fresh frontend).
build-full: webui-build
	cargo build

# Run locally with default config.
run:
	cargo run --bin lorawan-sniffer -- -c sniffer.toml

# Print example configuration.
configfile:
	cargo run --bin lorawan-sniffer -- configfile

fmt:
	cargo fmt

clippy:
	cargo clippy

test:
	cargo test

# ── Cross-compile release binaries ──────────────────────────────────────────

build-x86_64-unknown-linux-musl: webui-build
	cross build --target x86_64-unknown-linux-musl --release

build-aarch64-unknown-linux-musl: webui-build
	cross build --target aarch64-unknown-linux-musl --release

# Build all release targets (webui built once, shared by both).
build-all: webui-build
	cross build --target x86_64-unknown-linux-musl --release
	cross build --target aarch64-unknown-linux-musl --release

# ── Release archives ────────────────────────────────────────────────────────

# Release archives for both targets plus SHA256SUMS in dist/, and the
# binaries laid out for docker/Dockerfile.release.
dist: build-all
	rm -rf $(DIST_DIR)
	packaging/package.sh x86_64-unknown-linux-musl $(DIST_DIR)
	packaging/package.sh aarch64-unknown-linux-musl $(DIST_DIR)
	cd $(DIST_DIR) && sha256sum *.tar.gz > SHA256SUMS

# ── Docker ───────────────────────────────────────────────────────────────────

# Build a Docker image from source (x86_64 only; no toolchain needed locally).
docker-build:
	docker build -t $(DOCKER_IMAGE):$(DOCKER_TAG) .

# Build the release image for the host architecture from the dist/ binaries.
docker-build-release: dist
	docker buildx build -f docker/Dockerfile.release --load \
		-t $(DOCKER_IMAGE):$(DOCKER_TAG) .

# Build and push the multi-arch release image (amd64 + arm64) from the dist/
# binaries; the same binaries as the release archives, no emulation needed.
# Requires: docker buildx create --use
docker-buildx: dist
	docker buildx build -f docker/Dockerfile.release \
		--platform linux/amd64,linux/arm64 \
		-t $(DOCKER_IMAGE):$(DOCKER_TAG) \
		--push .
