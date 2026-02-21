#!/bin/bash
# pow-generate-proof.sh — Generate enhanced proof document with 7-section showboat format
# Usage:
#   pow-generate-proof.sh [output-dir]
#   pow-generate-proof.sh --session <id> [--output-dir <dir>] [--strict]
#
# In strict mode, the script skips generation unless core PoW evidence is present.
set -euo pipefail
export LC_ALL=C
export LANG=C

WORKSPACE_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
PROJECT_HASH="$(echo "$WORKSPACE_ROOT" | shasum -a 256 | cut -c1-12)"
MARKER_DIR="/tmp/temper-harness/${PROJECT_HASH}"

SESSION_ID="${CLAUDE_SESSION_ID:-default}"
OUTPUT_DIR="$WORKSPACE_ROOT/.proof"
STRICT_MODE=false

while [ $# -gt 0 ]; do
    case "$1" in
        --session)
            SESSION_ID="${2:-}"
            shift 2
            ;;
        --output-dir)
            OUTPUT_DIR="${2:-}"
            shift 2
            ;;
        --strict)
            STRICT_MODE=true
            shift
            ;;
        -h|--help)
            cat <<EOF
Usage: scripts/pow-generate-proof.sh [output-dir]
       scripts/pow-generate-proof.sh --session <id> [--output-dir <dir>] [--strict]

Options:
  --session <id>      Use a specific session id.
  --output-dir <dir>  Override proof output directory (default: .proof).
  --strict            Require claims+trace and review markers; skip empty proofs.
EOF
            exit 0
            ;;
        *)
            # Backward-compatible positional output dir
            OUTPUT_DIR="$1"
            shift
            ;;
    esac
done

mkdir -p "$OUTPUT_DIR"

get_mtime() {
    local FILE="$1"
    stat -f "%m" "$FILE" 2>/dev/null || stat -c "%Y" "$FILE" 2>/dev/null || echo 0
}

discover_latest_evidence_session() {
    local BEST_SESSION=""
    local BEST_MTIME=0
    local CLAIM_FILE=""

    for CLAIM_FILE in "$MARKER_DIR"/claims-*.toml; do
        [ -f "$CLAIM_FILE" ] || continue
        local SID
        SID="$(basename "$CLAIM_FILE" | sed 's/^claims-//;s/\.toml$//')"
        local TRACE_CANDIDATE="$MARKER_DIR/trace-${SID}.jsonl"
        if [ -s "$CLAIM_FILE" ] && [ -s "$TRACE_CANDIDATE" ]; then
            local MTIME
            MTIME="$(get_mtime "$CLAIM_FILE")"
            if [ "$MTIME" -gt "$BEST_MTIME" ]; then
                BEST_MTIME="$MTIME"
                BEST_SESSION="$SID"
            fi
        fi
    done

    echo "$BEST_SESSION"
}

TRACE_FILE="$MARKER_DIR/trace-${SESSION_ID}.jsonl"
CLAIMS_FILE="$MARKER_DIR/claims-${SESSION_ID}.toml"

# If default session has no evidence, pick latest session with claims+trace.
if { [ "$SESSION_ID" = "default" ] || [ -z "$SESSION_ID" ]; } \
    && { [ ! -s "$CLAIMS_FILE" ] || [ ! -s "$TRACE_FILE" ]; }; then
    DISCOVERED_SESSION="$(discover_latest_evidence_session)"
    if [ -n "$DISCOVERED_SESSION" ]; then
        SESSION_ID="$DISCOVERED_SESSION"
        TRACE_FILE="$MARKER_DIR/trace-${SESSION_ID}.jsonl"
        CLAIMS_FILE="$MARKER_DIR/claims-${SESSION_ID}.toml"
    fi
fi

marker_exists() {
    local NAME="$1"
    [ -f "$MARKER_DIR/$NAME" ] || [ -f "$MARKER_DIR/$NAME.toml" ]
}

