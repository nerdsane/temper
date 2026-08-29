#!/bin/bash
set -euo pipefail

cd "$(dirname "$0")"
cargo build --target wasm32-unknown-unknown --release
cp target/wasm32-unknown-unknown/release/generated_scoped_data_integration.wasm ../generated_scoped_data_integration.wasm
