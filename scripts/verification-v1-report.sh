#!/bin/bash
# verification-v1-report.sh
# Emit a model-agnostic verification report for the current workspace.
# Output schema: verification.v1 (docs/verification.v1.schema.json)
set -euo pipefail

WORKSPACE_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
PROJECT_HASH="$(echo "$WORKSPACE_ROOT" | shasum -a 256 | cut -c1-12)"
MARKER_DIR="/tmp/temper-harness/${PROJECT_HASH}"
mkdir -p "$MARKER_DIR"

SESSION_ID="${CLAUDE_SESSION_ID:-default}"
OUT_FILE=""
PRETTY=false

while [ $# -gt 0 ]; do
    case "$1" in
        --session)
            SESSION_ID="${2:-}"
            shift 2
            ;;
        --out)
            OUT_FILE="${2:-}"
            shift 2
            ;;
        --pretty)
            PRETTY=true
            shift
            ;;
        -h|--help)
            cat <<EOF
Usage: scripts/verification-v1-report.sh [--session <id>] [--out <file>] [--pretty]

Generates a verification.v1 report by normalizing current hook config, git hook
installation, and marker/trace evidence.
EOF
            exit 0
            ;;
        *)
            echo "Unknown argument: $1" >&2
            exit 1
            ;;
    esac
done

if [ -z "$OUT_FILE" ]; then
    OUT_FILE="$MARKER_DIR/verification.v1-${SESSION_ID}.json"
fi
mkdir -p "$(dirname "$OUT_FILE")"

SETTINGS_FILE="$WORKSPACE_ROOT/.claude/settings.json"
PRE_COMMIT_HOOK="$WORKSPACE_ROOT/.git/hooks/pre-commit"
PRE_PUSH_HOOK="$WORKSPACE_ROOT/.git/hooks/pre-push"
POST_COMMIT_HOOK="$WORKSPACE_ROOT/.git/hooks/post-commit"
TRACE_FILE="$MARKER_DIR/trace-${SESSION_ID}.jsonl"

TMP_CHECKS="$(mktemp)"
cleanup() {
    rm -f "$TMP_CHECKS"
}
trap cleanup EXIT

add_check() {
    local ID="$1"
    local NAME="$2"
    local STAGE="$3"
    local BLOCKING="$4"
    local RESULT="$5"
    local EVIDENCE_CLASS="$6"
    local ACCIDENTAL="$7"
    local ADVERSARIAL="$8"
    local PORTABILITY="$9"
    local DETAIL="${10}"
    local ARTIFACTS_JSON="${11:-[]}"

    jq -n \
        --arg id "$ID" \
        --arg name "$NAME" \
        --arg stage "$STAGE" \
        --arg result "$RESULT" \
        --arg evidence_class "$EVIDENCE_CLASS" \
        --arg detail "$DETAIL" \
        --argjson blocking "$BLOCKING" \
        --argjson accidental "$ACCIDENTAL" \
        --argjson adversarial "$ADVERSARIAL" \
        --argjson portability "$PORTABILITY" \
        --argjson artifacts "$ARTIFACTS_JSON" \
        '{
            id: $id,
            name: $name,
            stage: $stage,
            blocking: $blocking,
            result: $result,
            evidence_class: $evidence_class,
            hardness: {
                accidental_regression: $accidental,
                adversarial_bypass: $adversarial,
                portability: $portability
            },
            detail: $detail,
            evidence: {
                artifacts: $artifacts
            }
        }' >> "$TMP_CHECKS"
}

hook_in_settings() {
    local NEEDLE="$1"
    [ -f "$SETTINGS_FILE" ] && grep -q "$NEEDLE" "$SETTINGS_FILE"
}

marker_exists() {
    local NAME="$1"
    [ -f "$MARKER_DIR/$NAME" ] || [ -f "$MARKER_DIR/$NAME.toml" ]
}

find_commit_marker_writers() {
    local PATTERN='(touch .*commit-pending|touch .*sim-changed|> .*commit-pending|> .*sim-changed|write-marker\.sh.*commit-pending|write-marker\.sh.*sim-changed)'
    if command -v rg >/dev/null 2>&1; then
        rg -n --hidden -S "$PATTERN" "$WORKSPACE_ROOT/.claude" "$WORKSPACE_ROOT/scripts" 2>/dev/null || true
    else
        # CI images may not include ripgrep; fall back to grep.
        grep -R -n -E "$PATTERN" "$WORKSPACE_ROOT/.claude" "$WORKSPACE_ROOT/scripts" 2>/dev/null || true
    fi
}

