#!/usr/bin/env bash
# Build all Crucible WASM modules.
#
# Usage:
#   ./build.sh              # build only
#   ./build.sh --upload     # build + upload to a running temper-server
#
# Environment:
#   TEMPER_URL   — server URL          (default: http://127.0.0.1:3000)
#   TENANT       — tenant id           (default: crucible)

set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

MODULES=(crucible_cron_trigger crucible_scheduler_check crucible_scheduler_heartbeat)

for mod in "${MODULES[@]}"; do
    echo "==> Building $mod ..."
    (cd "$SCRIPT_DIR/$mod" && cargo build --target wasm32-unknown-unknown --release 2>&1)
    WASM="$SCRIPT_DIR/$mod/target/wasm32-unknown-unknown/release/${mod}.wasm"
    SIZE=$(wc -c < "$WASM" | tr -d ' ')
    echo "    Built: $WASM ($SIZE bytes)"
done

if [ "${1:-}" = "--upload" ]; then
    TEMPER_URL="${TEMPER_URL:-http://127.0.0.1:3000}"
    TENANT="${TENANT:-crucible}"

    for mod in "${MODULES[@]}"; do
        WASM="$SCRIPT_DIR/$mod/target/wasm32-unknown-unknown/release/${mod}.wasm"
        echo "==> Uploading $mod to $TEMPER_URL ..."
        RESP=$(curl -sS -w "\n%{http_code}" \
            -X POST \
            -H "X-Tenant-Id: $TENANT" \
            -H "Content-Type: application/wasm" \
            -H "X-Temper-Principal-Kind: admin" \
            --data-binary "@$WASM" \
            "$TEMPER_URL/api/wasm/modules/$mod")
        HTTP_CODE=$(echo "$RESP" | tail -1)
        BODY=$(echo "$RESP" | sed '$d')
        if [ "$HTTP_CODE" -ge 200 ] && [ "$HTTP_CODE" -lt 300 ]; then
            echo "    OK (HTTP $HTTP_CODE)"
        else
            echo "    FAILED (HTTP $HTTP_CODE): $BODY"
            exit 1
        fi
    done
fi

echo "==> Done."
