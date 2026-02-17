#!/bin/bash
# Session Exit Gate (Stop)
# BLOCKING: YES (exit 2 if unverified pushes, missing reviews, or compilation errors)
#
# Before Claude Code session ends, checks:
# 1. If push-pending marker exists, test-verified marker must also exist
# 1b. If push-pending marker exists, GitHub CI must have passed
# 2. If commits were made with sim-visible changes, DST review marker must exist
# 3. If commits were made, code review marker must exist
# 4. cargo check --workspace compiles clean
#
# This is the SAFETY NET. The pre-commit gate is the primary enforcement.
# This catches anything that slipped through.
set -euo pipefail

cat > /dev/null

WORKSPACE_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"

PROJECT_HASH="$(echo "$WORKSPACE_ROOT" | shasum -a 256 | cut -c1-12)"
MARKER_DIR="/tmp/temper-harness/${PROJECT_HASH}"
SESSION_ID="${CLAUDE_SESSION_ID:-}"

ANY_BLOCKED=false

# Helper: check for marker (supports both old plain format and new TOML format)
marker_exists() { [ -f "$MARKER_DIR/$1" ] || [ -f "$MARKER_DIR/$1.toml" ]; }

# --- Check 1: Unverified pushes (local tests) ---
if [ -d "$MARKER_DIR" ]; then
    for pending in "$MARKER_DIR"/push-pending-*; do
        [ -f "$pending" ] || continue
        MARKER_SESSION="$(basename "$pending" | sed 's/push-pending-//')"
        if [ ! -f "$MARKER_DIR/test-verified-${MARKER_SESSION}" ]; then
            echo "BLOCKED: git push was made but local tests have not passed!" >&2
            echo "Run 'cargo test --workspace' and ensure all tests pass before exiting." >&2
            ANY_BLOCKED=true
            break
        fi
    done
fi

# --- Check 1b: GitHub CI must pass for all pushed commits ---
if [ -d "$MARKER_DIR" ] && command -v gh >/dev/null 2>&1; then
    for pending in "$MARKER_DIR"/push-pending-*; do
        [ -f "$pending" ] || continue
        # Extract commit SHA from marker (format: "<sha> <timestamp>")
        PUSHED_SHA="$(awk '{print $1}' "$pending")"
        [ -z "$PUSHED_SHA" ] || [ "$PUSHED_SHA" = "unknown" ] && continue

        echo "Checking GitHub CI status for ${PUSHED_SHA:0:7}..." >&2

        # Poll CI status — wait up to 10 minutes for completion
        CI_PASSED=false
        CI_CHECKED=false
        MAX_POLLS=20
        POLL_INTERVAL=30
        for i in $(seq 1 $MAX_POLLS); do
            CI_STATUS="$(cd "$WORKSPACE_ROOT" && gh run list --commit "$PUSHED_SHA" --json status,conclusion --limit 1 2>/dev/null || echo "[]")"

            # No runs found yet — CI may not have started
            if [ "$CI_STATUS" = "[]" ] || [ -z "$CI_STATUS" ]; then
                if [ "$i" -lt "$MAX_POLLS" ]; then
                    echo "  Waiting for CI to start (attempt $i/$MAX_POLLS)..." >&2
                    sleep "$POLL_INTERVAL"
                    continue
                else
                    echo "BLOCKED: GitHub CI never started for commit ${PUSHED_SHA:0:7}!" >&2
                    echo "Check https://github.com/$(cd "$WORKSPACE_ROOT" && gh repo view --json nameWithOwner -q .nameWithOwner 2>/dev/null)/actions" >&2
                    ANY_BLOCKED=true
                    CI_CHECKED=true
                    break
                fi
            fi

            RUN_STATUS="$(echo "$CI_STATUS" | jq -r '.[0].status // empty')"
            RUN_CONCLUSION="$(echo "$CI_STATUS" | jq -r '.[0].conclusion // empty')"

            if [ "$RUN_STATUS" = "completed" ]; then
                CI_CHECKED=true
                if [ "$RUN_CONCLUSION" = "success" ]; then
                    echo "  GitHub CI: PASSED" >&2
                    CI_PASSED=true
                else
                    echo "BLOCKED: GitHub CI FAILED for commit ${PUSHED_SHA:0:7} (conclusion: $RUN_CONCLUSION)" >&2
                    echo "Fix CI failures before exiting. Run: gh run view --log-failed" >&2
                    ANY_BLOCKED=true
                fi
                break
            fi

            # Still running
            if [ "$i" -lt "$MAX_POLLS" ]; then
                echo "  CI still running (attempt $i/$MAX_POLLS, status: $RUN_STATUS)..." >&2
                sleep "$POLL_INTERVAL"
            else
                echo "BLOCKED: GitHub CI still running after $((MAX_POLLS * POLL_INTERVAL))s for ${PUSHED_SHA:0:7}" >&2
                echo "Wait for CI to complete, or check: gh run list --commit $PUSHED_SHA" >&2
                ANY_BLOCKED=true
                CI_CHECKED=true
            fi
        done

        if [ "$CI_CHECKED" = true ]; then
            break  # Only need to check the most recent push
        fi
    done
