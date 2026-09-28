#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

module="dsf_investigate"
echo "Building $module..."
(cd "$SCRIPT_DIR/$module" && cargo build --target wasm32-unknown-unknown --release)
cp "$SCRIPT_DIR/$module/target/wasm32-unknown-unknown/release/${module}.wasm" \
   "$SCRIPT_DIR/$module/${module}.wasm"
echo "  -> $module built successfully"
