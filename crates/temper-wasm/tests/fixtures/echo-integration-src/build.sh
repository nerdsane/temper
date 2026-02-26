#!/bin/bash
# Build the echo-integration WASM module.
# Requires: rustup target add wasm32-unknown-unknown
set -euo pipefail

cd "$(dirname "$0")"
cargo build --target wasm32-unknown-unknown --release
echo "Built: target/wasm32-unknown-unknown/release/echo_integration.wasm"
cp target/wasm32-unknown-unknown/release/echo_integration.wasm .
echo "Copied to: echo_integration.wasm"
