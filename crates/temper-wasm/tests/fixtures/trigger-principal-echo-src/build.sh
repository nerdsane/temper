#!/bin/bash
set -euo pipefail

cd "$(dirname "$0")"
cargo build --target wasm32-unknown-unknown --release
cp target/wasm32-unknown-unknown/release/trigger_principal_echo.wasm ..
echo "Copied to: ../trigger_principal_echo.wasm"
