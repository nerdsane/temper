#!/usr/bin/env bash
# Build all WASM modules for the temper-agent OS app.
# Usage: cd os-apps/temper-agent/wasm && ./build.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

for module in llm_caller tool_runner sandbox_provisioner; do
    echo "Building $module..."
    (cd "$SCRIPT_DIR/$module" && cargo build --target wasm32-unknown-unknown --release)
    echo "  -> $module built successfully"
done

echo ""
echo "All WASM modules built. Binaries at:"
for module in llm_caller tool_runner sandbox_provisioner; do
    wasm_file="$SCRIPT_DIR/$module/target/wasm32-unknown-unknown/release/${module/-/_}.wasm"
    if [ -f "$wasm_file" ]; then
        size=$(wc -c < "$wasm_file" | tr -d ' ')
        echo "  $module: $(( size / 1024 ))KB"
    else
        # cdylib uses the package name with hyphens replaced
        wasm_file="$SCRIPT_DIR/$module/target/wasm32-unknown-unknown/release/$(echo $module | tr '_' '-').wasm"
        if [ -f "$wasm_file" ]; then
            size=$(wc -c < "$wasm_file" | tr -d ' ')
            echo "  $module: $(( size / 1024 ))KB"
        fi
    fi
done
