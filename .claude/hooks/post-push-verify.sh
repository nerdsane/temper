#!/bin/bash
# Item 5: Post-Push Verification (PostToolUse — Bash)
# BLOCKING: No (advisory, but coordinates with Stop hook via markers)
#
# After git push, runs cargo test --workspace and writes session markers
# so the Stop hook (Item 6) can verify tests were run.
set -euo pipefail

PAYLOAD="$(cat)"

# Extract the command from bash tool input
if command -v jq >/dev/null 2>&1; then
    CMD="$(echo "$PAYLOAD" | jq -r '.tool_input.command // empty')"
else
    CMD="$(echo "$PAYLOAD" | grep -o -m1 '"command"[[:space:]]*:[[:space:]]*"[^"]*"' | head -1 | sed 's/.*"command"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/' || true)"
fi

# Only run verification after git push
case "${CMD:-}" in
    *"git push"*)
        WORKSPACE_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"

        # Create marker directory using project hash for isolation
        PROJECT_HASH="$(echo "$WORKSPACE_ROOT" | shasum -a 256 | cut -c1-12)"
        MARKER_DIR="/tmp/temper-harness/${PROJECT_HASH}"
        mkdir -p "$MARKER_DIR"

        # Session ID from environment or generate one
        SESSION_ID="${CLAUDE_SESSION_ID:-$(date +%s)}"

        # Write push-pending marker
        echo "$(date -u +%Y-%m-%dT%H:%M:%SZ)" > "$MARKER_DIR/push-pending-${SESSION_ID}"

        echo "Running post-push verification..." >&2
        if (cd "$WORKSPACE_ROOT" && cargo test --workspace 2>&1 | tail -20 >&2); then
            # Tests passed — write verified marker
            TIMESTAMP="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
            echo "$TIMESTAMP" > "$MARKER_DIR/test-verified-${SESSION_ID}"
            # Write TOML marker with test output summary
            if [ -x "$WORKSPACE_ROOT/scripts/pow-write-marker.sh" ]; then
                bash "$WORKSPACE_ROOT/scripts/pow-write-marker.sh" "test-verified-${SESSION_ID}" "pass" \
                    "trigger=post-push"
            fi
            echo "Post-push verification: ALL TESTS PASSED" >&2
        else
            echo "Post-push verification: TESTS FAILED" >&2
            echo "Session exit will be blocked until tests pass (Item 6)." >&2
        fi
        ;;
esac
exit 0