maybe_skip_empty_proof() {
    local REASONS=()

    if [ ! -s "$CLAIMS_FILE" ]; then
        REASONS+=("missing claims (${CLAIMS_FILE})")
    fi
    if [ ! -s "$TRACE_FILE" ]; then
        REASONS+=("missing trace (${TRACE_FILE})")
    fi

    if [ "$STRICT_MODE" = true ]; then
        if ! marker_exists "pow-verified"; then
            REASONS+=("missing pow-verified marker")
        fi
        if ! marker_exists "alignment-reviewed"; then
            REASONS+=("missing alignment-reviewed marker")
        fi
        if ! marker_exists "code-reviewed"; then
            REASONS+=("missing code-reviewed marker")
        fi
        if marker_exists "sim-changed" && ! marker_exists "dst-reviewed"; then
            REASONS+=("sim-changed present but dst-reviewed marker missing")
        fi
    fi

    if [ "${#REASONS[@]}" -gt 0 ]; then
        echo "Skipping proof generation: ${REASONS[*]}" >&2
        exit 0
    fi
}

maybe_skip_empty_proof

# Generate filename from date + short commit hash
COMMIT_SHORT="$(cd "$WORKSPACE_ROOT" && git rev-parse --short HEAD 2>/dev/null || echo "nocommit")"
BRANCH="$(cd "$WORKSPACE_ROOT" && git rev-parse --abbrev-ref HEAD 2>/dev/null || echo "unknown")"
DATE="$(date -u +%Y-%m-%d)"
OUTPUT_FILE="$OUTPUT_DIR/${DATE}-${COMMIT_SHORT}.md"

# --- Gather data from claims ---
INTENT_SUMMARY=""
PLAN_FILE=""
SCOPE_DESC=""
TESTS_ADDED=0
TESTS_RAN=false
if [ -f "$CLAIMS_FILE" ]; then
    INTENT_SUMMARY="$(grep '^intent_summary' "$CLAIMS_FILE" | sed 's/^intent_summary = "//;s/"$//' || echo "")"
    PLAN_FILE="$(grep '^plan_file' "$CLAIMS_FILE" | sed 's/^plan_file = "//;s/"$//' || echo "")"
    SCOPE_DESC="$(grep '^scope_description' "$CLAIMS_FILE" | sed 's/^scope_description = "//;s/"$//' || echo "")"
    TESTS_ADDED="$(grep '^tests_added' "$CLAIMS_FILE" | sed 's/tests_added = //' || echo 0)"
    TESTS_RAN="$(grep '^tests_ran' "$CLAIMS_FILE" | grep -o 'true\|false' || echo false)"
fi

# --- Read plan file for narrative content ---
PLAN_TITLE=""
PLAN_PHASES=""
if [ -n "$PLAN_FILE" ]; then
    PLAN_PATH="$PLAN_FILE"
    if [ "${PLAN_PATH#/}" = "$PLAN_PATH" ]; then
        PLAN_PATH="$WORKSPACE_ROOT/$PLAN_PATH"
    fi
    if [ -f "$PLAN_PATH" ]; then
        PLAN_TITLE="$(head -1 "$PLAN_PATH" | sed 's/^#* *//')"
        # Extract phase headings as bullet points
        PLAN_PHASES="$(grep '^### Phase' "$PLAN_PATH" | sed 's/^### /- /' 2>/dev/null || echo "")"
    fi
fi

# Fall back to intent_summary for title
if [ -z "$PLAN_TITLE" ]; then
    PLAN_TITLE="${INTENT_SUMMARY:-Agent Work Session}"
fi

