#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TEMPERPAW_WORKTREE="${TEMPERPAW_WORKTREE:-/Users/seshendranalla/Development/temperpaw-worktrees/latency-observability-program}"
MODE="${1:-quick}"

if [[ "$MODE" != "quick" && "$MODE" != "full" ]]; then
  echo "usage: $0 [quick|full]" >&2
  exit 2
fi

pass() {
  printf 'ok: %s\n' "$1"
}

fail() {
  printf 'error: %s\n' "$1" >&2
  exit 1
}

require_file() {
  local path="$1"
  [[ -f "$path" ]] || fail "missing file: $path"
  pass "found ${path#$ROOT/}"
}

require_pattern() {
  local path="$1"
  local pattern="$2"
  local label="$3"
  rg -q --fixed-strings "$pattern" "$path" || fail "$label missing from ${path#$ROOT/}"
  pass "$label"
}

require_regex() {
  local path="$1"
  local pattern="$2"
  local label="$3"
  rg -q "$pattern" "$path" || fail "$label missing from ${path#$ROOT/}"
  pass "$label"
}

echo "== Latency/observability package preflight =="
echo "Temper worktree: $ROOT"
echo "TemperPaw worktree: $TEMPERPAW_WORKTREE"
echo "Mode: $MODE"
echo

require_file "$ROOT/docs/temper-latency-observability-report.html"
require_file "$ROOT/docs/adrs/0081-latency-observability-acceleration-program.md"
require_file "$ROOT/docs/adrs/0082-projection-correctness-observability.md"
require_file "$ROOT/docs/adrs/0083-trace-budget-and-fanout-summarization.md"
require_file "$ROOT/docs/adrs/0084-authz-latency-phase-instrumentation.md"
require_file "$ROOT/docs/runbooks/datadog-postgres-dbm.md"
require_file "$ROOT/docs/runbooks/latency-observability-release.md"
require_file "$ROOT/scripts/datadog-postgres-dbm-setup.sql"

require_pattern "$ROOT/docs/temper-latency-observability-report.html" "Program Progress" "living dashboard progress section"
require_pattern "$ROOT/docs/temper-latency-observability-report.html" "OBS-005" "OBS-005 dashboard task"
require_pattern "$ROOT/docs/temper-latency-observability-report.html" "0083-trace-budget-and-fanout-summarization" "ADR-0083 dashboard link"
require_pattern "$ROOT/docs/temper-latency-observability-report.html" "0084-authz-latency-phase-instrumentation" "ADR-0084 dashboard link"
require_pattern "$ROOT/docs/temper-latency-observability-report.html" "live deployment" "live deployment blocker recorded"

require_pattern "$ROOT/crates/temper-server/src/profiling.rs" "TEMPER_PROFILING_CONTINUOUS" "profiler continuous gate"
require_pattern "$ROOT/crates/temper-server/src/profiling/metrics.rs" "datadog.profiling.rust.profiles_uploaded" "profiler upload metric"
require_pattern "$ROOT/crates/temper-store-postgres/src/metrics.rs" "temper_postgres_transaction_duration_ms" "Postgres transaction metric"
require_pattern "$ROOT/crates/temper-store-postgres/src/metrics.rs" "temper_postgres_pool_acquire_duration_ms" "Postgres pool metric"
require_pattern "$ROOT/crates/temper-server/src/query_projection_metrics.rs" "temper_query_projection_replay_parity_check_total" "projection replay parity metric"
require_pattern "$ROOT/crates/temper-server/src/odata/read_support/shadow.rs" "TEMPER_ODATA_CATALOG_SHADOW_READ_EVERY" "projection shadow-read gate"
require_regex "$ROOT/crates/temper-store-turso/src/schema/query_plane.rs" "fields[[:space:]]+TEXT NOT NULL DEFAULT '\\{\\}'" "Turso catalog fields column"
require_pattern "$ROOT/crates/temper-observe/src/otel.rs" "temper_trace_sampler_decisions_total" "trace sampler decision metric"
require_pattern "$ROOT/crates/temper-observe/src/otel.rs" "TEMPER_TRACE_DISPATCH_BACKGROUND_SAMPLE_PCT" "dispatch background trace budget flag"
require_pattern "$ROOT/crates/temper-server/src/trigger/dispatcher.rs" "reaction.fired_count" "reaction fanout summary span"
require_pattern "$ROOT/crates/temper-authz/src/metrics.rs" "temper_cedar_evaluation_duration_ms" "Cedar duration ms metric"
require_pattern "$ROOT/crates/temper-authz/src/metrics.rs" "temper_cedar_evaluation_phase_duration_ms" "Cedar phase duration metric"
require_pattern "$ROOT/crates/temper-authz/src/metrics.rs" "temper_cedar_request_attribute_count" "Cedar request shape metric"
require_pattern "$ROOT/crates/temper-authz/src/engine/mod.rs" "CedarEvaluationRecorder" "Cedar phase recorder"

