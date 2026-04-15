#!/bin/bash
# Git Pre-Push Hook
# BLOCKING: YES — all checks must pass before push
#
# This script is installed into .git/hooks/pre-push by scripts/setup-hooks.sh
# Bypass with: git push --no-verify (for emergencies only)
#
# Runs four gates in order:
# 1. Rust formatting check (cargo fmt --check)
# 2. Lint check (cargo clippy --workspace -- -D warnings)
# 3. Readability ratchet (no readability debt regressions)
# 4. Full test suite (cargo test --workspace)
#
# Removed gates (handled elsewhere):
# - Integrity check: git pre-commit hook
# - Determinism audit: Claude PostToolUse hook (edit-time)
# - Dep isolation: git pre-commit hook
# - MCP binary rebuild: CI
set -euo pipefail

WORKSPACE_ROOT="$(git rev-parse --show-toplevel)"

echo "Pre-push: running verification pipeline..." >&2
echo "Bypass with --no-verify for emergencies." >&2

# --- Gate 1: rustfmt check ---
echo "" >&2
echo "Gate 1/4: rustfmt check..." >&2
if ! (cd "$WORKSPACE_ROOT" && cargo fmt --check 2>&1 >&2); then
    echo "" >&2
    echo "BLOCKED: Formatting check failed! Push rejected." >&2
    echo "Run: cargo fmt --all" >&2
    exit 1
fi

# --- Gate 2: Lint check (clippy subsumes cargo check) ---
echo "" >&2
echo "Gate 2/4: cargo clippy..." >&2
if ! (cd "$WORKSPACE_ROOT" && cargo clippy --workspace -- -D warnings 2>&1 >&2); then
    echo "" >&2
    echo "BLOCKED: Clippy check failed! Push rejected." >&2
    exit 1
fi

# --- Gate 3: Readability ratchet ---
echo "" >&2
echo "Gate 3/4: Readability ratchet..." >&2
if ! "$WORKSPACE_ROOT/scripts/readability-ratchet.sh" check ".ci/readability-baseline.env" >&2; then
    echo "" >&2
    echo "BLOCKED: Readability regression detected. Push rejected." >&2
    echo "Reduce readability debt or update baseline intentionally." >&2
    exit 1
fi

# --- Gate 4: Full test suite ---
echo "" >&2
echo "Gate 4/4: Full test suite..." >&2
if ! (cd "$WORKSPACE_ROOT" && cargo test --workspace 2>&1 >&2); then
    echo "" >&2
    echo "BLOCKED: Test suite failed! Push rejected." >&2
    echo "Fix failing tests before pushing." >&2
    exit 1
fi

echo "" >&2
echo "Pre-push: ALL GATES PASSED" >&2
exit 0