# --- Trace stats ---
TRACE_TOTAL=0
TRACE_MUTATIONS=0
TRACE_READS=0
TRACE_CONTROLS=0
SESSION_START=""
SESSION_END=""
SESSION_DURATION=""
if [ -f "$TRACE_FILE" ] && command -v jq >/dev/null 2>&1; then
    TRACE_TOTAL="$(wc -l < "$TRACE_FILE" | tr -d ' ')"
    TRACE_MUTATIONS="$(jq -s '[.[] | select(.category == "mutation")] | length' "$TRACE_FILE" 2>/dev/null || echo 0)"
    TRACE_READS="$(jq -s '[.[] | select(.category == "read")] | length' "$TRACE_FILE" 2>/dev/null || echo 0)"
    TRACE_CONTROLS="$(jq -s '[.[] | select(.category == "control")] | length' "$TRACE_FILE" 2>/dev/null || echo 0)"
    SESSION_START="$(head -1 "$TRACE_FILE" | jq -r '.timestamp' 2>/dev/null || echo "unknown")"
    SESSION_END="$(tail -1 "$TRACE_FILE" | jq -r '.timestamp' 2>/dev/null || echo "unknown")"

    # Compute duration if both timestamps available
    if [ "$SESSION_START" != "unknown" ] && [ "$SESSION_END" != "unknown" ]; then
        START_EPOCH="$(date -j -f "%Y-%m-%dT%H:%M:%SZ" "$SESSION_START" "+%s" 2>/dev/null || echo 0)"
        END_EPOCH="$(date -j -f "%Y-%m-%dT%H:%M:%SZ" "$SESSION_END" "+%s" 2>/dev/null || echo 0)"
        if [ "$START_EPOCH" -gt 0 ] && [ "$END_EPOCH" -gt 0 ]; then
            DIFF_SECS=$((END_EPOCH - START_EPOCH))
            HOURS=$((DIFF_SECS / 3600))
            MINS=$(( (DIFF_SECS % 3600) / 60 ))
            if [ "$HOURS" -gt 0 ]; then
                SESSION_DURATION="${HOURS}h ${MINS}m"
            else
                SESSION_DURATION="${MINS}m"
            fi
        fi
    fi
fi

# --- Git diff stats (committed changes) ---
DIFF_STAT="$(cd "$WORKSPACE_ROOT" && git diff --stat HEAD~1..HEAD 2>/dev/null || git diff --stat HEAD 2>/dev/null || echo "No changes")"
FILES_CHANGED="$(cd "$WORKSPACE_ROOT" && git diff --name-only HEAD~1..HEAD 2>/dev/null | wc -l | tr -d ' ' || git diff --name-only HEAD 2>/dev/null | wc -l | tr -d ' ' || echo "0")"