elif [ -d "$MARKER_DIR" ] && ls "$MARKER_DIR"/push-pending-* >/dev/null 2>&1; then
    # gh CLI not available — warn but don't block
    echo "WARNING: gh CLI not found — cannot verify GitHub CI status" >&2
    echo "Install gh (https://cli.github.com) for CI verification." >&2
fi

# --- Check 2: Review markers (safety net) ---
# If review markers were consumed (deleted after commit), that's fine.
# If they exist but are stale, the pre-commit gate already handled it.
# This check catches the case where a commit somehow bypassed the gate.
if [ -d "$MARKER_DIR" ]; then
    if [ -f "$MARKER_DIR/commit-pending" ]; then
        # A commit was made — check for review markers
        if [ -f "$MARKER_DIR/sim-changed" ]; then
            if ! marker_exists "dst-reviewed"; then
                echo "BLOCKED: Simulation-visible code was committed without DST review!" >&2
                echo "Run the DST reviewer agent before exiting." >&2
                ANY_BLOCKED=true
            fi
        fi

        if ! marker_exists "code-reviewed"; then
            echo "BLOCKED: Code was committed without code review!" >&2
            echo "Run a code review before exiting." >&2
            ANY_BLOCKED=true
        fi

        if ! marker_exists "pow-verified"; then
            echo "BLOCKED: Proof of Work verification missing for committed code!" >&2
            echo "Run pow-agent-claims.sh and pow-compare.sh before exiting." >&2
            ANY_BLOCKED=true
        fi

        if ! marker_exists "alignment-reviewed"; then
            echo "BLOCKED: Alignment review missing for committed code!" >&2
            echo "Run the alignment reviewer agent before exiting." >&2
            ANY_BLOCKED=true
        fi
    fi
fi

# --- Check 3: Workspace compilation ---
echo "Running pre-exit workspace check..." >&2
if ! (cd "$WORKSPACE_ROOT" && cargo check --workspace 2>&1 >&2); then
    echo "BLOCKED: Workspace has compilation errors!" >&2
    echo "Fix all compilation errors before exiting the session." >&2
    ANY_BLOCKED=true
fi

# --- Archive + Clean up on success ---
if [ "$ANY_BLOCKED" = false ] && [ -d "$MARKER_DIR" ]; then
    # Generate proof document BEFORE deleting anything
    if [ -x "$WORKSPACE_ROOT/scripts/pow-generate-proof.sh" ]; then
        bash "$WORKSPACE_ROOT/scripts/pow-generate-proof.sh" 2>/dev/null || true
    fi

    # Archive trace + claims to .proof/archive/
    ARCHIVE_DIR="$WORKSPACE_ROOT/.proof/archive/$(date -u +%Y-%m-%d)"
    mkdir -p "$ARCHIVE_DIR"
    cp -f "$MARKER_DIR"/trace-*.jsonl "$ARCHIVE_DIR/" 2>/dev/null || true
    cp -f "$MARKER_DIR"/claims-*.toml "$ARCHIVE_DIR/" 2>/dev/null || true
    cp -f "$MARKER_DIR"/*.toml "$ARCHIVE_DIR/" 2>/dev/null || true

    # Now clean up markers
    rm -f "$MARKER_DIR"/push-pending-* "$MARKER_DIR"/test-verified-* 2>/dev/null || true
    rm -f "$MARKER_DIR"/commit-pending "$MARKER_DIR"/sim-changed 2>/dev/null || true
    rm -f "$MARKER_DIR"/dst-reviewed "$MARKER_DIR"/code-reviewed 2>/dev/null || true
    rm -f "$MARKER_DIR"/dst-reviewed.toml "$MARKER_DIR"/code-reviewed.toml 2>/dev/null || true
    rm -f "$MARKER_DIR"/pow-verified "$MARKER_DIR"/pow-verified.toml 2>/dev/null || true
    rm -f "$MARKER_DIR"/alignment-reviewed "$MARKER_DIR"/alignment-reviewed.toml 2>/dev/null || true
    rm -f "$MARKER_DIR"/claims-*.toml "$MARKER_DIR"/trace-*.jsonl 2>/dev/null || true
    rm -f "$MARKER_DIR"/trace-*.seq "$MARKER_DIR"/trace-*.prevhash 2>/dev/null || true
fi

if [ "$ANY_BLOCKED" = true ]; then
    exit 2
fi

echo "Session exit gate: OK" >&2
exit 0
