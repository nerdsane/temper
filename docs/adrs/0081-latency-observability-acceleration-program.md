# ADR-0081: Latency and Observability Acceleration Program

- Status: Proposed
- Date: 2026-05-14
- Deciders: Temper core maintainers
- Related:
  - ADR-0030: Hash-Gated Verification Cascade
  - ADR-0028: Memory-bounded lazy hydration and passivation
  - ADR-0029: Temper filesystem
  - ADR-0034: GEPA self-improvement loop
  - ADR-0052: Instrumentation as policy
  - ADR-0055: Continuous profiling
  - ADR-0057: Canonical dispatch traces and selective wide-event projection
  - ADR-0058: Query-plane hot-field opt-out and stable projections
  - ADR-0063: Object store for blob bytes
  - ADR-0077: Catalog-first OData materialization
  - ADR-0080: Agent-governed mutation denials
  - `docs/temper-latency-observability-report.html`
  - `crates/temper-observe/`
  - `crates/temper-authz/`
  - `crates/temper-server/src/state/dispatch/`
  - `crates/temper-store-postgres/`

## Context

Temper's mission is to let developers and agents create applications from
conversation while preserving verified behavior, Cedar governance, tenant
isolation, deterministic simulation, durable event audit, and an evolution loop
grounded in real trajectories. Latency is now a first-order product and
architecture requirement: developer chat, production chat, entity transitions,
query reads, integrations, and file/blob operations all need to feel fast while
remaining correct.

Live Datadog evidence for the production `temperpaw` service shows that the
oldest local diagnosis is only partially complete:

- The verified transition table and actor admission paths are not the observed
  bottleneck. Actor spawn/admission is generally sub-millisecond, and local JIT
  transition evaluation is microsecond-scale.
- Production has a newer projection read plane with `entity_catalog`,
  `entity_field_index`, and `dispatch.phase.query_projection` spans that are
  not present in this local checkout. That plane appears to make regular OData
  reads mostly healthy, with `GET /odata/{path}` around single-digit p50 and
  tens-of-milliseconds p99.
- Projection writes now show the hotter tradeoff: per-field write
  amplification, fan-out, and trace cardinality. One observed trace contained
  roughly 166k spans, including roughly 104k field-index insert spans.
- Cedar authorization is CPU-heavy in production traces, with p95 around
  hundreds of milliseconds for `entity.authorize_with_context`.
- WASM/external integrations and blob upload/data-plane work can take seconds
  to tens of seconds and should not define the whole control-plane latency.
- Observability is rich but incomplete for this goal. A live check on
  2026-05-14 found Rust profiling metrics registered, but
  `datadog.profiling.rust.profiles_uploaded{service:temperpaw}` returned
  CPU upload points with value 0 over the sampled 24-hour window. DBM sees
  `temperpaw-postgres` as healthy, but a plan search returned 2,408 samples
  with `plan.collection_errors` / `invalid_schema`, making plan-level
  diagnosis unreliable until repaired. Some metrics/monitors also use averages
  or unsupported percentile queries, and projection correctness observability
  is not yet sufficient to prove absence of drift.

This ADR establishes the program structure. It intentionally does not approve a
single large rewrite. The work must proceed in small, measurable slices, each
with correctness evidence and a dashboard update.

## Decision

### Sub-Decision 1: Treat the HTML report as the living program dashboard

`docs/temper-latency-observability-report.html` is the canonical program
dashboard for this work. It must include:

- Current diagnosis and architecture diagrams.
- Proposed target architecture diagrams.
- A progress bar, milestone table, current branch/worktree, current phase, and
  status log.
- Task list with status, evidence required, and exit criteria.
- Links or references to ADRs, PRs, test output, live run results, deployment
  evidence, and Datadog evidence as they become available.

**Why this approach**: The work spans architecture, instrumentation,
production observability, performance changes, and deployment. A living
dashboard gives maintainers a single readable status surface and prevents
progress from disappearing into chat history.

### Sub-Decision 2: Work only in main-based worktrees

Any Temper or TemperPaw repository changes for this program must happen in a
separate git worktree branched from `main`. Agents must not switch the existing
checkout in place for this work.

Initial Temper worktree:

```text
/Users/seshendranalla/Development/temper-worktrees/latency-observability-program
branch: codex/latency-observability-program
base: main
```

