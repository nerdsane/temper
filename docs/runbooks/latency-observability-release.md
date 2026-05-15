# Temper Latency Observability Release Runbook

Status: OBS-001 through OBS-005 complete locally; PERF-001 measurement prep in progress
Owner: Temper/OpenPaw operator
Living dashboard: `docs/temper-latency-observability-report.html`

## Purpose

This runbook turns the local latency/observability repair slices into a
repeatable release. The goal is not "merge code and hope Datadog lights up";
the goal is to prove that every new measurement surface is visible, current,
and useful before using it to drive performance work.

## Repositories And Order

Use the main-based worktrees created for this program:

- Temper:
  `/Users/seshendranalla/Development/temper-worktrees/latency-observability-program`
- TemperPaw:
  `/Users/seshendranalla/Development/temperpaw-worktrees/latency-observability-program`

Release order:

1. Merge and deploy the Temper runtime/store instrumentation.
2. Run the DBM setup SQL against staging/live Postgres.
3. Deploy the TemperPaw Datadog dashboard and monitor config.
4. Enable guarded runtime flags and run live verification.
5. Record links/screenshots and status in the living dashboard.

## Local Verification Before PR

Fast package consistency check:

```sh
scripts/verify-latency-observability-package.sh quick
```

Run these in the Temper worktree:

```sh
cargo fmt --check
cargo check -p temper-cli
cargo test -p temper-server profiling::tests --lib -- --nocapture
cargo test -p temper-store-postgres metrics --lib -- --nocapture
cargo test -p temper-observe otel --lib -- --nocapture
cargo test -p temper-authz -- --nocapture
cargo check -p temper-server
cargo test -p temper-server query_projection_metrics --lib -- --nocapture
cargo test -p temper-server odata::read_support --lib -- --nocapture
cargo test -p temper-server --test query_projection_backfill -- --nocapture
cargo test -p temper-store-turso load_entity_catalog_rows_preserves_projected_fields_json --lib -- --nocapture
cargo test -p temper-store-turso query_projection_catalog_preserves_projected_fields --lib -- --nocapture
cargo test -p temper-store-turso export_query_projections_returns_all_fields_for_migration --lib -- --nocapture
git diff --check
```

Run these in the TemperPaw worktree:

```sh
python3 -m json.tool dd-dashboards/temperpaw-overview.json >/tmp/temperpaw-overview.json.check
python3 -m json.tool dd-monitors/temperpaw-monitors.json >/tmp/temperpaw-monitors.json.check
git diff --check
```

Or run the focused full preflight, which executes the static package checks,
the focused Temper checks above, and the TemperPaw JSON/diff checks:

```sh
scripts/verify-latency-observability-package.sh full
```

Before committing Temper code, run the required project reviews described in
`docs/HARNESS.md` and `AGENTS.md`: DST compliance review for sim-visible code
and code-quality review for the full change set.

## PR Split

Use two PRs so runtime behavior and Datadog configuration can be reviewed and
rolled back independently:

1. **Temper runtime/store observability PR**
   - ADR-0081, ADR-0082, ADR-0083, and ADR-0084.
   - Profiler freshness/capture/upload metrics and continuous profiler gate.
   - Postgres app-side pool/transaction/projection metrics.
   - Projection queue/backfill/shadow/replay-parity metrics.
   - Turso `entity_catalog.fields` preservation and migration.
   - Trace sampler decision/config/rate metrics and reaction fanout summary
     span fields.
   - Cedar/AuthZ canonical counter cleanup, millisecond duration metric, phase
     duration metrics, and request-shape histograms.
   - DBM setup SQL and runbooks.

2. **TemperPaw Datadog config PR**
   - Dashboard widgets for projection correctness, replay parity, Postgres,
     dispatch, WASM, blob, Monty, session tail latency, and trace budgets.
   - Monitors for drift/errors and p95/p99 regressions.
   - Monitors for missing sampler metrics, unusual delegated span volume, and
     disabled background trace budgeting.
   - AuthZ phase dashboard widgets and monitors for Cedar max-duration fallback
     and error-path phase instrumentation.
   - The request-latency monitor must use `p95:temper_dispatch_ask_latency_ms`,
     not an average.

Merge Temper first. Merge/deploy the Datadog config after the runtime emits the
new metrics in at least staging, otherwise the config can validate but not
prove usefulness.

## Runtime Flags

Profiler gates:

```sh
TEMPER_PROFILING_ENABLED=true
TEMPER_PROFILING_CONTINUOUS=true
TEMPER_PROFILING_AUTO_UPLOAD=true
```

Use conservative initial profiler windows/intervals unless the deployment
already has an accepted profile cost budget. Compare the in-process profiler
signal with Datadog `ddprof` before deciding which path is the long-term Rust
profiling standard.

Projection shadow checks:

```sh
TEMPER_ODATA_CATALOG_SHADOW_READ_EVERY=1000
```