check_hook_config() {
    local ID="$1"
    local NAME="$2"
    local SCRIPT_NAME="$3"
    local STAGE="$4"
    local BLOCKING="$5"
    local A="$6"
    local D="$7"
    local P="$8"
    local ARTIFACTS='[".claude/settings.json"]'

    if hook_in_settings "$SCRIPT_NAME"; then
        add_check "$ID" "$NAME" "$STAGE" "$BLOCKING" "pass" "mechanical" "$A" "$D" "$P" \
            "Configured in .claude/settings.json." "$ARTIFACTS"
    else
        add_check "$ID" "$NAME" "$STAGE" "$BLOCKING" "fail" "mechanical" "$A" "$D" "$P" \
            "Missing from .claude/settings.json." "$ARTIFACTS"
    fi
}

# Hook config checks.
check_hook_config "config.hook.pretool.plan_reminder" "Plan reminder hook configured" "check-plan-reminder.sh" "config" false 0.20 0.05 0.70
check_hook_config "config.hook.posttool.verify_specs" "Spec verification hook configured" "verify-specs.sh" "config" true 0.90 0.45 0.60
check_hook_config "config.hook.posttool.check_deps" "Dependency isolation hook configured" "check-deps.sh" "config" true 0.85 0.50 0.65
check_hook_config "config.hook.posttool.check_determinism" "Determinism hook configured" "check-determinism.sh" "config" true 0.70 0.30 0.65
check_hook_config "config.hook.pretool.pre_commit_review_gate" "Pre-commit review gate configured" "pre-commit-review-gate.sh" "config" true 0.88 0.40 0.55
check_hook_config "config.hook.posttool.post_push_verify" "Post-push verification hook configured" "post-push-verify.sh" "config" false 0.60 0.35 0.60
check_hook_config "config.hook.stop.stop_verify" "Session exit hook configured" "stop-verify.sh" "config" true 0.75 0.30 0.55
check_hook_config "config.hook.posttool.trace_capture" "Trace capture hook configured" "trace-capture.sh" "config" false 0.55 0.40 0.80

# Git hook wrapper installation checks.
if [ -f "$PRE_COMMIT_HOOK" ] && grep -q "\.claude/hooks/pre-commit\.sh" "$PRE_COMMIT_HOOK"; then
    add_check "install.git_hook.pre_commit" "Git pre-commit wrapper installed" "git" true "pass" "mechanical" 0.82 0.50 0.80 \
        "pre-commit wrapper points to .claude/hooks/pre-commit.sh." '[".git/hooks/pre-commit",".claude/hooks/pre-commit.sh"]'
else
    add_check "install.git_hook.pre_commit" "Git pre-commit wrapper installed" "git" true "fail" "mechanical" 0.82 0.50 0.80 \
        "pre-commit wrapper missing or not pointed at .claude/hooks/pre-commit.sh." '[".git/hooks/pre-commit",".claude/hooks/pre-commit.sh"]'
fi

if [ -f "$PRE_PUSH_HOOK" ] && grep -q "\.claude/hooks/pre-push\.sh" "$PRE_PUSH_HOOK"; then
    add_check "install.git_hook.pre_push" "Git pre-push wrapper installed" "git" true "pass" "mechanical" 0.80 0.48 0.80 \
        "pre-push wrapper points to .claude/hooks/pre-push.sh." '[".git/hooks/pre-push",".claude/hooks/pre-push.sh"]'
else
    add_check "install.git_hook.pre_push" "Git pre-push wrapper installed" "git" true "fail" "mechanical" 0.80 0.48 0.80 \
        "pre-push wrapper missing or not pointed at .claude/hooks/pre-push.sh." '[".git/hooks/pre-push",".claude/hooks/pre-push.sh"]'
fi

if [ -f "$POST_COMMIT_HOOK" ] && grep -q "\.claude/hooks/post-commit\.sh" "$POST_COMMIT_HOOK"; then
    add_check "install.git_hook.post_commit" "Git post-commit wrapper installed" "git" true "pass" "mechanical" 0.74 0.42 0.82 \
        "post-commit wrapper points to .claude/hooks/post-commit.sh." '[".git/hooks/post-commit",".claude/hooks/post-commit.sh"]'
else
    add_check "install.git_hook.post_commit" "Git post-commit wrapper installed" "git" true "fail" "mechanical" 0.74 0.42 0.82 \
        "post-commit wrapper missing or not pointed at .claude/hooks/post-commit.sh." '[".git/hooks/post-commit",".claude/hooks/post-commit.sh"]'
