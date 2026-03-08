# Multi-stage Dockerfile for Temper platform server.
# Uses cargo-chef for build layer caching.

# ── Stage 1: Chef ────────────────────────────────────────────────────────
FROM rust:1-bookworm AS chef
RUN cargo install cargo-chef --locked
RUN apt-get update && apt-get install -y \
    pkg-config libssl-dev python3-dev clang libclang-dev \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app

# ── Stage 2: Planner ────────────────────────────────────────────────────
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# ── Stage 3: Builder ────────────────────────────────────────────────────
FROM chef AS builder
# Install Rust 1.92 (workspace MSRV).
RUN rustup toolchain install 1.92 && rustup default 1.92

COPY --from=planner /app/recipe.json recipe.json
# Build dependencies (cached unless Cargo.toml/lock changes).
RUN cargo chef cook --release --recipe-path recipe.json

# Build the actual binary.
COPY . .
RUN cargo build --release --bin temper

# ── Stage 4: Runtime ────────────────────────────────────────────────────
FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y \
    ca-certificates libssl3 python3 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/temper /usr/local/bin/temper

ENV RUST_LOG=info,temper=debug
EXPOSE 3000

ENTRYPOINT ["temper"]
CMD ["serve", "--port", "3000", "--storage", "turso"]
