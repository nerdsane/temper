#!/bin/bash
# Post-Push Marker (PostToolUse — Bash)
# BLOCKING: No (advisory)
#
# After git push, records that a push happened for this session.
# Tests are handled by the pre-push git hook, so we don't re-run them here.
set -euo pipefail

PAYLOAD="$(cat)"

# Extract the command from bash tool input
if command -v jq >/dev/null 2>&1; then
    CMD="$(echo "$PAYLOAD" | jq -r '.tool_input.command // empty')"
else
    CMD="$(echo "$PAYLOAD" | grep -o -m1 '"command"[[:space:]]*:[[:space:]]*"[^"]*"' | head -1 | sed 's/.*"command"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/' || true)"
fi

# Only run after git push
case "${CMD:-}" in
    *"git push"*)
        WORKSPACE_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
        PROJECT_HASH="$(echo "$WORKSPACE_ROOT" | shasum -a 256 | cut -c1-12)"
        SESSION_ID="${CLAUDE_SESSION_ID:-default}"
        MARKER_DIR="/tmp/temper-harness/${PROJECT_HASH}/${SESSION_ID}"
        mkdir -p "$MARKER_DIR"

        PUSHED_SHA="$(cd "$WORKSPACE_ROOT" && git rev-parse HEAD 2>/dev/null || echo "unknown")"
        TIMESTAMP="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

        echo "${PUSHED_SHA} ${TIMESTAMP}" > "$MARKER_DIR/push-completed"
        echo "Post-push: recorded push of ${PUSHED_SHA}" >&2
        ;;
esac
exit 0