fi

# Trace integrity.
if [ -f "$TRACE_FILE" ]; then
    if bash "$WORKSPACE_ROOT/scripts/verify-trace.sh" "$TRACE_FILE" >/dev/null 2>&1; then
        add_check "evidence.trace.integrity" "Trace hash chain integrity" "trace" false "pass" "mechanical" 0.65 0.55 0.85 \
            "Trace exists and hash chain verifies." "$(jq -nc --arg t "$TRACE_FILE" '[$t]')"
    else
        add_check "evidence.trace.integrity" "Trace hash chain integrity" "trace" false "fail" "mechanical" 0.65 0.55 0.85 \
            "Trace exists but hash chain verification failed." "$(jq -nc --arg t "$TRACE_FILE" '[$t]')"
    fi
else
    add_check "evidence.trace.integrity" "Trace hash chain integrity" "trace" false "warn" "mechanical" 0.65 0.55 0.85 \
        "No trace file for this session id." "$(jq -nc --arg t "$TRACE_FILE" '[$t]')"
fi

# Marker checks (presence evidence; requirement is often conditional).
if marker_exists "dst-reviewed"; then
    add_check "evidence.marker.review.dst" "DST review marker present" "review" true "pass" "attestation" 0.55 0.20 0.75 \
        "dst-reviewed marker exists." '["/tmp/temper-harness/*/dst-reviewed","/tmp/temper-harness/*/dst-reviewed.toml"]'
else
    add_check "evidence.marker.review.dst" "DST review marker present" "review" true "warn" "attestation" 0.55 0.20 0.75 \
        "dst-reviewed marker missing (may be conditional or cleaned after successful exit)." '["/tmp/temper-harness/*/dst-reviewed","/tmp/temper-harness/*/dst-reviewed.toml"]'
fi

if marker_exists "code-reviewed"; then
    add_check "evidence.marker.review.code" "Code review marker present" "review" true "pass" "attestation" 0.55 0.20 0.75 \
        "code-reviewed marker exists." '["/tmp/temper-harness/*/code-reviewed","/tmp/temper-harness/*/code-reviewed.toml"]'
else
    add_check "evidence.marker.review.code" "Code review marker present" "review" true "warn" "attestation" 0.55 0.20 0.75 \
        "code-reviewed marker missing (may be cleaned after successful exit)." '["/tmp/temper-harness/*/code-reviewed","/tmp/temper-harness/*/code-reviewed.toml"]'
fi

if marker_exists "alignment-reviewed"; then
    add_check "evidence.marker.alignment_reviewed" "Alignment review marker present" "review" true "pass" "attestation" 0.60 0.25 0.70 \
        "alignment-reviewed marker exists." '["/tmp/temper-harness/*/alignment-reviewed","/tmp/temper-harness/*/alignment-reviewed.toml"]'
else
    add_check "evidence.marker.alignment_reviewed" "Alignment review marker present" "review" true "warn" "attestation" 0.60 0.25 0.70 \
        "alignment-reviewed marker missing." '["/tmp/temper-harness/*/alignment-reviewed","/tmp/temper-harness/*/alignment-reviewed.toml"]'
fi

# Push verification marker consistency.
shopt -s nullglob
PENDING_MARKERS=("$MARKER_DIR"/push-pending-*)
shopt -u nullglob

if [ "${#PENDING_MARKERS[@]}" -eq 0 ]; then
    add_check "evidence.push_post_verify" "Post-push verification marker consistency" "push" true "skip" "mechanical" 0.68 0.30 0.75 \
        "No push-pending markers found." "$(jq -nc --arg d "$MARKER_DIR" '[$d]')"
else
    ALL_VERIFIED=true
    for PENDING in "${PENDING_MARKERS[@]}"; do
        MARKER_SESSION="$(basename "$PENDING" | sed 's/^push-pending-//')"
        if [ ! -f "$MARKER_DIR/test-verified-${MARKER_SESSION}" ] && [ ! -f "$MARKER_DIR/test-verified-${MARKER_SESSION}.toml" ]; then
            ALL_VERIFIED=false
            break
        fi
    done

    if [ "$ALL_VERIFIED" = true ]; then
        add_check "evidence.push_post_verify" "Post-push verification marker consistency" "push" true "pass" "mechanical" 0.68 0.30 0.75 \
            "Every push-pending marker has matching test-verified evidence." "$(jq -nc --arg d "$MARKER_DIR" '[$d]')"
    else
        add_check "evidence.push_post_verify" "Post-push verification marker consistency" "push" true "fail" "mechanical" 0.68 0.30 0.75 \
            "Found push-pending marker(s) without matching test-verified marker." "$(jq -nc --arg d "$MARKER_DIR" '[$d]')"
    fi
