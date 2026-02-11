#!/bin/bash
set -euo pipefail
PAYLOAD="$(cat)"
# Extract the command from bash tool input
if command -v jq >/dev/null 2>&1; then
    CMD="$(echo "$PAYLOAD" | jq -r '.tool_input.command // empty')"
else
    # Fallback: extract command field. Use -m1 to avoid hanging on empty input.
    CMD="$(echo "$PAYLOAD" | grep -o -m1 '"command"[[:space:]]*:[[:space:]]*"[^"]*"' | head -1 | sed 's/.*"command"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/' || true)"
fi
# Only run verification after git push
case "${CMD:-}" in
    *"git push"*)
        WORKSPACE_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
        echo "Running post-push verification..." >&2
        cd "$WORKSPACE_ROOT" && cargo test --workspace 2>&1 | tail -5 >&2
        ;;
esac
exit 0