**Why this approach**: The program will touch multiple files and may span
multiple PRs. Worktrees keep the user's current local state stable, make branch
scope explicit, and allow separate follow-up worktrees for TemperPaw if needed.

### Sub-Decision 3: Measurement repair comes before performance changes

The first implementation phase must repair or add measurement needed to make
speed work trustworthy:

- Datadog profiling freshness, upload rate, upload errors, and CPU/allocation
  profile availability.
- DBM plan collection, database CPU/vacuum coverage, lock/wait visibility, pool
  acquire time, and transaction duration visibility.
- Distribution/percentile metrics for Cedar, dispatch ask, projection updates,
  WASM invocation, DB pool wait, and blob upload.
- Trace cardinality budgets using summary spans and exemplars for high-fanout
  projection work.
- Projection correctness observability: lag, last applied event sequence,
  projection version/hash, drift samples, shadow reads, and replay parity.

**Why this approach**: Optimizing before the profiler, DBM, percentiles, and
projection correctness signals are trustworthy risks moving latency around,
overfitting stale traces, or making projections faster but wrong.

### Sub-Decision 4: Performance work proceeds by evidence-ranked slices

After measurement repair, the program will implement performance slices in this
order unless new evidence changes the order:

1. Cedar/AuthZ CPU path: profile, remove avoidable cloning/conversion, add
   policy/context dimensions, and evaluate deny-safe cache/precompile options.
2. Projection write amplification: diff previous/new state, batch index
   updates, coalesce per-entity work, and make indexing schema/query-driven
   where possible.
3. DB transaction shape: remove idle-in-transaction patterns, separate pool
   wait from SQL execution, and replace event append `SELECT MAX(sequence_nr)`
   patterns where safe.
4. Workflow/integration executor: move long external work to durable,
   observable execution with idempotent verified callbacks when semantics allow.
5. Blob/data plane: stream or direct-upload large bytes while keeping Temper in
   charge of metadata, policy, hash/content verification, lifecycle, and
   verified callbacks.

**Why this approach**: This order attacks observed hot paths while preserving
Temper's architectural guarantees.

### Sub-Decision 5: Correctness is a speed gate

Any change that introduces or changes projections, caches, coalescing, async
execution, or direct data-plane movement must include correctness evidence:

- Projection changes require drift and replay parity proof.
- AuthZ caches require tenant, policy-set hash, principal, action, resource,
  and relevant context in the cache key, plus invalidation on policy/principal
  changes.
- Workflow and integration changes require idempotency keys and verified
  callback transitions.
- Blob/data-plane changes require content hash verification and no partial state
  transition after failed external writes.

**Why this approach**: Temper should become faster by making derived work
observable and bounded, not by weakening governance or accepting data drift.

### Sub-Decision 6: Completion requires live evidence, merged PRs, and deployment

This program is not complete when code lands locally. Completion requires:

- ADRs written for architectural changes.
- Local tests and relevant workspace checks pass.
- Live runs and live end-to-end tests pass.
- PRs are opened, reviewed, merged.
- The deployed system shows the intended observability and latency/correctness
  evidence.
- `docs/temper-latency-observability-report.html` records the final evidence.

**Why this approach**: The user's requirement is production speed and
observability, not a local patch.

## Rollout Plan

1. **Phase 0 (Program setup)** -- Create the worktree, living dashboard, and
   ADR-0081. Identify whether Temper MCP project-management tracking is
   available in the session. If not available, use the thread goal and the HTML
   dashboard until a Temper issue can be created.
2. **Phase 1 (Measurement repair)** -- Implement profiler freshness checks,
   DBM repair/coverage, distribution metrics, trace budgets, and projection
   correctness observability. Update the dashboard after each slice.
3. **Phase 2 (Low-risk hot-path performance)** -- Optimize Cedar/AuthZ,
   projection write amplification, and DB transaction shape based on Phase 1
   evidence.
4. **Phase 3 (Structural performance)** -- Add durable workflow/integration
   execution and blob/data-plane changes where evidence and ADRs justify them.
5. **Phase 4 (Verification and deployment)** -- Run repeatable local tests,
   live runs, live end-to-end tests, PR review/merge, deployment, and final
   Datadog verification. Record all evidence in the dashboard.

## Implementation Notes

