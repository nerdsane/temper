#!/bin/bash
# verify-specs.sh -- Claude Code PostToolUse hook for spec verification.
#
# Reads the PostToolUse JSON payload from stdin, extracts the file path,
# and runs `temper verify` if the file is a spec file (*.ioa.toml,
# *.csdl.xml, or *.cedar).
#
# Exit codes:
#   0 -- not a spec file, or verification passed
#   2 -- verification failed (blocks the edit in Claude Code)
#
# Expected behavior:
#   Input: {"tool_name":"Write","tool_input":{"file_path":"/path/to/order.ioa.toml",...}}
#   -> extracts /path/to/order.ioa.toml
#   -> determines parent directory as specs dir
#   -> runs: cargo run -p temper-cli -- verify --specs-dir /path/to
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

# Check if the file is a spec file we care about.
case "$FILE_PATH" in
    *.ioa.toml)  ;;
    *.csdl.xml)  ;;
    *.cedar)     ;;
    *)
        # Not a spec file -- nothing to verify.
        exit 0
        ;;
esac

# Determine the specs directory.
# For *.ioa.toml and *.csdl.xml the parent directory is the specs dir.
# For *.cedar files the specs dir is two levels up (policies/ sits inside specs/).
case "$FILE_PATH" in
    *.cedar)
        SPECS_DIR="$(dirname "$(dirname "$FILE_PATH")")"
        ;;
    *)
        SPECS_DIR="$(dirname "$FILE_PATH")"
        ;;
esac

# Verify the specs directory exists and contains a CSDL model.
if [ ! -d "$SPECS_DIR" ]; then
    # Directory does not exist yet; skip verification.
    exit 0
fi

if [ ! -f "$SPECS_DIR/model.csdl.xml" ]; then
    # No CSDL model present; skip verification (incomplete spec set).
    exit 0
fi

# Find the workspace root (location of the top-level Cargo.toml).
WORKSPACE_ROOT="$(cd "$SPECS_DIR" && while [ "$(pwd)" != "/" ]; do
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

echo "Verifying specs in $SPECS_DIR ..." >&2

# Run the verification cascade.
if OUTPUT="$(cd "$WORKSPACE_ROOT" && cargo run -p temper-cli -- verify --specs-dir "$SPECS_DIR" 2>&1)"; then
    echo "$OUTPUT" >&2
    echo "Spec verification passed." >&2
    exit 0
else
    EXIT_CODE=$?
    echo "$OUTPUT" >&2
    echo "" >&2
    echo "Spec verification FAILED (exit $EXIT_CODE)." >&2
    echo "The edited spec file did not pass the verification cascade." >&2
    echo "Fix the spec errors above and try again." >&2
    exit 2
fi
