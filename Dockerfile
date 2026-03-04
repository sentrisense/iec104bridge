# SPDX-License-Identifier: GPL-3.0-or-later
# Multi-stage Rust build.
#
# Dependency caching strategy (no cargo-chef):
#   1. Copy only Cargo.toml + Cargo.lock and build a stub binary.
#      Docker caches this layer; it is only invalidated when dependencies change.
#   2. Copy the real source, touch main.rs to force a re-link, and build the
#      final release binary reusing the cached dependency artefacts.
#
# lib60870 (a build-dep) compiles a C library via CMake, so cmake + a C
# compiler must be available in the builder stage.

# ─── Stage 1 – builder ────────────────────────────────────────────────────────
FROM rust:1 AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
        cmake \
        build-essential \
        clang \
        libclang-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# ── Cache dependencies ────────────────────────────────────────────────────────
# Copy only the manifest files and create a stub src/main.rs so that
# `cargo build` compiles all dependencies without touching real source.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src \
    && echo 'fn main() {}' > src/main.rs \
    && cargo build --release \
    && rm -rf src

# ── Build the real binary ─────────────────────────────────────────────────────
COPY src/ src/
# Touch main.rs so cargo knows it changed (avoids the "nothing to compile" cache hit).
RUN touch src/main.rs && cargo build --release

# ─── Stage 2 – minimal runtime ────────────────────────────────────────────────
FROM debian:trixie-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder /build/target/release/iec104bridge /usr/local/bin/iec104bridge
# Include the example messages so the image works standalone in file mode.
COPY examples/ /app/examples/

# ── Defaults (all overridable via env / docker-compose) ───────────────────────
ENV INPUT_FILE=/app/examples/messages.jsonl \
    IEC104_CA=1 \
    IEC104_PORT=2404 \
    IEC104_BIND_ADDR=0.0.0.0 \
    RUST_LOG=iec104bridge=info

EXPOSE 2404

ENTRYPOINT ["/usr/local/bin/iec104bridge"]