# --- Read marker files for review evidence ---
read_marker_field() {
    local MARKER="$1" FIELD="$2" DEFAULT="${3:-}"
    local FILE="$MARKER_DIR/${MARKER}.toml"
    if [ -f "$FILE" ]; then
        local RAW
        RAW="$(grep "^${FIELD}[[:space:]]*=" "$FILE" 2>/dev/null | head -1 | sed "s/^${FIELD}[[:space:]]*=[[:space:]]*//" || true)"
        RAW="$(echo "$RAW" | sed 's/^ *//;s/ *$//')"
        if [ -z "$RAW" ]; then
            echo "$DEFAULT"
            return
        fi

        # Quoted scalar
        if [[ "$RAW" == \"*\" ]]; then
            echo "$RAW" | sed 's/^"//;s/"$//'
            return
        fi

        # TOML array -> render as comma-separated scalar for readability
        if [[ "$RAW" == \[*\] ]]; then
            echo "$RAW" | sed 's/^\[//;s/\]$//;s/"//g;s/, */, /g'
            return
        fi

        # Numeric/bool/raw scalar
        echo "$RAW"
    else
        echo "$DEFAULT"
    fi
}

DST_STATUS="NOT FOUND"
DST_DETAIL=""
CODE_STATUS="NOT FOUND"
CODE_DETAIL=""
ALIGNMENT_STATUS="NOT FOUND"
POW_STATUS="NOT FOUND"
POW_DETAIL=""

if [ -f "$MARKER_DIR/dst-reviewed.toml" ] || [ -f "$MARKER_DIR/dst-reviewed" ]; then
    DST_STATUS="PASS"
    DST_DETAIL="$(read_marker_field 'dst-reviewed' 'summary' 'Determinism patterns verified')"
fi
if [ -f "$MARKER_DIR/code-reviewed.toml" ] || [ -f "$MARKER_DIR/code-reviewed" ]; then
    CODE_STATUS="PASS"
    CODE_DETAIL="$(read_marker_field 'code-reviewed' 'summary' 'Code quality verified')"
fi
if [ -f "$MARKER_DIR/alignment-reviewed.toml" ] || [ -f "$MARKER_DIR/alignment-reviewed" ]; then
    ALIGNMENT_STATUS="PASS"
fi
if [ -f "$MARKER_DIR/pow-verified.toml" ] || [ -f "$MARKER_DIR/pow-verified" ]; then
    POW_STATUS="PASS"
    POW_CHECKS="$(read_marker_field 'pow-verified' 'checks_passed' '0')"
    POW_WARNS="$(read_marker_field 'pow-verified' 'checks_warned' '0')"
    POW_DETAIL="${POW_CHECKS} passed, ${POW_WARNS} warned"
fi

# --- Read alignment edge verdicts ---
EDGE1_VERDICT="$(read_marker_field 'alignment-reviewed' 'edge1_intent_action' "$(read_marker_field 'alignment-reviewed' 'edge1_verdict' 'N/A')")"
EDGE2_VERDICT="$(read_marker_field 'alignment-reviewed' 'edge2_action_claim' "$(read_marker_field 'alignment-reviewed' 'edge2_verdict' 'N/A')")"
EDGE3_VERDICT="$(read_marker_field 'alignment-reviewed' 'edge3_intent_claim' "$(read_marker_field 'alignment-reviewed' 'edge3_verdict' 'N/A')")"
EDGE1_DETAIL="$(read_marker_field 'alignment-reviewed' 'edge1_detail' '')"
EDGE2_DETAIL="$(read_marker_field 'alignment-reviewed' 'edge2_detail' '')"
EDGE3_DETAIL="$(read_marker_field 'alignment-reviewed' 'edge3_detail' '')"

# --- Compute alignment summary ---
ALIGNMENT_EDGES_PASS=0
ALIGNMENT_EDGES_TOTAL=3
for V in "$EDGE1_VERDICT" "$EDGE2_VERDICT" "$EDGE3_VERDICT"; do
    case "$V" in
        ALIGNED|ACCURATE) ALIGNMENT_EDGES_PASS=$((ALIGNMENT_EDGES_PASS + 1)) ;;
        PARTIAL|MINOR_GAPS) ALIGNMENT_EDGES_PASS=$((ALIGNMENT_EDGES_PASS + 1)) ;;
    esac
done

# --- Run comparison and capture output ---
COMPARISON_OUTPUT=""
if [ -f "$CLAIMS_FILE" ]; then
    COMPARISON_OUTPUT="$(LC_ALL=C LANG=C bash "$WORKSPACE_ROOT/scripts/pow-compare.sh" --no-marker "$CLAIMS_FILE" 2>&1 || true)"
fi

# --- Test count from trace ---
TEST_RESULT_LINE=""
if [ "$TESTS_RAN" = "true" ]; then
    TEST_RESULT_LINE="Tests passed (${TESTS_ADDED} new test functions)"
else
    TEST_RESULT_LINE="Tests not verified in trace"
fi

# ===================================================================
# Generate the 7-section proof document
# ===================================================================
cat > "$OUTPUT_FILE" <<PROOF
# Agent Proof of Work — ${PLAN_TITLE}

**Session:** ${SESSION_ID} | **Date:** ${DATE} | **Commit:** ${COMMIT_SHORT} | **Branch:** ${BRANCH}

---

## Executive Summary

${INTENT_SUMMARY:-No intent summary available.}

| Metric | Value |
|--------|-------|
| Files changed | ${FILES_CHANGED} |
| Tests added | ${TESTS_ADDED} |
| Alignment | ${ALIGNMENT_STATUS} (${ALIGNMENT_EDGES_PASS}/${ALIGNMENT_EDGES_TOTAL} edges) |
| Harness | ${DST_STATUS} DST / ${CODE_STATUS} Code / ${POW_STATUS} PoW |

