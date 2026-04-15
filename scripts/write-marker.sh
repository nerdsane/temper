#!/bin/bash
# write-marker.sh — Write structured TOML marker
# Usage: write-marker.sh <type> <verdict> [key=value ...]
#
# Writes a TOML marker file to /tmp/temper-harness/{project_hash}/
# Example: write-marker.sh dst-reviewed pass files_reviewed="a.rs,b.rs" findings_count=0
set -euo pipefail

TYPE="${1:?Usage: write-marker.sh <type> <verdict> [key=value ...]}"
VERDICT="${2:?Usage: write-marker.sh <type> <verdict> [key=value ...]}"
shift 2

WORKSPACE_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
PROJECT_HASH="$(echo "$WORKSPACE_ROOT" | shasum -a 256 | cut -c1-12)"
MARKER_DIR="/tmp/temper-harness/${PROJECT_HASH}"
mkdir -p "$MARKER_DIR"

SESSION_ID="${CLAUDE_SESSION_ID:-$(date +%s)}"
TIMESTAMP="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

# Escape special characters for TOML string values
escape_toml() {
    printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g; s/	/\\t/g'
}

# Write TOML marker
MARKER_FILE="$MARKER_DIR/${TYPE}.toml"

ESCAPED_TYPE="$(escape_toml "$TYPE")"
ESCAPED_VERDICT="$(escape_toml "$VERDICT")"
ESCAPED_SESSION="$(escape_toml "$SESSION_ID")"

cat > "$MARKER_FILE" <<EOF
[marker]
type = "$ESCAPED_TYPE"
verdict = "$ESCAPED_VERDICT"
timestamp = "$TIMESTAMP"
session_id = "$ESCAPED_SESSION"

[evidence]
EOF

# Append key=value pairs to [evidence] section
for KV in "$@"; do
    KEY="${KV%%=*}"
    VALUE="${KV#*=}"
    # If value contains commas, format as TOML array
    if echo "$VALUE" | grep -q ','; then
        TOML_ARRAY="["
        FIRST=true
        IFS=',' read -ra ITEMS <<< "$VALUE"
        for ITEM in "${ITEMS[@]}"; do
            if [ "$FIRST" = true ]; then
                FIRST=false
            else
                TOML_ARRAY="$TOML_ARRAY, "
            fi
            ESCAPED_ITEM="$(escape_toml "$ITEM")"
            TOML_ARRAY="$TOML_ARRAY\"$ESCAPED_ITEM\""
        done
        TOML_ARRAY="$TOML_ARRAY]"
        echo "$KEY = $TOML_ARRAY" >> "$MARKER_FILE"
    elif echo "$VALUE" | grep -qE '^[0-9]+$'; then
        echo "$KEY = $VALUE" >> "$MARKER_FILE"
    elif [ "$VALUE" = "true" ] || [ "$VALUE" = "false" ]; then
        echo "$KEY = $VALUE" >> "$MARKER_FILE"
    else
        ESCAPED_VALUE="$(escape_toml "$VALUE")"
        echo "$KEY = \"$ESCAPED_VALUE\"" >> "$MARKER_FILE"
    fi
done

# Also write backward-compatible plain marker (without .toml)
echo "$TIMESTAMP ${TYPE}-passed" > "$MARKER_DIR/${TYPE}"

echo "Marker written: $MARKER_FILE" >&2
