#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

for module in channel_connect route_message send_reply; do
    echo "Building $module..."
    (cd "$SCRIPT_DIR/$module" && cargo build --target wasm32-unknown-unknown --release)
    echo "  -> $module built successfully"
done

echo ""
echo "All Temper channel WASM modules built. Binaries at:"
for module in channel_connect route_message send_reply; do
    wasm_file="$SCRIPT_DIR/$module/target/wasm32-unknown-unknown/release/${module}.wasm"
    if [ ! -f "$wasm_file" ]; then
        wasm_file="$SCRIPT_DIR/$module/target/wasm32-unknown-unknown/release/$(echo "$module" | tr '_' '-').wasm"
    fi
    if [ -f "$wasm_file" ]; then
        size=$(wc -c < "$wasm_file" | tr -d ' ')
        echo "  $module: $(( size / 1024 ))KB"
    fi
done
