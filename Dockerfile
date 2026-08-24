# Multi-stage Dockerfile for Temper platform server.
# Uses cargo-chef for build layer caching.

# ── Stage 1: Chef ────────────────────────────────────────────────────────
FROM rust:1-bookworm AS chef
RUN cargo install cargo-chef --locked
# Match the repository toolchain in every build stage. Otherwise the planner
# downloads it again and the source copy invalidates the cooked dependency cache.
RUN rustup toolchain install nightly-2026-02-08 --profile minimal \
        --component clippy,rustfmt \
    && rustup default nightly-2026-02-08
RUN apt-get update && apt-get install -y \
    pkg-config libssl-dev python3-dev clang libclang-dev libjemalloc-dev mold \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app

# ── Stage 2: Planner ────────────────────────────────────────────────────
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# ── Stage 3: Builder ────────────────────────────────────────────────────
FROM chef AS builder
# Use the low-memory parallel linker alongside the repository's distribution
# profile, whose ThinLTO bounds LLVM memory during the final application build.
ENV CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C link-arg=-fuse-ld=mold"

COPY --from=planner /app/recipe.json recipe.json
# cargo-chef's recipe recreates workspace members, but not patched path
# dependencies that live outside the workspace.
COPY third-party/libsql-0.9.29-temper third-party/libsql-0.9.29-temper
# Build dependencies (cached unless Cargo.toml/lock changes).
RUN cargo chef cook --profile dist --recipe-path recipe.json

# Build the actual binary.
COPY . .
RUN cargo build --profile dist --bin temper

# ── Stage 4: Runtime ────────────────────────────────────────────────────
FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y \
    ca-certificates libssl3 python3 libz3-4 libjemalloc2 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/dist/temper /usr/local/bin/temper

ENV RUST_LOG=info,temper=info
EXPOSE 3000

# No ENTRYPOINT — Railway's startCommand provides the full command.
CMD ["temper", "serve", "--port", "3000", "--storage", "turso"]
