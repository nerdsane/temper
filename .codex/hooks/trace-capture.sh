#!/bin/bash
# trace-capture.sh — PostToolUse hook for hash-chained trace capture
# Appends one JSON line per tool call to a session trace file.
# BLOCKING: No (advisory, exit 0 always)
#
# Markers are session-scoped: /tmp/temper-harness/{project_hash}/{session_id}/
set -u

PAYLOAD="$(cat)"

WORKSPACE_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
PROJECT_HASH="$(echo "$WORKSPACE_ROOT" | shasum -a 256 | cut -c1-12)"
SESSION_ID="${CLAUDE_SESSION_ID:-default}"
MARKER_DIR="/tmp/temper-harness/${PROJECT_HASH}/${SESSION_ID}"
mkdir -p "$MARKER_DIR"

TRACE_FILE="$MARKER_DIR/trace.jsonl"
SEQ_FILE="$MARKER_DIR/trace.seq"
PREV_HASH_FILE="$MARKER_DIR/trace.prevhash"

# Reset hash chain if trace file is new or empty (prevents stale prevhash from prior session)
if [ ! -f "$TRACE_FILE" ] || [ ! -s "$TRACE_FILE" ]; then
    rm -f "$PREV_HASH_FILE" "$SEQ_FILE"
fi

# Acquire mkdir-based lock (atomic on all POSIX systems, unlike flock which is unavailable on macOS)
LOCK_DIR="$MARKER_DIR/trace.lock"
LOCK_ATTEMPTS=0
while ! mkdir "$LOCK_DIR" 2>/dev/null; do
    LOCK_ATTEMPTS=$((LOCK_ATTEMPTS + 1))
    if [ "$LOCK_ATTEMPTS" -ge 200 ]; then
        exit 0  # skip trace rather than block the agent (2s timeout)
    fi
    sleep 0.01
done
trap 'rmdir "$LOCK_DIR" 2>/dev/null' EXIT

# Get sequence number (inside lock — critical section)
if [ -f "$SEQ_FILE" ]; then
    SEQ="$(cat "$SEQ_FILE")"
    SEQ=$((SEQ + 1))
else
    SEQ=1
fi
echo "$SEQ" > "$SEQ_FILE"

TIMESTAMP="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

# Extract tool info from payload
if command -v jq >/dev/null 2>&1; then
    TOOL_NAME="$(echo "$PAYLOAD" | jq -r '.tool_name // "unknown"')"

    # Categorize the tool
    case "$TOOL_NAME" in
        Write|Edit|NotebookEdit)
            CATEGORY="mutation"
            INPUT_SUMMARY="$(echo "$PAYLOAD" | jq -c '{
                file_path: (.tool_input.file_path // .tool_input.notebook_path // "unknown"),
                content_length: ((.tool_input.content // .tool_input.new_string // .tool_input.new_source // "") | length)
            }')"
            ;;
        Bash)
            CMD="$(echo "$PAYLOAD" | jq -r '.tool_input.command // ""')"
            if echo "$CMD" | grep -qE '(rm |git push|git reset|drop |delete )'; then
                CATEGORY="mutation"
            else
                CATEGORY="control"
            fi
            TRUNCATED_CMD="$(echo "$CMD" | head -c 200)"
            INPUT_SUMMARY="$(echo "{}" | jq -c --arg cmd "$TRUNCATED_CMD" '{command: $cmd}')"
            ;;
        Read|Grep|Glob)
            CATEGORY="read"
            INPUT_SUMMARY="$(echo "$PAYLOAD" | jq -c '{
                path: (.tool_input.file_path // .tool_input.path // .tool_input.pattern // "unknown")
            }' 2>/dev/null || echo '{}')"
            ;;
        Task|LSP|WebFetch|WebSearch)
            CATEGORY="control"
            INPUT_SUMMARY="$(echo "$PAYLOAD" | jq -c '{
                type: (.tool_input.subagent_type // .tool_input.operation // "unknown")
            }' 2>/dev/null || echo '{}')"
            ;;
        *)
            CATEGORY="control"
            INPUT_SUMMARY="{}"
            ;;
    esac

    # Response summary
    TOOL_RESPONSE="$(echo "$PAYLOAD" | jq -r '.tool_response // ""' | head -c 100)"
    RESPONSE_SUMMARY="$(echo "{}" | jq -c --arg r "$TOOL_RESPONSE" '{snippet: $r}')"
else
    TOOL_NAME="unknown"
    CATEGORY="unknown"
    INPUT_SUMMARY="{}"
    RESPONSE_SUMMARY="{}"
fi

# Hash chain: get previous hash
if [ -f "$PREV_HASH_FILE" ]; then
    PREV_HASH="$(cat "$PREV_HASH_FILE")"
else
    PREV_HASH="0000000000000000000000000000000000000000000000000000000000000000"
fi

# Compute entry hash = sha256(seq|timestamp|tool_name|prev_hash)
ENTRY_DATA="${SEQ}|${TIMESTAMP}|${TOOL_NAME}|${PREV_HASH}"
ENTRY_HASH="$(echo -n "$ENTRY_DATA" | shasum -a 256 | cut -c1-64)"

# Save current hash for next entry
echo "$ENTRY_HASH" > "$PREV_HASH_FILE"

# Build JSON line and append
if command -v jq >/dev/null 2>&1; then
    jq -n -c \
        --argjson seq "$SEQ" \
        --arg ts "$TIMESTAMP" \
        --arg tool "$TOOL_NAME" \
        --argjson input "$INPUT_SUMMARY" \
        --argjson response "$RESPONSE_SUMMARY" \
        --arg cat "$CATEGORY" \
        --arg prev "$PREV_HASH" \
        --arg hash "$ENTRY_HASH" \
        '{seq: $seq, timestamp: $ts, tool_name: $tool, tool_input_summary: $input, tool_response_summary: $response, category: $cat, prev_hash: $prev, entry_hash: $hash}' \
        >> "$TRACE_FILE"
else
    echo "{\"seq\":$SEQ,\"timestamp\":\"$TIMESTAMP\",\"tool_name\":\"$TOOL_NAME\",\"category\":\"$CATEGORY\",\"prev_hash\":\"$PREV_HASH\",\"entry_hash\":\"$ENTRY_HASH\"}" >> "$TRACE_FILE"
fi

# Release lock explicitly (trap EXIT also handles cleanup)
rmdir "$LOCK_DIR" 2>/dev/null

exit 0
