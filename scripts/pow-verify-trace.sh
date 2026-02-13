#!/bin/bash
# pow-verify-trace.sh — Verify hash chain integrity of a trace file
# Usage: pow-verify-trace.sh [trace-file]
# If no file specified, uses the current session's trace.
set -euo pipefail

WORKSPACE_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
PROJECT_HASH="$(echo "$WORKSPACE_ROOT" | shasum -a 256 | cut -c1-12)"
MARKER_DIR="/tmp/temper-harness/${PROJECT_HASH}"

SESSION_ID="${CLAUDE_SESSION_ID:-default}"
TRACE_FILE="${1:-$MARKER_DIR/trace-${SESSION_ID}.jsonl}"

if [ ! -f "$TRACE_FILE" ]; then
    echo "ERROR: Trace file not found: $TRACE_FILE" >&2
    exit 1
fi

TOTAL=0
VERIFIED=0
BROKEN=0
PREV_HASH="0000000000000000000000000000000000000000000000000000000000000000"

while IFS= read -r LINE; do
    TOTAL=$((TOTAL + 1))

    # Extract fields
    SEQ="$(echo "$LINE" | jq -r '.seq')"
    TIMESTAMP="$(echo "$LINE" | jq -r '.timestamp')"
    TOOL_NAME="$(echo "$LINE" | jq -r '.tool_name')"
    RECORDED_PREV="$(echo "$LINE" | jq -r '.prev_hash')"
    RECORDED_HASH="$(echo "$LINE" | jq -r '.entry_hash')"

    # Check prev_hash linkage
    if [ "$RECORDED_PREV" != "$PREV_HASH" ]; then
        echo "BROKEN at seq $SEQ: prev_hash mismatch (expected $PREV_HASH, got $RECORDED_PREV)" >&2
        BROKEN=$((BROKEN + 1))
    fi

    # Recompute entry hash (pipe-delimited to avoid field collision)
    ENTRY_DATA="${SEQ}|${TIMESTAMP}|${TOOL_NAME}|${RECORDED_PREV}"
    EXPECTED_HASH="$(echo -n "$ENTRY_DATA" | shasum -a 256 | cut -c1-64)"

    if [ "$EXPECTED_HASH" != "$RECORDED_HASH" ]; then
        echo "TAMPERED at seq $SEQ: hash mismatch (expected $EXPECTED_HASH, got $RECORDED_HASH)" >&2
        BROKEN=$((BROKEN + 1))
    else
        VERIFIED=$((VERIFIED + 1))
    fi

    PREV_HASH="$RECORDED_HASH"
done < "$TRACE_FILE"

echo "=== Trace Verification ==="
echo "Total entries: $TOTAL"
echo "Verified: $VERIFIED"
echo "Broken: $BROKEN"

if [ "$BROKEN" -gt 0 ]; then
    echo "RESULT: TAMPERED"
    exit 1
else
    echo "RESULT: OK"
    exit 0
fi