require_pattern "$ROOT/docs/runbooks/latency-observability-release.md" "TEMPER_TRACE_DISPATCH_BACKGROUND_SAMPLE_PCT=25" "trace budget runtime flag in runbook"
require_pattern "$ROOT/docs/runbooks/latency-observability-release.md" "temper_cedar_evaluation_phase_duration_ms" "Cedar phase metric in runbook"
require_pattern "$ROOT/docs/runbooks/latency-observability-release.md" "Datadog Metric Configuration" "Datadog metric config section"
require_pattern "$ROOT/docs/runbooks/latency-observability-release.md" "Trace budget" "trace budget live proof row"
require_pattern "$ROOT/docs/runbooks/latency-observability-release.md" "python3 scripts/temper_agent_e2e_proof.py" "e2e proof command in runbook"

if [[ ! -d "$TEMPERPAW_WORKTREE" ]]; then
  fail "TemperPaw worktree not found: $TEMPERPAW_WORKTREE"
fi

require_file "$TEMPERPAW_WORKTREE/dd-dashboards/temperpaw-overview.json"
require_file "$TEMPERPAW_WORKTREE/dd-monitors/temperpaw-monitors.json"
require_file "$TEMPERPAW_WORKTREE/scripts/read_datadog_snapshot.py"
python3 -m json.tool "$TEMPERPAW_WORKTREE/dd-dashboards/temperpaw-overview.json" >/dev/null
pass "TemperPaw dashboard JSON parses"
python3 -m json.tool "$TEMPERPAW_WORKTREE/dd-monitors/temperpaw-monitors.json" >/dev/null
pass "TemperPaw monitor JSON parses"
python3 -m py_compile "$TEMPERPAW_WORKTREE/scripts/read_datadog_snapshot.py"
pass "TemperPaw Datadog snapshot helper compiles"
require_pattern "$TEMPERPAW_WORKTREE/dd-dashboards/temperpaw-overview.json" "Trace Budget (ADR-0083)" "Trace Budget dashboard group"
require_pattern "$TEMPERPAW_WORKTREE/dd-dashboards/temperpaw-overview.json" "service:temperpaw" "Railway production service tag in dashboard"
require_pattern "$TEMPERPAW_WORKTREE/dd-dashboards/temperpaw-overview.json" "temper_trace_sampler_decisions_total" "trace sampler dashboard query"
require_pattern "$TEMPERPAW_WORKTREE/dd-dashboards/temperpaw-overview.json" "AuthZ Phase Breakdown (ADR-0084)" "AuthZ phase dashboard group"
require_pattern "$TEMPERPAW_WORKTREE/dd-dashboards/temperpaw-overview.json" "temper_cedar_evaluation_phase_duration_ms" "Cedar phase dashboard query"
if rg -q --fixed-strings "service:openpaw" "$TEMPERPAW_WORKTREE/dd-dashboards/temperpaw-overview.json" "$TEMPERPAW_WORKTREE/dd-monitors/temperpaw-monitors.json"; then
  fail "TemperPaw Datadog config still targets service:openpaw; Railway production exports DD_SERVICE=temperpaw"
fi
require_pattern "$TEMPERPAW_WORKTREE/dd-monitors/temperpaw-monitors.json" "[Temper] Trace Sampler Metrics Missing" "trace sampler missing monitor"
require_pattern "$TEMPERPAW_WORKTREE/dd-monitors/temperpaw-monitors.json" "service:temperpaw" "Railway production service tag in monitors"
require_pattern "$TEMPERPAW_WORKTREE/dd-monitors/temperpaw-monitors.json" "[Temper] Trace Sampler Delegated Volume Spike" "delegated volume monitor"
require_pattern "$TEMPERPAW_WORKTREE/dd-monitors/temperpaw-monitors.json" "[Temper] Dispatch Background Trace Budget Disabled" "trace budget disabled monitor"
require_pattern "$TEMPERPAW_WORKTREE/dd-monitors/temperpaw-monitors.json" "[Temper] Cedar Evaluation Duration Max Regression" "Cedar max fallback monitor"
require_pattern "$TEMPERPAW_WORKTREE/dd-monitors/temperpaw-monitors.json" "[Temper] Cedar AuthZ Phase Error" "Cedar phase error monitor"

if [[ "$MODE" == "full" ]]; then
  echo
  echo "== Full focused verification =="
  (
    cd "$ROOT"
    cargo fmt --check
    cargo check -p temper-cli
    cargo check -p temper-server
    cargo test -p temper-authz -- --nocapture
    cargo test -p temper-server profiling::tests --lib -- --nocapture
    cargo test -p temper-observe otel --lib -- --nocapture
    cargo test -p temper-store-postgres metrics --lib -- --nocapture
    cargo test -p temper-server query_projection_metrics --lib -- --nocapture
    cargo test -p temper-server odata::read_support --lib -- --nocapture
    cargo test -p temper-server --test query_projection_backfill -- --nocapture
    cargo test -p temper-store-turso load_entity_catalog_rows_preserves_projected_fields_json --lib -- --nocapture
    cargo test -p temper-store-turso export_query_projections_returns_all_fields_for_migration --lib -- --nocapture
    git diff --check
  )
  (
    cd "$TEMPERPAW_WORKTREE"
    git diff --check
  )
  pass "full focused verification passed"
else
  echo
  echo "quick mode skipped cargo tests; run '$0 full' before PR."
fi

echo
pass "latency/observability package preflight passed"
