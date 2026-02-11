#!/bin/bash
# Git Pre-Push Hook (Item 10 + full-codebase audits)
# BLOCKING: YES — all checks must pass before push
#
# This script is installed into .git/hooks/pre-push by scripts/setup-hooks.sh
# Bypass with: git push --no-verify (for emergencies only)
#
# Runs three gates in order:
# 1. Integrity check (no TODO/unwrap/hacks in production code)
# 2. Determinism audit (no HashMap/SystemTime in simulation code)
# 3. Full test suite (cargo test --workspace)
set -euo pipefail

WORKSPACE_ROOT="$(git rev-parse --show-toplevel)"

echo "Pre-push: running full verification pipeline..." >&2
echo "Bypass with --no-verify for emergencies." >&2

# --- Gate 1: Integrity check (fast — grep scan) ---
echo "" >&2
echo "Gate 1/3: Integrity check..." >&2
if ! "$WORKSPACE_ROOT/scripts/integrity-check.sh" >&2; then
    echo "" >&2
    echo "BLOCKED: Integrity check failed! Push rejected." >&2
    echo "Remove TODO/FIXME/HACK/unwrap from production code." >&2
    exit 1
fi

# --- Gate 2: Determinism audit (fast — grep scan) ---
echo "" >&2
echo "Gate 2/3: Determinism audit..." >&2
"$WORKSPACE_ROOT/scripts/check-determinism.sh" >&2
# Determinism is advisory — doesn't block push, but output is visible

# --- Gate 3: Full test suite ---
echo "" >&2
echo "Gate 3/3: Full test suite..." >&2
if ! (cd "$WORKSPACE_ROOT" && cargo test --workspace 2>&1 >&2); then
    echo "" >&2
    echo "BLOCKED: Test suite failed! Push rejected." >&2
    echo "Fix failing tests before pushing." >&2
    exit 1
fi

echo "" >&2
echo "Pre-push: ALL GATES PASSED" >&2
exit 0
