#!/bin/bash
set -euo pipefail

cd "$(dirname "$0")"
cargo build --target wasm32-unknown-unknown --release
cp target/wasm32-unknown-unknown/release/local_tdata_integration.wasm ../local_tdata_integration.wasm