fi

# Wiring check: stop-gate commit markers are checked but currently unwritten.
# Only count concrete write operations (redirection/touch/write-marker), not generic mentions.
COMMIT_MARKER_WRITERS="$(find_commit_marker_writers \
    | grep -v '/.claude/hooks/stop-verify.sh:' \
    | grep -v '/scripts/verification-v1-report.sh:' || true)"
if [ -n "$COMMIT_MARKER_WRITERS" ]; then
    add_check "wiring.exit_gate.commit_markers" "Stop-gate commit markers are wired" "wiring" true "pass" "mechanical" 0.72 0.35 0.82 \
        "Found writer(s) for commit-pending/sim-changed markers." '[".claude/hooks/stop-verify.sh",".claude/hooks/pre-commit-review-gate.sh","scripts/*"]'
else
    add_check "wiring.exit_gate.commit_markers" "Stop-gate commit markers are wired" "wiring" true "fail" "mechanical" 0.72 0.35 0.82 \
        "stop-verify checks commit-pending/sim-changed but no writer was found." '[".claude/hooks/stop-verify.sh",".claude/hooks/pre-commit-review-gate.sh","scripts/*"]'
fi

# Wiring check: marker session binding.
if grep -q 'marker_exists()' "$WORKSPACE_ROOT/.claude/hooks/pre-commit-review-gate.sh" \
    && ! grep -q 'session_id' "$WORKSPACE_ROOT/.claude/hooks/pre-commit-review-gate.sh"; then
    add_check "wiring.marker.session_binding" "Marker checks are session/change-bound" "wiring" false "warn" "inferred" 0.35 0.10 0.85 \
        "Gate checks marker presence but does not verify marker session_id or change-set freshness." '[".claude/hooks/pre-commit-review-gate.sh","scripts/write-marker.sh"]'
else
    add_check "wiring.marker.session_binding" "Marker checks are session/change-bound" "wiring" false "pass" "inferred" 0.35 0.10 0.85 \
        "Marker session/change binding appears configured." '[".claude/hooks/pre-commit-review-gate.sh","scripts/write-marker.sh"]'
fi

# Wiring check: determinism gate behavior at push time.
if grep -q 'Determinism is advisory' "$WORKSPACE_ROOT/.claude/hooks/pre-push.sh"; then
    add_check "wiring.pre_push_determinism_blocking" "Pre-push determinism enforcement is blocking" "wiring" true "warn" "mechanical" 0.30 0.10 0.80 \
        "pre-push determinism check is currently advisory in practice." '[".claude/hooks/pre-push.sh","scripts/check-determinism.sh"]'
else
    add_check "wiring.pre_push_determinism_blocking" "Pre-push determinism enforcement is blocking" "wiring" true "pass" "mechanical" 0.30 0.10 0.80 \
        "pre-push determinism check appears blocking." '[".claude/hooks/pre-push.sh","scripts/check-determinism.sh"]'
fi

CHECKS_JSON="$(jq -s '.' "$TMP_CHECKS")"

CHECKS_TOTAL="$(echo "$CHECKS_JSON" | jq 'length')"
CHECKS_PASSED="$(echo "$CHECKS_JSON" | jq '[.[] | select(.result == "pass")] | length')"
CHECKS_FAILED="$(echo "$CHECKS_JSON" | jq '[.[] | select(.result == "fail")] | length')"
CHECKS_WARNED="$(echo "$CHECKS_JSON" | jq '[.[] | select(.result == "warn")] | length')"
CHECKS_SKIPPED="$(echo "$CHECKS_JSON" | jq '[.[] | select(.result == "skip")] | length')"
CHECKS_UNKNOWN="$(echo "$CHECKS_JSON" | jq '[.[] | select(.result == "unknown")] | length')"
BLOCKING_FAILURES="$(echo "$CHECKS_JSON" | jq '[.[] | select(.blocking == true and .result == "fail")] | length')"

