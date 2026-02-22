#!/bin/bash
# Git Post-Commit Hook
# Writes commit lifecycle markers used by stop-verify safety net.
set -euo pipefail

WORKSPACE_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
PROJECT_HASH="$(echo "$WORKSPACE_ROOT" | shasum -a 256 | cut -c1-12)"
MARKER_DIR="/tmp/temper-harness/${PROJECT_HASH}"
mkdir -p "$MARKER_DIR"

HEAD_SHA="$(git -C "$WORKSPACE_ROOT" rev-parse HEAD 2>/dev/null || echo "unknown")"
TIMESTAMP="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

# Sticky marker: at least one commit happened in this session.
echo "${HEAD_SHA} ${TIMESTAMP}" > "$MARKER_DIR/commit-pending"

# Structured marker for downstream tooling.
if [ -x "$WORKSPACE_ROOT/scripts/write-marker.sh" ]; then
    bash "$WORKSPACE_ROOT/scripts/write-marker.sh" "commit-pending" "pass" \
        "head_sha=${HEAD_SHA}" \
        "source=git-post-commit" >/dev/null 2>&1 || true
fi

# Sticky marker: at least one simulation-visible Rust file changed in a commit.
CHANGED_FILES="$(git -C "$WORKSPACE_ROOT" show --name-only --pretty=format: HEAD 2>/dev/null || true)"
if echo "$CHANGED_FILES" | grep -qE '^crates/(temper-runtime|temper-jit|temper-server)/.*\.rs$'; then
    echo "${HEAD_SHA} ${TIMESTAMP}" > "$MARKER_DIR/sim-changed"

    if [ -x "$WORKSPACE_ROOT/scripts/write-marker.sh" ]; then
        bash "$WORKSPACE_ROOT/scripts/write-marker.sh" "sim-changed" "pass" \
            "head_sha=${HEAD_SHA}" \
            "source=git-post-commit" >/dev/null 2>&1 || true
    fi
fi

exit 0
