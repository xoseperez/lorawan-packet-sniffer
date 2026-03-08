# ─── Stage 1: Build the React WebUI ──────────────────────────────────────────
FROM node:20-alpine AS webui
WORKDIR /build/webui
COPY webui/package*.json ./
RUN npm ci
COPY webui/ ./
RUN npm run build

# ─── Stage 2: Build the Rust binary ──────────────────────────────────────────
#
# rust-musl-cross provides:
#   - a glibc host so proc-macro crates (cdylib) can be compiled
#   - x86_64-linux-musl-gcc / g++ — a real musl-targeting C/C++ cross-compiler
#     (avoids glibc-internal symbols like __libc_single_threaded that Debian's
#      native g++ embeds when building ZeroMQ with std::shared_ptr)
#   - the x86_64-unknown-linux-musl Rust target pre-installed
#   - CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER already set
FROM ghcr.io/rust-cross/rust-musl-cross:x86_64-musl AS builder

WORKDIR /build

# ── 2a. Dependency caching layer ──────────────────────────────────────────────
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY lorawan-sniffer/Cargo.toml lorawan-sniffer/Cargo.toml
COPY .cargo/                    .cargo/

# Stub sources and a minimal webui placeholder so rust-embed does not error.
RUN mkdir -p lorawan-sniffer/src webui/dist \
    && echo 'fn main() {}' > lorawan-sniffer/src/main.rs \
    && printf '<!DOCTYPE html><html></html>' > webui/dist/index.html

# Compile all dependencies (errors from stub code are expected and ignored).
RUN cargo build --release --bin lorawan-sniffer --target x86_64-unknown-linux-musl || true

# ── 2b. Real build ────────────────────────────────────────────────────────────
COPY lorawan-sniffer/ lorawan-sniffer/
COPY --from=webui /build/webui/dist ./webui/dist

# Touch entry points so Cargo recompiles the changed crates.
RUN touch lorawan-sniffer/src/main.rs

RUN cargo build --release --bin lorawan-sniffer --target x86_64-unknown-linux-musl

# ─── Stage 3: Minimal runtime image ──────────────────────────────────────────
FROM alpine:3.20

# ZeroMQ, SQLite, and all other dependencies are compiled into the binary;
# no extra packages are needed at runtime.

COPY --from=builder /build/target/x86_64-unknown-linux-musl/release/lorawan-sniffer /usr/bin/lorawan-sniffer
COPY docker/sniffer.toml /etc/lorawan-sniffer/sniffer.toml
COPY docker/docker-entrypoint.sh /usr/bin/docker-entrypoint.sh
RUN chmod +x /usr/bin/docker-entrypoint.sh

RUN mkdir -p /var/lib/lorawan-sniffer

# WebUI / HTTP API
EXPOSE 8888
# LoRaWAN gateway UDP port (Semtech protocol)
EXPOSE 1700/udp

# Database is stored here; mount a named volume for persistence.
VOLUME ["/var/lib/lorawan-sniffer"]

ENTRYPOINT ["/usr/bin/docker-entrypoint.sh"]
CMD ["/usr/bin/lorawan-sniffer", "-c", "/etc/lorawan-sniffer/sniffer.toml"]