Start with a low sample rate. Increase only after Datadog shows no routine
shadow drift/errors and the additional actor reads do not affect request
latency.

Catalog-fast reads:

```sh
TEMPER_ODATA_CATALOG_FAST_READ=false
```

Keep catalog-fast reads off for broad rollout until replay parity is clean on
deployed data. Enable per environment only after a clean parity window is
recorded in the living dashboard.

Trace budget:

```sh
TEMPER_TRACE_WASM_AUX_SAMPLE_PCT=5
TEMPER_TRACE_DISPATCH_BACKGROUND_SAMPLE_PCT=25
```

Raise either value temporarily during an incident when full child-span detail
is more important than trace volume. Restore conservative values before normal
traffic if high-fanout traces approach the 100k-span failure mode observed in
production.

## DBM Repair

Run the DBM setup SQL in each monitored Postgres database:

```sh
psql "$DATABASE_URL" -f scripts/datadog-postgres-dbm-setup.sql
```

If the DBM Agent uses a role other than the default `datadog`, pass it
explicitly. Railway production on May 15, 2026 used the
`datadog-postgres-agent` service with `PGUSER=postgres`:

```sh
psql "$DATABASE_URL" -v dbm_agent_role=postgres -f scripts/datadog-postgres-dbm-setup.sql
```

Then follow `docs/runbooks/datadog-postgres-dbm.md` to validate:

- `datadog.pg_stat_activity()`,
- `datadog.pg_stat_statements()`,
- `datadog.explain_statement(TEXT)`,
- no routine `plan.collection_errors:invalid_schema` on hot Temper queries.

## Deploy Datadog Config

Railway production discovery on May 14, 2026:

- Railway project: `openpaw-seshendranalla`
- Railway environment: `production`
- Railway service: `openpaw`
- Public domains: `https://openpaw-production.up.railway.app` and `https://temperpaw.katagami.ai`
- Runtime Datadog tag: `DD_SERVICE=temperpaw`
- Current production build variables observed: `BUILD_VERSION=sha-6c352c5`

The Railway service is named `openpaw`, but Datadog dashboard and monitor
queries must target `service:temperpaw` unless production intentionally changes
`DD_SERVICE`. The package preflight fails if TemperPaw Datadog config regresses
to `service:openpaw`.

Requires `DD_API_KEY`, `DD_APP_KEY`, and optionally `DD_SITE`. These are present
in the Railway production environment for the `openpaw` service as of the
May 14, 2026 check. Prefer `railway run` for dry-runs so the local shell does
not need secret exports.

Dry-run monitors first:

```sh
railway run --service openpaw --environment production -- python3 scripts/deploy_monitors.py --dry-run
```

May 14, 2026 dry-run result: the Railway-injected Datadog credentials worked
and the script would create all 62 source-of-truth monitors under the
`team:openpaw` monitor tag scope. On May 15, 2026, the live source set was
expanded to 66 monitors with four standard APM coverage monitors, and the
Datadog deploy created or updated the full set. Before running future
`--reconcile` deployments, confirm that older dashboards or monitors are not
still intentionally managed under different tags or names.

Deploy dashboard:

```sh
railway run --service openpaw --environment production -- python3 scripts/deploy_dashboard.py dd-dashboards/temperpaw-overview.json
```

Deploy monitors:

```sh
railway run --service openpaw --environment production -- python3 scripts/deploy_monitors.py
```

Record the dashboard URL and newly created/updated monitor IDs in the living
dashboard.

## Railway Health Probe

Production health probe on May 14, 2026:

```sh
curl -fsS https://openpaw-production.up.railway.app/readyz
curl -i -fsS https://openpaw-production.up.railway.app/healthz
```

Observed result: `/readyz` returned JSON with `status:"ready"` and Discord
connected/configured, while `/healthz` returned HTTP 200 with an empty body.
The custom domain `temperpaw.katagami.ai` did not resolve from this local
environment during the check, so use the Railway domain for immediate smoke
tests unless DNS is repaired or verified from another network.

## Read-Only Datadog Snapshot

Use Railway-injected Datadog credentials for read-only metric snapshots:

```sh
railway run --service openpaw --environment production -- python3 scripts/read_datadog_snapshot.py
```

May 14, 2026 result for `service:temperpaw`:

- `temper_up` returned one live series over 1h/24h with latest value `1`.
- `temper_cedar_evaluations_total` returned counted traffic: 8,876 over 1h and
  17,005 over 24h in the query output.
- `temper_cedar_evaluation_duration` returned live averages; the 24h max
  sampled value was about 122 ms.
- `p95:temper_dispatch_ask_latency_ms`, new Postgres p95 metrics, projection
  update errors, and profiler upload/error metrics returned no series.

Interpretation: the deployed service and Cedar metrics are live under
`service:temperpaw`, but profiler and several tail/correctness surfaces remain
measurement gaps until this package is deployed and Datadog percentile
aggregations are configured.

