#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

for module in signal_observer episode_orchestrator work_item_result_router; do
    echo "Building $module..."
    (cd "$SCRIPT_DIR/$module" && cargo build --target wasm32-unknown-unknown --release)
    cp "$SCRIPT_DIR/$module/target/wasm32-unknown-unknown/release/${module}.wasm" \
       "$SCRIPT_DIR/$module/${module}.wasm"
    echo "  -> $module built successfully"
done
