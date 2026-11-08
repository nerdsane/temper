#!/bin/bash
# verify-specs.sh -- Claude Code PostToolUse hook for spec verification.
#
# Reads the PostToolUse JSON payload from stdin, extracts the file path,
# and runs `temper verify-ioa` on it if it is an IOA spec (*.ioa.toml): it
# parses and passes its verification cascade. CSDL, Cedar and the other checks
# over a whole source directory run in CI (scripts/verify-ioa.sh says why).
#
# Exit codes:
#   0 -- not a spec file, or verification passed
#   2 -- verification failed (blocks the edit in Claude Code)
#
# Expected behavior:
#   Input: {"tool_name":"Write","tool_input":{"file_path":"/path/to/order.ioa.toml",...}}
#   -> extracts /path/to/order.ioa.toml
#   -> runs: scripts/verify-ioa.sh /path/to/order.ioa.toml
#   -> exit 0 on pass, exit 2 on fail
#
#   Input: {"tool_name":"Edit","tool_input":{"file_path":"/path/to/main.rs",...}}
#   -> not a spec file, exit 0 immediately

set -euo pipefail

# Read the full JSON payload from stdin.
PAYLOAD="$(cat)"

# Extract the file path from the tool input.
# Prefer jq if available; fall back to grep/sed.
if command -v jq >/dev/null 2>&1; then
    FILE_PATH="$(echo "$PAYLOAD" | jq -r '.tool_input.file_path // empty')"
else
    FILE_PATH="$(echo "$PAYLOAD" | grep -o '"file_path"\s*:\s*"[^"]*"' | head -1 | sed 's/.*"file_path"\s*:\s*"\([^"]*\)".*/\1/')"
fi

# If we could not extract a file path, nothing to check.
if [ -z "$FILE_PATH" ]; then
    exit 0
fi

# Only IOA specs have a local check.
case "$FILE_PATH" in
    *.ioa.toml) ;;
    *) exit 0 ;;
esac

if [ ! -f "$FILE_PATH" ]; then
    # Deleted, or not written yet; nothing to verify.
    exit 0
fi

# Find the workspace root (location of the top-level Cargo.toml).
WORKSPACE_ROOT="$(cd "$(dirname "$FILE_PATH")" && while [ "$(pwd)" != "/" ]; do
    if [ -f Cargo.toml ] && grep -q '\[workspace\]' Cargo.toml 2>/dev/null; then
        pwd
        break
    fi
    cd ..
done)"

if [ -z "$WORKSPACE_ROOT" ]; then
    echo "Warning: could not find Temper workspace root. Skipping verification." >&2
    exit 0
fi

echo "Verifying $FILE_PATH ..." >&2

# Run the verification cascade.
if OUTPUT="$(cd "$WORKSPACE_ROOT" && "$WORKSPACE_ROOT/scripts/verify-ioa.sh" "$FILE_PATH" 2>&1)"; then
    echo "$OUTPUT" >&2
    echo "Spec verification passed." >&2
    exit 0
else
    EXIT_CODE=$?
    echo "$OUTPUT" >&2
    echo "" >&2
    echo "Spec verification FAILED (exit $EXIT_CODE)." >&2
    echo "The edited spec did not parse or did not pass its verification cascade." >&2
    echo "Fix the spec errors above and try again." >&2
    exit 2
fi