- 2026-05-14, OBS-001 local repair: `crates/temper-server/src/profiling.rs`
  now emits profiler config, freshness, capture, duration, upload, and error
  metrics and supports an opt-in continuous CPU capture loop gated by
  `TEMPER_PROFILING_ENABLED`, `TEMPER_PROFILING_CONTINUOUS`, and
  `TEMPER_PROFILING_AUTO_UPLOAD`. `crates/temper-cli/src/serve/mod.rs` wires
  the loop at server startup. This is not considered complete until a PR is
  deployed and Datadog shows fresh live profiles.
- 2026-05-14, Datadog profiler target: current Datadog documentation points
  Rust/C/C++ services at the native `ddprof` profiler. The in-process pprof
  repair above should be treated as a useful fallback and operational
  freshness/control signal, while the production OBS-001 finish line must also
  evaluate `ddprof` deployment, OS permissions, symbols, and live profile
  freshness.
- 2026-05-14, OBS-002 DBM repair package:
  `scripts/datadog-postgres-dbm-setup.sql` and
  `docs/runbooks/datadog-postgres-dbm.md` capture the Datadog Postgres DBM
  setup needed to repair the observed `invalid_schema` plan collection
  failures. The package installs the `datadog` schema helper functions,
  `datadog.explain_statement(TEXT)`, role grants, `pg_stat_statements`, and a
  search path compatible with Temper's unqualified public-schema SQL. The
  explain helper also sets a synthetic `app.current_tenant` value so Temper RLS
  policies are explicit while plans are collected. This is not considered done
  until it is executed against staging/live Postgres and Datadog plan records
  for hot Temper query signatures no longer carry `plan.collection_errors`.
- 2026-05-14, OBS-002 app-side Postgres metrics:
  `crates/temper-store-postgres` now emits low-cardinality OpenTelemetry
  metrics for PostgreSQL pool acquire time, transaction `BEGIN` time,
  transaction `COMMIT` time, end-to-end transaction duration, transaction
  operation outcomes, projection indexed field counts, and oversized projection
  field skips. The first instrumented operations are `event_append`,
  `query_projection_upsert`, and `query_projection_remove`, because these sit
  on the event audit path and the observed projection write-amplification path.
  `crates/temper-cli/src/serve/mod.rs` initializes these metrics at startup.
  This is not considered complete until the metrics are deployed and visible in
  Datadog as p50/p95/p99 distributions by operation.
- 2026-05-14, OBS-003 projection correctness metrics:
  ADR-0082 now defines projection correctness observability as the safety gate
  before projection write-amplification work. The first local slice extends
  `crates/temper-server/src/query_projection_metrics.rs` and the projection
  update/backfill call sites with source-tagged update started/error/duration,
  queue wait, end-to-end duration, applied sequence, backfill coverage,
  backfill duration, and backfill replay-event metrics. The second local slice
  adds opt-in sampled catalog-fast-read shadow checks gated by
  `TEMPER_ODATA_CATALOG_SHADOW_READ_EVERY`, comparing projected status, fields,
  and sequence against authoritative actor state in the background. The third
  local slice adds `ServerState::verify_query_projection_replay_parity`, which
  rebuilds active entity state from the event journal, compares it with durable
  catalog rows, emits `temper_query_projection_replay_parity_*` metrics, and
  returns bounded drift examples without using entity IDs as metric tags. This
  parity verifier exposed and fixed a Turso query-plane correctness gap:
  `entity_catalog` now preserves the full projected `fields` JSON blob, while
  `entity_field_index` remains the scalar filter-pushdown index. This is not
  considered complete until the full signal set is visible in Datadog and the
  parity verifier is run against deployed data.
- 2026-05-14, OBS-004 Datadog dashboard and monitor config:
  the TemperPaw main-based worktree
  `/Users/seshendranalla/Development/temperpaw-worktrees/latency-observability-program`
  now updates `dd-dashboards/temperpaw-overview.json` and
  `dd-monitors/temperpaw-monitors.json` with widgets and alerts for projection
  queue wait, end-to-end projection duration, applied sequence, backfill
  coverage, shadow-check drift, shadow sequence gap, replay parity drift,
  replay parity duration, replay parity sequence gap, PostgreSQL pool acquire,
  PostgreSQL transaction duration, dispatch tail latency, WASM invocation
  duration, WASM host HTTP duration, blob I/O and transport wait, Monty REPL
  wait, context preparation duration, and session phase duration. The
  mislabeled request-latency monitor now uses
  `p95:temper_dispatch_ask_latency_ms` instead of an average. This is not
  considered complete until the Datadog configuration is deployed, widgets
  resolve against live `service:openpaw` data, monitor IDs/links are recorded,
  and any remaining Cedar/trace-budget No Data gaps are closed.
