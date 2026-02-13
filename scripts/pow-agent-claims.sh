#!/bin/bash
# pow-agent-claims.sh — Write agent-generated claims (self-report)
#
# Called BY the agent with its own self-reported values. This is the agent's
# declaration of what it believes it did. The PoW comparison engine
# (pow-compare.sh) independently verifies these claims against the trace
# and git diff evidence.
#
# Usage: pow-agent-claims.sh \
#   intent="Fix Gap #23" \
#   plan_file=".progress/014_..." \
#   files_modified="types.rs,evaluate.rs" \
#   tests_ran=true \
#   tests_added=1 \
#   scope="Replaced derived Deserialize with manual impl"
set -euo pipefail

WORKSPACE_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
PROJECT_HASH="$(echo "$WORKSPACE_ROOT" | shasum -a 256 | cut -c1-12)"
MARKER_DIR="/tmp/temper-harness/${PROJECT_HASH}"
mkdir -p "$MARKER_DIR"

SESSION_ID="${CLAUDE_SESSION_ID:-default}"
CLAIMS_FILE="$MARKER_DIR/claims-${SESSION_ID}.toml"

# Parse key=value arguments
INTENT=""
PLAN_FILE=""
FILES_MODIFIED=""
TESTS_RAN="false"
TESTS_ADDED="0"
SCOPE=""

for ARG in "$@"; do
    KEY="${ARG%%=*}"
    VALUE="${ARG#*=}"
    case "$KEY" in
        intent) INTENT="$VALUE" ;;
        plan_file) PLAN_FILE="$VALUE" ;;
        files_modified) FILES_MODIFIED="$VALUE" ;;
        tests_ran) TESTS_RAN="$VALUE" ;;
        tests_added) TESTS_ADDED="$VALUE" ;;
        scope) SCOPE="$VALUE" ;;
        *) echo "Warning: unknown key '$KEY'" >&2 ;;
    esac
done

# Auto-detect plan file if not provided
if [ -z "$PLAN_FILE" ]; then
    PLAN_FILE="$(ls -t "$WORKSPACE_ROOT"/.progress/*.md 2>/dev/null | head -1 || echo "")"
    if [ -n "$PLAN_FILE" ]; then
        PLAN_FILE="${PLAN_FILE#"$WORKSPACE_ROOT"/}"
    fi
fi

# Auto-fill intent from plan file heading if not provided
if [ -z "$INTENT" ] && [ -n "$PLAN_FILE" ]; then
    PLAN_PATH="$PLAN_FILE"
    if [ "${PLAN_PATH#/}" = "$PLAN_PATH" ]; then
        PLAN_PATH="$WORKSPACE_ROOT/$PLAN_PATH"
    fi
    if [ -f "$PLAN_PATH" ]; then
        INTENT="$(head -1 "$PLAN_PATH" | sed 's/^#* *//')"
    fi
fi

# Format files_modified as TOML array from comma-separated string
format_toml_array() {
    local INPUT="$1"
    if [ -z "$INPUT" ]; then
        echo "[]"
        return
    fi
    echo -n "["
    FIRST=true
    echo "$INPUT" | tr ',' '\n' | while IFS= read -r ITEM; do
        ITEM="$(echo "$ITEM" | sed 's/^ *//;s/ *$//')"
        [ -z "$ITEM" ] && continue
        if [ "$FIRST" = true ]; then
            FIRST=false
        else
            echo -n ", "
        fi
        echo -n "\"$ITEM\""
    done
    echo "]"
}

MODIFIED_ARRAY="$(format_toml_array "$FILES_MODIFIED")"

# Get git diff files for reference
GIT_COMMITTED="$(cd "$WORKSPACE_ROOT" && git diff --name-only HEAD~1..HEAD 2>/dev/null || echo "")"
if [ -n "$GIT_COMMITTED" ]; then
    ALL_GIT="$GIT_COMMITTED"
else
    GIT_DIFF="$(cd "$WORKSPACE_ROOT" && git diff --name-only HEAD 2>/dev/null || echo "")"
    GIT_STAGED="$(cd "$WORKSPACE_ROOT" && git diff --cached --name-only 2>/dev/null || echo "")"
    ALL_GIT="$(echo -e "${GIT_DIFF}\n${GIT_STAGED}" | sort -u | grep -v '^$' || echo "")"
fi

# Format git files as TOML array
GIT_ARRAY="["
FIRST=true
if [ -n "$ALL_GIT" ]; then
    while IFS= read -r F; do
        [ -z "$F" ] && continue
        if [ "$FIRST" = true ]; then
            FIRST=false
        else
            GIT_ARRAY="${GIT_ARRAY}, "
        fi
        GIT_ARRAY="${GIT_ARRAY}\"$F\""
    done <<< "$ALL_GIT"
fi
GIT_ARRAY="${GIT_ARRAY}]"

cat > "$CLAIMS_FILE" <<EOF
[meta]
session_id = "$SESSION_ID"
plan_file = "$PLAN_FILE"
intent_summary = "$INTENT"
timestamp = "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
source = "agent"

[claims]
files_modified = $MODIFIED_ARRAY
git_diff_files = $GIT_ARRAY
tests_ran = $TESTS_RAN
tests_added = $TESTS_ADDED
no_unrelated_changes = true
scope_description = "$SCOPE"
EOF

echo "Agent claims written: $CLAIMS_FILE" >&2
echo "$CLAIMS_FILE"
