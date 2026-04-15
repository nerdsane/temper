#!/bin/bash
# Item 3: Dependency Isolation Guard (PostToolUse — Write|Edit)
# BLOCKING: YES (exit 2 on violation)
#
# After editing any Cargo.toml, checks that:
# 1. temper-jit does not have temper-verify in [dependencies] (only [dev-dependencies])
# 2. No production crate pulls in stateright or proptest
set -euo pipefail

PAYLOAD="$(cat)"

# Extract file path from tool input
if command -v jq >/dev/null 2>&1; then
    FILE_PATH="$(echo "$PAYLOAD" | jq -r '.tool_input.file_path // .tool_input.path // empty')"
else
    FILE_PATH="$(echo "$PAYLOAD" | grep -o '"file_path"[[:space:]]*:[[:space:]]*"[^"]*"' | head -1 | sed 's/.*"\([^"]*\)"$/\1/' || true)"
fi

# Only check Cargo.toml edits
case "${FILE_PATH:-}" in
    *Cargo.toml) ;;
    *) exit 0 ;;
esac

WORKSPACE_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"

# Check 1: temper-jit must not have production dependency on temper-verify
JIT_TOML="$WORKSPACE_ROOT/crates/temper-jit/Cargo.toml"
if [ -f "$JIT_TOML" ]; then
    # Use cargo tree to check real dependency graph (not just TOML text)
    if cargo tree --no-dev -p temper-jit --manifest-path "$WORKSPACE_ROOT/Cargo.toml" 2>/dev/null | grep -q temper-verify; then
        echo "BLOCKED: temper-jit has production dependency on temper-verify!" >&2
        echo "temper-verify (and its deps: stateright, z3, proptest) must NOT be in production binaries." >&2
        echo "Move temper-verify to [dev-dependencies] in temper-jit/Cargo.toml." >&2
        exit 2
    fi
fi

# Check 2: No stateright or proptest in production deps
for CRATE in temper-jit temper-server temper-runtime; do
    if cargo tree --no-dev -p "$CRATE" --manifest-path "$WORKSPACE_ROOT/Cargo.toml" 2>/dev/null | grep -qE 'stateright|proptest'; then
        echo "BLOCKED: $CRATE production binary includes stateright or proptest!" >&2
        echo "Verification libraries must not be in production dependency graph." >&2
        exit 2
    fi
done

echo "Dependency isolation: OK" >&2
exit 0
