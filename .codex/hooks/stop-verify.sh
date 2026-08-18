#!/bin/bash
# Session Exit Gate (Stop) — DISABLED via settings.json (Stop: [])
# Kept on disk for re-enablement.
#
# When re-enabled, uses session-scoped markers:
#   /tmp/temper-harness/{project_hash}/{session_id}/
#
# Checks:
# 1. cargo check --workspace compiles clean (if this session changed code)
set -euo pipefail

cat > /dev/null

WORKSPACE_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
PROJECT_HASH="$(echo "$WORKSPACE_ROOT" | shasum -a 256 | cut -c1-12)"
SESSION_ID="${CLAUDE_SESSION_ID:-default}"
MARKER_DIR="/tmp/temper-harness/${PROJECT_HASH}/${SESSION_ID}"

# Only check compilation if this session changed .rs files
SESSION_CHANGED=false
if [ -d "$MARKER_DIR" ] && [ -f "$MARKER_DIR/commit-pending" ]; then
    SESSION_CHANGED=true
fi
if (cd "$WORKSPACE_ROOT" && git diff --name-only 2>/dev/null | grep -q '\.rs$'); then
    SESSION_CHANGED=true
fi

if [ "$SESSION_CHANGED" = true ]; then
    echo "Running pre-exit workspace check..." >&2
    if ! (cd "$WORKSPACE_ROOT" && cargo check --workspace 2>&1 >&2); then
        echo "BLOCKED: Workspace has compilation errors!" >&2
        echo "Fix all compilation errors before exiting the session." >&2
        exit 2
    fi
else
    echo "No code changes in this session — skipping compilation check." >&2
fi

# Clean up this session's markers
if [ -d "$MARKER_DIR" ]; then
    rm -rf "$MARKER_DIR"
fi

echo "Session exit gate: OK" >&2
exit 0