AVG_ACCIDENTAL="$(echo "$CHECKS_JSON" | jq 'if length == 0 then 0 else ([.[].hardness.accidental_regression] | add / length) end')"
AVG_ADVERSARIAL="$(echo "$CHECKS_JSON" | jq 'if length == 0 then 0 else ([.[].hardness.adversarial_bypass] | add / length) end')"
AVG_PORTABILITY="$(echo "$CHECKS_JSON" | jq 'if length == 0 then 0 else ([.[].hardness.portability] | add / length) end')"

OVERALL_RESULT="pass"
if [ "$BLOCKING_FAILURES" -gt 0 ] || [ "$CHECKS_FAILED" -gt 0 ]; then
    OVERALL_RESULT="fail"
elif [ "$CHECKS_WARNED" -gt 0 ]; then
    OVERALL_RESULT="warn"
fi

COMMIT_HEAD="$(git -C "$WORKSPACE_ROOT" rev-parse HEAD 2>/dev/null || echo "unknown")"
BRANCH="$(git -C "$WORKSPACE_ROOT" rev-parse --abbrev-ref HEAD 2>/dev/null || echo "unknown")"
DIRTY=false
if ! git -C "$WORKSPACE_ROOT" diff --quiet 2>/dev/null || ! git -C "$WORKSPACE_ROOT" diff --cached --quiet 2>/dev/null; then
    DIRTY=true
fi

AGENT_PROVIDER="${VERIFICATION_AGENT_PROVIDER:-unknown}"
AGENT_MODEL="${VERIFICATION_AGENT_MODEL:-unknown}"
if [ "$AGENT_PROVIDER" = "unknown" ] && [ -n "${CLAUDE_SESSION_ID:-}" ]; then
    AGENT_PROVIDER="anthropic"
    AGENT_MODEL="${CLAUDE_MODEL:-unknown}"
fi

RUN_ID="${SESSION_ID}-$(date +%s)"
GENERATED_AT="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

REPORT_JSON="$(jq -n \
    --arg schema "verification.v1" \
    --arg run_id "$RUN_ID" \
    --arg generated_at "$GENERATED_AT" \
    --arg repo_path "$WORKSPACE_ROOT" \
    --arg project_hash "$PROJECT_HASH" \
    --arg commit_head "$COMMIT_HEAD" \
    --arg branch "$BRANCH" \
    --arg provider "$AGENT_PROVIDER" \
    --arg model "$AGENT_MODEL" \
    --arg session_id "$SESSION_ID" \
    --arg overall_result "$OVERALL_RESULT" \
    --argjson dirty "$DIRTY" \
    --argjson checks_total "$CHECKS_TOTAL" \
    --argjson checks_passed "$CHECKS_PASSED" \
    --argjson checks_failed "$CHECKS_FAILED" \
    --argjson checks_warned "$CHECKS_WARNED" \
    --argjson checks_skipped "$CHECKS_SKIPPED" \
    --argjson checks_unknown "$CHECKS_UNKNOWN" \
    --argjson blocking_failures "$BLOCKING_FAILURES" \
    --argjson accidental "$AVG_ACCIDENTAL" \
    --argjson adversarial "$AVG_ADVERSARIAL" \
    --argjson portability "$AVG_PORTABILITY" \
    --argjson checks "$CHECKS_JSON" \
    '{
        schema: $schema,
        run_id: $run_id,
        generated_at: $generated_at,
        repository: {
            path: $repo_path,
            project_hash: $project_hash,
            commit_head: $commit_head,
            branch: $branch,
            dirty: $dirty
        },
        agent: {
            provider: $provider,
            model: $model,
            session_id: $session_id
        },
        summary: {
            overall_result: $overall_result,
            checks_total: $checks_total,
            checks_passed: $checks_passed,
            checks_failed: $checks_failed,
            checks_warned: $checks_warned,
            checks_skipped: $checks_skipped,
            checks_unknown: $checks_unknown,
            blocking_failures: $blocking_failures,
            overall_hardness: {
                accidental_regression: $accidental,
                adversarial_bypass: $adversarial,
                portability: $portability
            }
        },
        checks: $checks
    }')"

if [ "$PRETTY" = true ]; then
    echo "$REPORT_JSON" | jq '.' > "$OUT_FILE"
else
    echo "$REPORT_JSON" | jq -c '.' > "$OUT_FILE"
fi

echo "verification.v1 report written: $OUT_FILE"
echo "overall_result=$OVERALL_RESULT checks_total=$CHECKS_TOTAL failed=$CHECKS_FAILED warned=$CHECKS_WARNED blocking_failures=$BLOCKING_FAILURES"