## Datadog Metric Configuration

Before p95/p99 gates are treated as live, enable percentile aggregations for
the distribution metrics that drive the latency program:

- `temper_cedar_evaluation_duration`
- `temper_cedar_evaluation_duration_ms`
- `temper_cedar_evaluation_phase_duration_ms`
- `temper_dispatch_ask_latency_ms`
- `temper_query_projection_update_duration_ms`
- `temper_query_projection_update_end_to_end_duration_ms`
- `temper_postgres_pool_acquire_duration_ms`
- `temper_postgres_transaction_duration_ms`

This is a required Datadog configuration step. The local code can emit
histograms, but Datadog must be configured to expose p95/p99 aggregations. If a
percentile widget or monitor is No Data while avg/max is live, treat it as a
measurement gap, not a product latency conclusion.

The TemperPaw Datadog config repository keeps this step repeatable:

```sh
railway run --service openpaw --environment production -- python3 scripts/configure_metric_percentiles.py
railway run --service openpaw --environment production -- python3 scripts/configure_metric_percentiles.py --apply
```

The script manages a bounded list of latency distribution metrics and excludes
known high-cardinality tags such as `session_id` from the queryable tag list.
Datadog cannot configure metrics that have never emitted, so missing future
runtime metrics are skipped and must be re-run after the Temper runtime PR is
deployed.

## Live Verification

Verify every signal in Datadog after deployment. Passing local tests is not a
substitute for this table.

| Area | Required live proof |
| --- | --- |
| Profiler | Fresh `datadog.profiling.rust.profiles_uploaded` for `service:temperpaw`, upload errors near zero, flamegraph visible for the deployed version. |
| AuthZ | `temper_cedar_evaluation_duration_ms`, `temper_cedar_evaluation_phase_duration_ms`, and `temper_cedar_request_attribute_count` visible; p95/p99 aggregations enabled; duplicate `temper-authz` counter scope absent from new versions. |
| DBM | Hot `entity_catalog`, `entity_field_index`, and event append/query signatures have explain plans without `invalid_schema`. |
| Postgres app metrics | p50/p95/p99 visible for pool acquire and transaction duration by `operation` and `outcome`. |
| Projection updates | Queue wait, update duration, end-to-end duration, applied sequence, and update errors visible by `source`, `entity_type`, and operation. |
| Shadow checks | Shadow check match/drift/error rates visible; no unexplained drift before increasing sampling or enabling fast reads. |
| Replay parity | Replay parity match/drift/error rates visible; parity clean before projection write-amplification changes. |
| Turso catalog | Fast-read catalog rows preserve JSON types and parity stays clean after migration/backfill. |
| Trace budget | `temper_trace_sampler_*` metrics visible; sampler configured-rule gauges are nonzero; high-fanout smoke traces retain roots and reaction summaries without routine 100k-span child traces. |
| Dispatch | `temper_dispatch_ask_latency_ms` p95/p99 visible and request-latency monitor evaluates percentile data. |
| WASM | Invocation and host HTTP duration p95/p99 visible by trigger/call kind. |
| Blob/Monty | Blob I/O, blob transport, and Monty wait p95/p99 visible. |
| Sessions | Context prepare and session phase p95/p99 visible by phase/result where applicable. |

## Live End-To-End Runs

Run at least one staging and one live smoke path that exercises:

1. entity creation,
2. entity action dispatch,
3. OData collection read,
4. projection update and backfill/parity signal,
5. a session/tool/WASM path if available in the environment,
6. a blob/content path if available in the environment.

For the existing Temper agent proof harness, run:

```sh
python3 scripts/temper_agent_e2e_proof.py
```

The script writes its markdown proof to `.proof/temper-agent-e2e-proof.md` and
JSON artifacts under `.tmp/temper-agent-proof/artifacts`. Attach or link the
relevant proof summary from the living dashboard after staging/live execution.

For each run, record:

- exact environment,
- deployed git SHA/version,
- start/end time,
- request/trace links,
- Datadog dashboard screenshot or URL,
- any monitor state changes,
- pass/fail result.

## Rollback

Runtime rollback:

- Disable profiler continuous upload flags.
- Set `TEMPER_ODATA_CATALOG_SHADOW_READ_EVERY=0`.
- Keep `TEMPER_ODATA_CATALOG_FAST_READ=false`.
- Roll back runtime deployment if metrics cause unexpected overhead.

Datadog config rollback:

- Revert the TemperPaw Datadog PR and run the dashboard/monitor deploy scripts.
- Prefer disabling noisy monitors over deleting evidence while investigating
  threshold tuning.

## Done Criteria

This release package is done only when:

- both PRs are merged,
- runtime is deployed,
- DBM repair is applied and verified,
- Datadog dashboard/monitors are deployed,
- live e2e runs pass,
- fresh Datadog evidence is recorded in
  `docs/temper-latency-observability-report.html`,
- the living dashboard names any remaining optimization work with evidence.
