#!/bin/bash
# pow-compare.sh — Mechanical comparison engine for Proof of Work
# Reads trace + agent-generated claims, cross-references against
# ground truth (trace evidence + git diff), outputs verification table.
# Claims are written by the agent via pow-agent-claims.sh (self-report).
# Usage: pow-compare.sh [claims-file]
set -euo pipefail

WORKSPACE_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
PROJECT_HASH="$(echo "$WORKSPACE_ROOT" | shasum -a 256 | cut -c1-12)"
MARKER_DIR="/tmp/temper-harness/${PROJECT_HASH}"

SESSION_ID="${CLAUDE_SESSION_ID:-default}"
TRACE_FILE="$MARKER_DIR/trace-${SESSION_ID}.jsonl"
CLAIMS_FILE="${1:-$MARKER_DIR/claims-${SESSION_ID}.toml}"

if [ ! -f "$CLAIMS_FILE" ]; then
    echo "ERROR: Claims file not found: $CLAIMS_FILE" >&2
    echo "Run pow-agent-claims.sh to write agent claims first." >&2
    exit 1
fi

PASS_COUNT=0
FAIL_COUNT=0
WARN_COUNT=0

# TOML single-line array parser (our scripts always produce single-line arrays)
parse_toml_array() {
    local KEY="$1" FILE="$2"
    grep "^${KEY}" "$FILE" 2>/dev/null | sed 's/.*\[//;s/\].*//;s/"//g;s/,/\n/g' | sed 's/^ *//;s/ *$//' | sort -u | grep -v '^$' || echo ""
}

result() {
    local CHECK="$1"
    local STATUS="$2"
    local DETAIL="$3"
    printf "%-30s %-10s %s\n" "$CHECK" "$STATUS" "$DETAIL"
    case "$STATUS" in
        VERIFIED) PASS_COUNT=$((PASS_COUNT + 1)) ;;
        REFUTED)  FAIL_COUNT=$((FAIL_COUNT + 1)) ;;
        WARNING)  WARN_COUNT=$((WARN_COUNT + 1)) ;;
    esac
}

echo "╔══════════════════════════════════════════════════════════════╗"
echo "║              Proof of Work — Verification Table             ║"
echo "╚══════════════════════════════════════════════════════════════╝"
echo ""
printf "%-30s %-10s %s\n" "CHECK" "STATUS" "DETAIL"
echo "─────────────────────────────────────────────────────────────────"

# --- Check 1: Trace integrity (hash chain) ---
if [ -f "$TRACE_FILE" ]; then
    if bash "$WORKSPACE_ROOT/scripts/pow-verify-trace.sh" "$TRACE_FILE" > /dev/null 2>&1; then
        TRACE_ENTRIES="$(wc -l < "$TRACE_FILE" | tr -d ' ')"
        result "Trace integrity" "VERIFIED" "$TRACE_ENTRIES entries, hash chain intact"
    else
        result "Trace integrity" "REFUTED" "Hash chain broken — trace may be tampered"
    fi
else
    result "Trace integrity" "WARNING" "No trace file found"
fi

# --- Check 2: Files modified match claims ---
if [ -f "$TRACE_FILE" ] && command -v jq >/dev/null 2>&1; then
    TRACE_MUTATIONS="$(jq -r 'select(.category == "mutation") | .tool_input_summary.file_path // empty' "$TRACE_FILE" 2>/dev/null | sort -u | grep -v '^$' || echo "")"
    CLAIMED_FILES="$(parse_toml_array 'files_modified' "$CLAIMS_FILE")"

    UNCLAIMED="$(comm -23 <(echo "$TRACE_MUTATIONS") <(echo "$CLAIMED_FILES") || echo "")"
    OVERCLAIMED="$(comm -13 <(echo "$TRACE_MUTATIONS") <(echo "$CLAIMED_FILES") || echo "")"

    if [ -z "$UNCLAIMED" ] && [ -z "$OVERCLAIMED" ]; then
        result "Files modified match" "VERIFIED" "Trace mutations match claims"
    elif [ -n "$UNCLAIMED" ]; then
        result "Files modified match" "REFUTED" "Unclaimed: $(echo "$UNCLAIMED" | tr '\n' ' ')"
    else
        result "Files modified match" "WARNING" "Overclaimed: $(echo "$OVERCLAIMED" | tr '\n' ' ')"
    fi
else
    result "Files modified match" "WARNING" "Cannot verify (no trace or jq)"
fi

# --- Check 3: No unclaimed changes (git diff vs claims) ---
# Prefer committed diff, fall back to working tree
GIT_COMMITTED="$(cd "$WORKSPACE_ROOT" && git diff --name-only HEAD~1..HEAD 2>/dev/null | sort -u || echo "")"
if [ -n "$GIT_COMMITTED" ]; then
    ALL_GIT="$GIT_COMMITTED"
else
    GIT_CHANGED="$(cd "$WORKSPACE_ROOT" && git diff --name-only HEAD 2>/dev/null | sort -u || echo "")"
    GIT_STAGED="$(cd "$WORKSPACE_ROOT" && git diff --cached --name-only 2>/dev/null | sort -u || echo "")"
    ALL_GIT="$(echo -e "${GIT_CHANGED}\n${GIT_STAGED}" | sort -u | grep -v '^$' || echo "")"
fi

CLAIMED_GIT="$(parse_toml_array 'git_diff_files' "$CLAIMS_FILE")"

