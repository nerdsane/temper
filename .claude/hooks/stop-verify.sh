#!/bin/bash
# Session Exit Gate (Stop)
# BLOCKING: YES (exit 2 if unverified pushes, missing reviews, or compilation errors)
#
# Before Claude Code session ends, checks:
# 1. If push-pending marker exists, test-verified marker must also exist
# 2. If commits were made with sim-visible changes, DST review marker must exist
# 3. If commits were made, code review marker must exist
# 4. cargo check --workspace compiles clean
#
# This is the SAFETY NET. The pre-commit gate is the primary enforcement.
# This catches anything that slipped through.
set -euo pipefail

cat > /dev/null

WORKSPACE_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"

PROJECT_HASH="$(echo "$WORKSPACE_ROOT" | shasum -a 256 | cut -c1-12)"
MARKER_DIR="/tmp/temper-harness/${PROJECT_HASH}"
SESSION_ID="${CLAUDE_SESSION_ID:-}"

ANY_BLOCKED=false

# --- Check 1: Unverified pushes ---
if [ -d "$MARKER_DIR" ]; then
    for pending in "$MARKER_DIR"/push-pending-*; do
        [ -f "$pending" ] || continue
        MARKER_SESSION="$(basename "$pending" | sed 's/push-pending-//')"
        if [ ! -f "$MARKER_DIR/test-verified-${MARKER_SESSION}" ]; then
            echo "BLOCKED: git push was made but tests have not passed!" >&2
            echo "Run 'cargo test --workspace' and ensure all tests pass before exiting." >&2
            ANY_BLOCKED=true
            break
        fi
    done
fi

# --- Check 2: Review markers (safety net) ---
# If review markers were consumed (deleted after commit), that's fine.
# If they exist but are stale, the pre-commit gate already handled it.
# This check catches the case where a commit somehow bypassed the gate.
if [ -d "$MARKER_DIR" ]; then
    if [ -f "$MARKER_DIR/commit-pending" ]; then
        # A commit was made — check for review markers
        if [ -f "$MARKER_DIR/sim-changed" ]; then
            if [ ! -f "$MARKER_DIR/dst-reviewed" ]; then
                echo "BLOCKED: Simulation-visible code was committed without DST review!" >&2
                echo "Run the DST reviewer agent before exiting." >&2
                ANY_BLOCKED=true
            fi
        fi

        if [ ! -f "$MARKER_DIR/code-reviewed" ]; then
            echo "BLOCKED: Code was committed without code review!" >&2
            echo "Run a code review before exiting." >&2
            ANY_BLOCKED=true
        fi
    fi
fi

# --- Check 3: Workspace compilation ---
echo "Running pre-exit workspace check..." >&2
if ! (cd "$WORKSPACE_ROOT" && cargo check --workspace 2>&1 >&2); then
    echo "BLOCKED: Workspace has compilation errors!" >&2
    echo "Fix all compilation errors before exiting the session." >&2
    ANY_BLOCKED=true
fi

# --- Clean up on success ---
if [ "$ANY_BLOCKED" = false ] && [ -d "$MARKER_DIR" ]; then
    rm -f "$MARKER_DIR"/push-pending-* "$MARKER_DIR"/test-verified-* 2>/dev/null || true
    rm -f "$MARKER_DIR"/commit-pending "$MARKER_DIR"/sim-changed 2>/dev/null || true
    rm -f "$MARKER_DIR"/dst-reviewed "$MARKER_DIR"/code-reviewed 2>/dev/null || true
fi

if [ "$ANY_BLOCKED" = true ]; then
    exit 2
fi

echo "Session exit gate: OK" >&2
exit 0
