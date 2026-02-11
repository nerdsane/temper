#!/bin/bash
# Item 6: Session Exit Gate (Stop)
# BLOCKING: YES (exit 2 if unverified pushes or compilation errors)
#
# Before Claude Code session ends, checks:
# 1. If push-pending marker exists, test-verified marker must also exist
# 2. cargo check --workspace compiles clean
set -euo pipefail

cat > /dev/null

WORKSPACE_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"

# --- Check 1: Unverified pushes ---
PROJECT_HASH="$(echo "$WORKSPACE_ROOT" | shasum -a 256 | cut -c1-12)"
MARKER_DIR="/tmp/temper-harness/${PROJECT_HASH}"
SESSION_ID="${CLAUDE_SESSION_ID:-}"

PUSH_BLOCKED=false
if [ -d "$MARKER_DIR" ]; then
    # Check all push-pending markers (not just current session)
    for pending in "$MARKER_DIR"/push-pending-*; do
        [ -f "$pending" ] || continue
        MARKER_SESSION="$(basename "$pending" | sed 's/push-pending-//')"
        if [ ! -f "$MARKER_DIR/test-verified-${MARKER_SESSION}" ]; then
            echo "BLOCKED: git push was made but tests have not passed!" >&2
            echo "Run 'cargo test --workspace' and ensure all tests pass before exiting." >&2
            PUSH_BLOCKED=true
            break
        fi
    done

    # Clean up verified markers on successful exit
    if [ "$PUSH_BLOCKED" = false ]; then
        rm -f "$MARKER_DIR"/push-pending-* "$MARKER_DIR"/test-verified-* 2>/dev/null || true
    fi
fi

# --- Check 2: Workspace compilation ---
echo "Running pre-exit workspace check..." >&2
COMPILE_OK=true
if ! (cd "$WORKSPACE_ROOT" && cargo check --workspace 2>&1 >&2); then
    echo "BLOCKED: Workspace has compilation errors!" >&2
    echo "Fix all compilation errors before exiting the session." >&2
    COMPILE_OK=false
fi

# Exit with blocking code if either check failed
if [ "$PUSH_BLOCKED" = true ] || [ "$COMPILE_OK" = false ]; then
    exit 2
fi

echo "Session exit gate: OK" >&2
exit 0