if [ -z "$ALL_GIT" ]; then
    result "Git diff alignment" "VERIFIED" "No uncommitted changes"
elif [ -n "$CLAIMED_GIT" ]; then
    UNTRACKED_GIT="$(comm -23 <(echo "$ALL_GIT") <(echo "$CLAIMED_GIT") || echo "")"
    if [ -z "$UNTRACKED_GIT" ]; then
        result "Git diff alignment" "VERIFIED" "All git changes in claims"
    else
        result "Git diff alignment" "WARNING" "Unclaimed git changes: $(echo "$UNTRACKED_GIT" | tr '\n' ' ')"
    fi
else
    result "Git diff alignment" "WARNING" "Claims missing git_diff_files"
fi

# --- Check 4: Tests actually ran ---
if [ -f "$TRACE_FILE" ] && command -v jq >/dev/null 2>&1; then
    TEST_RUNS="$(jq -r 'select(.tool_name == "Bash") | .tool_input_summary.command // ""' "$TRACE_FILE" 2>/dev/null | grep -c 'cargo test' || true)"
    TEST_RUNS="${TEST_RUNS:-0}"
    CLAIMS_TESTS="$(grep '^tests_ran' "$CLAIMS_FILE" 2>/dev/null | grep -o 'true\|false' || echo "unknown")"

    if [ "$TEST_RUNS" -gt 0 ] && [ "$CLAIMS_TESTS" = "true" ]; then
        result "Tests ran" "VERIFIED" "$TEST_RUNS test run(s) found in trace"
    elif [ "$TEST_RUNS" -gt 0 ] && [ "$CLAIMS_TESTS" = "false" ]; then
        result "Tests ran" "WARNING" "Tests ran but claims say not passing"
    elif [ "$TEST_RUNS" -eq 0 ] && [ "$CLAIMS_TESTS" = "true" ]; then
        result "Tests ran" "REFUTED" "Claims say tests pass but no test run in trace"
    else
        result "Tests ran" "WARNING" "No test runs found in trace"
    fi
else
    result "Tests ran" "WARNING" "Cannot verify (no trace or jq)"
fi

# --- Check 5: Harness markers present ---
MARKERS_PRESENT=0
MARKERS_TOTAL=0

check_marker() {
    local NAME="$1"
    MARKERS_TOTAL=$((MARKERS_TOTAL + 1))
    if [ -f "$MARKER_DIR/${NAME}.toml" ] || [ -f "$MARKER_DIR/${NAME}" ]; then
        MARKERS_PRESENT=$((MARKERS_PRESENT + 1))
    fi
    return 0
}

check_marker "code-reviewed"
check_marker "dst-reviewed"

if [ "$MARKERS_PRESENT" -gt 0 ]; then
    result "Harness markers" "VERIFIED" "$MARKERS_PRESENT marker(s) present"
else
    result "Harness markers" "WARNING" "No review markers found"
fi

# --- Check 6: Suspicious patterns ---
SUSPICIOUS=0
if [ -f "$TRACE_FILE" ] && command -v jq >/dev/null 2>&1; then
    PREV_WAS_TEST=false
    EDIT_AFTER_TEST=false
    RETEST_COUNT=0
    while IFS= read -r LINE; do
        TOOL="$(echo "$LINE" | jq -r '.tool_name')"
        CAT="$(echo "$LINE" | jq -r '.category')"
        if echo "$TOOL" | grep -q "Bash" && echo "$LINE" | jq -r '.tool_input_summary.command // ""' | grep -q 'cargo test'; then
            if [ "$EDIT_AFTER_TEST" = true ]; then
                RETEST_COUNT=$((RETEST_COUNT + 1))
            fi
            PREV_WAS_TEST=true
            EDIT_AFTER_TEST=false
        elif [ "$CAT" = "mutation" ]; then
            if [ "$PREV_WAS_TEST" = true ]; then
                EDIT_AFTER_TEST=true
            fi
        fi
    done < "$TRACE_FILE"

    if [ "$RETEST_COUNT" -gt 3 ]; then
        result "Suspicious patterns" "WARNING" "$RETEST_COUNT test-edit-retest cycles (review if excessive)"
        SUSPICIOUS=1
    fi
fi

if [ "$SUSPICIOUS" -eq 0 ]; then
    result "Suspicious patterns" "VERIFIED" "No suspicious patterns detected"
fi

# --- Summary ---
echo ""
echo "─────────────────────────────────────────────────────────────────"
TOTAL=$((PASS_COUNT + FAIL_COUNT + WARN_COUNT))
echo "Summary: $PASS_COUNT VERIFIED, $FAIL_COUNT REFUTED, $WARN_COUNT WARNING (of $TOTAL checks)"

if [ "$FAIL_COUNT" -gt 0 ]; then
    echo ""
    echo "RESULT: FAIL — $FAIL_COUNT check(s) refuted"
    exit 1
else
    echo ""
    echo "RESULT: PASS — all mechanical checks verified"

    # Write pow-verified marker
    bash "$WORKSPACE_ROOT/scripts/pow-write-marker.sh" "pow-verified" "pass" \
        "checks_passed=$PASS_COUNT" \
        "checks_warned=$WARN_COUNT" \
        "checks_failed=0" \
        "trace_file=$(basename "$TRACE_FILE")" \
        "claims_file=$(basename "$CLAIMS_FILE")"

    exit 0
fi
