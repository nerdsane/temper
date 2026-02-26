#!/bin/bash
# Git Pre-Push Hook (Item 10 + full-codebase audits)
# BLOCKING: YES — all checks must pass before push
#
# This script is installed into .git/hooks/pre-push by scripts/setup-hooks.sh
# Bypass with: git push --no-verify (for emergencies only)
#
# Runs eight gates in order:
# 1. Integrity check (no TODO/unwrap/hacks in production code)
# 2. Readability ratchet (no readability debt regressions)
# 3. Rust formatting check (cargo fmt --check)
# 4. Rust compile check (cargo check --workspace)
# 5. Rust lint check (cargo clippy --workspace -- -D warnings)
# 6. Determinism audit (no HashMap/SystemTime in simulation code, advisory)
# 7. Full test suite (cargo test --workspace)
# 8. Rebuild MCP binary (ensures .mcp.json binary stays current)
set -euo pipefail

WORKSPACE_ROOT="$(git rev-parse --show-toplevel)"

echo "Pre-push: running full verification pipeline..." >&2
echo "Bypass with --no-verify for emergencies." >&2

# --- Gate 1: Integrity check (fast — grep scan) ---
echo "" >&2
echo "Gate 1/8: Integrity check..." >&2
if ! "$WORKSPACE_ROOT/scripts/integrity-check.sh" >&2; then
    echo "" >&2
    echo "BLOCKED: Integrity check failed! Push rejected." >&2
    echo "Remove TODO/FIXME/HACK/unwrap from production code." >&2
    exit 1
fi

# --- Gate 2: Readability ratchet (fast — structural checks) ---
echo "" >&2
echo "Gate 2/8: Readability ratchet..." >&2
if ! "$WORKSPACE_ROOT/scripts/readability-ratchet.sh" check ".ci/readability-baseline.env" >&2; then
    echo "" >&2
    echo "BLOCKED: Readability regression detected. Push rejected." >&2
    echo "Reduce readability debt or update baseline intentionally." >&2
    exit 1
fi

# --- Gate 3: rustfmt check ---
echo "" >&2
echo "Gate 3/8: rustfmt check..." >&2
if ! (cd "$WORKSPACE_ROOT" && cargo fmt --check 2>&1 >&2); then
    echo "" >&2
    echo "BLOCKED: Formatting check failed! Push rejected." >&2
    echo "Run: cargo fmt --all" >&2
    exit 1
fi

# --- Gate 4: Compile check ---
echo "" >&2
echo "Gate 4/8: cargo check..." >&2
if ! (cd "$WORKSPACE_ROOT" && cargo check --workspace 2>&1 >&2); then
    echo "" >&2
    echo "BLOCKED: Compile check failed! Push rejected." >&2
    exit 1
fi

# --- Gate 5: Lint check ---
echo "" >&2
echo "Gate 5/8: cargo clippy..." >&2
if ! (cd "$WORKSPACE_ROOT" && cargo clippy --workspace -- -D warnings 2>&1 >&2); then
    echo "" >&2
    echo "BLOCKED: Clippy check failed! Push rejected." >&2
    exit 1
fi

# --- Gate 6: Determinism audit (fast — grep scan) ---
echo "" >&2
echo "Gate 6/8: Determinism audit..." >&2
"$WORKSPACE_ROOT/scripts/check-determinism.sh" >&2
# Determinism is advisory — doesn't block push, but output is visible

# --- Gate 7: Full test suite ---
echo "" >&2
echo "Gate 7/8: Full test suite..." >&2
if ! (cd "$WORKSPACE_ROOT" && cargo test --workspace 2>&1 >&2); then
    echo "" >&2
    echo "BLOCKED: Test suite failed! Push rejected." >&2
    echo "Fix failing tests before pushing." >&2
    exit 1
fi

echo "" >&2
echo "Gate 8/8: Rebuild MCP binary..." >&2
if ! (cd "$WORKSPACE_ROOT" && cargo build -p temper-cli 2>&1 >&2); then
    echo "" >&2
    echo "BLOCKED: MCP binary build failed! Push rejected." >&2
    exit 1
fi

echo "" >&2
echo "Pre-push: ALL GATES PASSED" >&2
exit 0
