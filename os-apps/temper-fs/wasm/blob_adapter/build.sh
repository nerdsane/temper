#!/usr/bin/env bash
# Build the blob_adapter WASM module.
#
# Requires: rustup target add wasm32-unknown-unknown
# Output:   target/wasm32-unknown-unknown/release/blob_adapter.wasm
set -euo pipefail

cd "$(dirname "$0")"
cargo build --target wasm32-unknown-unknown --release

# Copy the built .wasm to a predictable location for tests
cp target/wasm32-unknown-unknown/release/blob_adapter.wasm ../../wasm/blob_adapter.wasm 2>/dev/null || true
echo "Built: target/wasm32-unknown-unknown/release/blob_adapter.wasm"