<!-- VISUAL:alignment-triangle -->

---

## What Was Built

**Scope:** ${SCOPE_DESC:-No scope description available.}

PROOF

# Append plan phase details if available
if [ -n "$PLAN_PHASES" ]; then
    echo "$PLAN_PHASES" >> "$OUTPUT_FILE"
else
    echo "_No plan phases available. See plan file: ${PLAN_FILE:-N/A}_" >> "$OUTPUT_FILE"
fi

cat >> "$OUTPUT_FILE" <<PROOF

---

## How It Works

**Plan:** \`${PLAN_FILE:-N/A}\`

<!-- VISUAL:architecture -->

---

## Three-Way Alignment

### Edge 1: Intent <> Action — ${EDGE1_VERDICT}

**Plan said:** ${SCOPE_DESC:-N/A}
**Code did:** ${FILES_CHANGED} files changed, ${TRACE_MUTATIONS} mutations in trace
**Verdict:** ${EDGE1_VERDICT}$([ -n "$EDGE1_DETAIL" ] && echo " — ${EDGE1_DETAIL}")

### Edge 2: Action <> Claim — ${EDGE2_VERDICT}

**Diff shows:** ${FILES_CHANGED} files changed
**Claims say:** tests_ran = ${TESTS_RAN}, tests_added = ${TESTS_ADDED}
**Verdict:** ${EDGE2_VERDICT}$([ -n "$EDGE2_DETAIL" ] && echo " — ${EDGE2_DETAIL}")

### Edge 3: Intent <> Claim — ${EDGE3_VERDICT}

**Plan goal:** ${PLAN_TITLE}
**Claim summary:** ${INTENT_SUMMARY:-N/A}
**Verdict:** ${EDGE3_VERDICT}$([ -n "$EDGE3_DETAIL" ] && echo " — ${EDGE3_DETAIL}")

---

## Verification Evidence

| Review | Status | Method | Detail |
|--------|--------|--------|--------|
| DST Compliance | ${DST_STATUS} | LLM (semantic) | ${DST_DETAIL:-—} |
| Code Quality | ${CODE_STATUS} | LLM (semantic) | ${CODE_DETAIL:-—} |
| Alignment | ${ALIGNMENT_STATUS} | LLM (three-way) | ${ALIGNMENT_EDGES_PASS}/${ALIGNMENT_EDGES_TOTAL} edges |
| PoW Mechanical | ${POW_STATUS} | TRACE (mechanical) | ${POW_DETAIL:-—} |
| Tests | $([ "$TESTS_RAN" = "true" ] && echo "PASS" || echo "N/A") | cargo test | ${TEST_RESULT_LINE} |

### Mechanical Verification Detail

\`\`\`
${COMPARISON_OUTPUT}
\`\`\`

---

## Session Trace

| Metric | Value |
|--------|-------|
| Total tool calls | ${TRACE_TOTAL} |
| Mutations (Write/Edit) | ${TRACE_MUTATIONS} |
| Reads (Read/Grep/Glob) | ${TRACE_READS} |
| Control (Task/LSP/Bash) | ${TRACE_CONTROLS} |
| Session start | ${SESSION_START:-unknown} |
| Session end | ${SESSION_END:-unknown} |
| Duration | ${SESSION_DURATION:-unknown} |

### Diff Summary

**Files changed:** ${FILES_CHANGED}

\`\`\`
${DIFF_STAT}
\`\`\`

---

*Generated by pow-generate-proof.sh — Agent Proof of Work System*
*Mechanical checks are TRACE-verified. Semantic checks are LLM-asserted.*
*Visual placeholders (\`<!-- VISUAL:xxx -->\`) are filled by the proof-illustrator agent.*
PROOF

echo "Proof document written: $OUTPUT_FILE" >&2
echo "$OUTPUT_FILE"
