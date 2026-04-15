#!/bin/bash
set -euo pipefail

cd "$(dirname "$0")"
cargo build --target wasm32-unknown-unknown --release
echo "Built: target/wasm32-unknown-unknown/release/sdk_context_reader.wasm"
cp target/wasm32-unknown-unknown/release/sdk_context_reader.wasm ..
echo "Copied to: ../sdk_context_reader.wasm"
