#!/bin/bash
# pow-produce-claims.sh — Generate claims skeleton from trace + git state
# Usage: pow-produce-claims.sh [intent_summary] [plan_file]
set -euo pipefail

WORKSPACE_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
PROJECT_HASH="$(echo "$WORKSPACE_ROOT" | shasum -a 256 | cut -c1-12)"
MARKER_DIR="/tmp/temper-harness/${PROJECT_HASH}"

SESSION_ID="${CLAUDE_SESSION_ID:-default}"
TRACE_FILE="$MARKER_DIR/trace-${SESSION_ID}.jsonl"
CLAIMS_FILE="$MARKER_DIR/claims-${SESSION_ID}.toml"

INTENT_SUMMARY="${1:-}"
PLAN_FILE="${2:-}"

# Auto-detect plan file if not provided
if [ -z "$PLAN_FILE" ]; then
    PLAN_FILE="$(ls -t "$WORKSPACE_ROOT"/.progress/*.md 2>/dev/null | head -1 || echo "")"
    # Make path relative to workspace root
    if [ -n "$PLAN_FILE" ]; then
        PLAN_FILE="${PLAN_FILE#"$WORKSPACE_ROOT"/}"
    fi
fi

# Auto-fill intent_summary from plan file heading if not provided
if [ -z "$INTENT_SUMMARY" ] && [ -n "$PLAN_FILE" ]; then
    PLAN_PATH="$PLAN_FILE"
    # Resolve relative paths against workspace root
    if [ "${PLAN_PATH#/}" = "$PLAN_PATH" ]; then
        PLAN_PATH="$WORKSPACE_ROOT/$PLAN_PATH"
    fi
    if [ -f "$PLAN_PATH" ]; then
        INTENT_SUMMARY="$(head -1 "$PLAN_PATH" | sed 's/^#* *//')"
    fi
fi

# Derive files_modified from trace mutation events
FILES_MODIFIED=""
if [ -f "$TRACE_FILE" ] && command -v jq >/dev/null 2>&1; then
    FILES_MODIFIED="$(jq -r 'select(.category == "mutation") | .tool_input_summary.file_path // .tool_input_summary.command // empty' "$TRACE_FILE" 2>/dev/null | sort -u | grep -v '^$' || echo "")"
fi

# Derive files_reviewed from trace read events
FILES_REVIEWED=""
if [ -f "$TRACE_FILE" ] && command -v jq >/dev/null 2>&1; then
    FILES_REVIEWED="$(jq -r 'select(.category == "read") | .tool_input_summary.path // empty' "$TRACE_FILE" 2>/dev/null | sort -u | grep -v '^$' || echo "")"
fi

# Get git diff files — prefer committed diff, fall back to working tree
GIT_COMMITTED_FILES="$(cd "$WORKSPACE_ROOT" && git diff --name-only HEAD~1..HEAD 2>/dev/null || echo "")"
if [ -n "$GIT_COMMITTED_FILES" ]; then
    ALL_GIT_FILES="$(echo "$GIT_COMMITTED_FILES" | sort -u | grep -v '^$' || echo "")"
else
    GIT_DIFF_FILES="$(cd "$WORKSPACE_ROOT" && git diff --name-only HEAD 2>/dev/null || echo "")"
    GIT_STAGED_FILES="$(cd "$WORKSPACE_ROOT" && git diff --cached --name-only 2>/dev/null || echo "")"
    ALL_GIT_FILES="$(echo -e "${GIT_DIFF_FILES}\n${GIT_STAGED_FILES}" | sort -u | grep -v '^$' || echo "")"
fi

# Check if tests ran in trace
TESTS_RAN=false
if [ -f "$TRACE_FILE" ] && command -v jq >/dev/null 2>&1; then
    if jq -r 'select(.tool_name == "Bash") | .tool_input_summary.command // ""' "$TRACE_FILE" 2>/dev/null | grep -q 'cargo test'; then
        TESTS_RAN=true
    fi
fi

# Count trace events
TRACE_MUTATIONS=0
TRACE_READS=0
TRACE_TOTAL=0
if [ -f "$TRACE_FILE" ] && command -v jq >/dev/null 2>&1; then
    TRACE_MUTATIONS="$(jq -s '[.[] | select(.category == "mutation")] | length' "$TRACE_FILE" 2>/dev/null || echo 0)"
    TRACE_READS="$(jq -s '[.[] | select(.category == "read")] | length' "$TRACE_FILE" 2>/dev/null || echo 0)"
    TRACE_TOTAL="$(wc -l < "$TRACE_FILE" | tr -d ' ')"
fi

# Format arrays for TOML
format_toml_array() {
    local INPUT="$1"
    if [ -z "$INPUT" ]; then
        echo "[]"
        return
    fi
    echo -n "["
    FIRST=true
    while IFS= read -r ITEM; do
        [ -z "$ITEM" ] && continue
        if [ "$FIRST" = true ]; then
            FIRST=false
        else
            echo -n ", "
        fi
        echo -n "\"$ITEM\""
    done <<< "$INPUT"
    echo "]"
}

MODIFIED_ARRAY="$(format_toml_array "$FILES_MODIFIED")"
REVIEWED_ARRAY="$(format_toml_array "$FILES_REVIEWED")"
GIT_FILES_ARRAY="$(format_toml_array "$ALL_GIT_FILES")"

# Auto-fill scope_description from plan file phase headings
SCOPE_DESC=""
if [ -n "$PLAN_FILE" ]; then
    PLAN_PATH="$PLAN_FILE"
    if [ "${PLAN_PATH#/}" = "$PLAN_PATH" ]; then
        PLAN_PATH="$WORKSPACE_ROOT/$PLAN_PATH"
    fi
    if [ -f "$PLAN_PATH" ]; then
        SCOPE_DESC="$(grep '^### Phase' "$PLAN_PATH" | sed 's/^### //' | tr '\n' ';' | sed 's/;$//' | sed 's/;/; /g')"
    fi
fi

# Auto-count tests added from committed diff
TESTS_ADDED=0
if command -v git >/dev/null 2>&1; then
    TESTS_ADDED="$(cd "$WORKSPACE_ROOT" && git diff HEAD~1..HEAD 2>/dev/null | grep -c '^\+.*#\[test\]\|^\+.*fn test_\|^\+.*#\[tokio::test\]' || echo 0)"
fi

cat > "$CLAIMS_FILE" <<EOF
[meta]
session_id = "$SESSION_ID"
plan_file = "$PLAN_FILE"
intent_summary = "$INTENT_SUMMARY"
timestamp = "$(date -u +%Y-%m-%dT%H:%M:%SZ)"

[claims]
files_modified = $MODIFIED_ARRAY
files_reviewed = $REVIEWED_ARRAY
git_diff_files = $GIT_FILES_ARRAY
tests_ran = $TESTS_RAN
tests_added = $TESTS_ADDED
no_unrelated_changes = true
scope_description = "$SCOPE_DESC"

[trace_stats]
total_events = $TRACE_TOTAL
mutations = $TRACE_MUTATIONS
reads = $TRACE_READS
EOF

echo "Claims written: $CLAIMS_FILE" >&2
echo "$CLAIMS_FILE"