- 2026-05-14, OBS-005 trace budget and fanout summarization:
  ADR-0083 defines a bounded trace-volume policy for high-fanout workflows.
  The first local slice makes Temper's name-based OTEL sampler measurable via
  `temper_trace_sampler_decisions_total`,
  `temper_trace_sampler_configured_rules`, and
  `temper_trace_sampler_reduced_sample_rate_pct`; adds startup-tunable reduced
  sampling for WASM auxiliary spans and background dispatch/projection helper
  spans; and records reaction fanout summaries on `reaction.dispatch` spans.
  The TemperPaw Datadog config now includes a Trace Budget dashboard group and
  monitors for missing sampler metrics, unusual delegated span volume, and
  disabled background-dispatch trace budgeting. This is not considered complete
  until deployed high-fanout live runs prove routine traces no longer emit
  100k+ spans while slow/error traces retain useful root and summary context.
- 2026-05-14, release verification package:
  `docs/runbooks/latency-observability-release.md` defines the two-PR split,
  local verification commands, runtime flags, DBM repair, Datadog deployment
  commands, live proof matrix, e2e smoke requirements, rollback plan, and done
  criteria. This runbook is the operational bridge between local patches and
  the living dashboard evidence required before performance optimization work.

## Readiness Gates

- Measurement gate: profiler, DBM, percentile metrics, trace cardinality, and
  projection correctness signals are current and trustworthy enough to guide
  optimization.
- Correctness gate: projection/cache/async/data-plane changes have tests and
  runtime signals proving no drift, authorization bypass, duplicate side
  effects, or partial state transition.
- Performance gate: each optimized path has before/after evidence and an SLO
  target or trend recorded in the dashboard.
- Deployment gate: live end-to-end tests pass after merge and deploy.

## Consequences

### Positive

- The program has a clear finish line and evidence trail.
- Speed work is guided by current profiler/DBM/metric data rather than stale or
  average-only signals.
- Projection speed and projection correctness are treated together.
- Worktree discipline reduces accidental branch or checkout disruption.

### Negative

- Measurement-first sequencing delays some direct performance fixes.
- The living dashboard adds documentation maintenance during implementation.
- Some structural changes will require additional ADRs and PRs.

### Risks

- Production code may continue to differ from the local checkout. Mitigation:
  use Datadog as the current production truth and verify branch/revision before
  optimizing any code path.
- Datadog profiling or DBM may require production configuration changes outside
  this repository. Mitigation: record those as explicit deployment/config tasks
  and verify with live data.
- Projection correctness instrumentation may expose existing drift. Mitigation:
  treat this as a success of observability and gate optimization until the drift
  is understood.

### DST Compliance

This ADR itself changes documentation only. Follow-up code changes touching
simulation-visible crates (`temper-runtime`, `temper-jit`, `temper-server`)
must preserve deterministic simulation constraints:

- Keep non-deterministic I/O outside simulation-visible paths or behind traits.
- Use deterministic time/UUID helpers in simulation-visible code.
- Do not add unbounded concurrency or non-deterministic collection iteration.
- Run DST/code-review gates before commits that affect simulation-visible code.

## Non-Goals

- Replacing the verified entity actor architecture.
- Weakening Cedar default-deny behavior.
- Removing durable event audit.
- Accepting projection drift as a performance tradeoff.
- Declaring completion based only on local tests or a dashboard change.

## Alternatives Considered

1. **Optimize hot paths immediately** -- Rejected because profiler, DBM,
   percentile metrics, and projection correctness signals are incomplete.
2. **Treat observability as separate from performance** -- Rejected because
   the observed bottlenecks include observability cardinality and because
   projection correctness must be measured while optimizing.
3. **One large performance rewrite** -- Rejected because the work crosses
   governance, projection, persistence, integration, and blob paths. Small PRs
   with live evidence are safer.
4. **Work in the existing checkout** -- Rejected by user requirement and by
   local repo layout. Main-based worktrees make branch scope explicit.

## Rollback Policy

Documentation-only changes can be reverted by removing this ADR and the
dashboard updates. Follow-up implementation slices must define their own
rollback policies in their ADRs or PR descriptions, including feature flags,
config rollbacks, or disabling new projection/cache/executor paths when
appropriate.
