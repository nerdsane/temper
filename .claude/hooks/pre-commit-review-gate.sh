#!/bin/bash
# Pre-Commit Review Gate (PreToolUse — Bash)
# BLOCKING: YES (exit 2 if review markers missing)
#
# Before any `git commit` command, checks:
# 1. DST review marker exists (if sim-visible code was changed)
# 2. Code review marker exists (for any significant change)
# 3. Tests pass (cargo test --workspace)
# 4. Determinism canary passes
#
# This is the PRIMARY enforcement point. The Stop hook is the safety net.
#
# Markers are simple files in /tmp/temper-harness/{project_hash}/:
#   dst-reviewed   — written by DST reviewer agent on PASS
#   code-reviewed  — written by code reviewer agent on PASS
# Cleaned up by the Stop hook on successful session exit.
set -euo pipefail

PAYLOAD="$(cat)"

# Extract the command from bash tool input
if command -v jq >/dev/null 2>&1; then
    CMD="$(echo "$PAYLOAD" | jq -r '.tool_input.command // empty')"
else
    CMD="$(echo "$PAYLOAD" | grep -o -m1 '"command"[[:space:]]*:[[:space:]]*"[^"]*"' | head -1 | sed 's/.*"command"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/' || true)"
fi

# Only gate git commit commands
case "${CMD:-}" in
    *"git commit"*) ;;
    *) exit 0 ;;
esac

WORKSPACE_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
PROJECT_HASH="$(echo "$WORKSPACE_ROOT" | shasum -a 256 | cut -c1-12)"
MARKER_DIR="/tmp/temper-harness/${PROJECT_HASH}"

BLOCKED=false

# Helper: check for marker (supports both old plain format and new TOML format)
marker_exists() { [ -f "$MARKER_DIR/$1" ] || [ -f "$MARKER_DIR/$1.toml" ]; }

# --- Check 1: DST review marker (if sim-visible code changed) ---
SIM_FILES_CHANGED=false
if cd "$WORKSPACE_ROOT" 2>/dev/null; then
    if git diff --cached --name-only 2>/dev/null | grep -qE '(temper-runtime|temper-jit|temper-server)/.*\.rs$'; then
        SIM_FILES_CHANGED=true
    fi
fi

if [ "$SIM_FILES_CHANGED" = true ]; then
    if ! marker_exists "dst-reviewed"; then
        echo "" >&2
        echo "══════════════════════════════════════════════════════════════" >&2
        echo "  BLOCKED: DST review required before commit" >&2
        echo "══════════════════════════════════════════════════════════════" >&2
        echo "  Simulation-visible code was changed. You must run the DST" >&2
        echo "  compliance reviewer before committing." >&2
        echo "" >&2
        echo "  Invoke the DST reviewer agent, then retry the commit." >&2
        echo "══════════════════════════════════════════════════════════════" >&2
        BLOCKED=true
    fi
fi

# --- Check 2: Code review marker ---
if ! marker_exists "code-reviewed"; then
    echo "" >&2
    echo "══════════════════════════════════════════════════════════════" >&2
    echo "  BLOCKED: Code review required before commit" >&2
    echo "══════════════════════════════════════════════════════════════" >&2
    echo "  Run a code review (invoke the code-reviewer agent or use" >&2
    echo "  /review-code), then retry the commit." >&2
    echo "══════════════════════════════════════════════════════════════" >&2
    BLOCKED=true
fi

# --- Check 3: Tests must pass ---
echo "Running pre-commit test suite..." >&2
if ! (cd "$WORKSPACE_ROOT" && cargo test --workspace 2>&1 | tail -5 >&2); then
    echo "" >&2
    echo "══════════════════════════════════════════════════════════════" >&2
    echo "  BLOCKED: Tests failed" >&2
    echo "══════════════════════════════════════════════════════════════" >&2
    echo "  All tests must pass before committing." >&2
    echo "══════════════════════════════════════════════════════════════" >&2
    BLOCKED=true
fi

# --- Check 4: Determinism canary must pass ---
echo "Running determinism canary..." >&2
if ! (cd "$WORKSPACE_ROOT" && cargo test -p temper-platform --test system_entity_dst determinism_canary 2>&1 | tail -5 >&2); then
    echo "" >&2
    echo "══════════════════════════════════════════════════════════════" >&2
    echo "  BLOCKED: Determinism canary failed" >&2
    echo "══════════════════════════════════════════════════════════════" >&2
    echo "  Same seed must produce identical simulation output." >&2
    echo "══════════════════════════════════════════════════════════════" >&2
    BLOCKED=true
fi

if [ "$BLOCKED" = true ]; then
    exit 2
fi

echo "Pre-commit gate: ALL CHECKS PASSED" >&2
exit 0
