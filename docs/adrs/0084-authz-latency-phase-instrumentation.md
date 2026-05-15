# ADR-0084: AuthZ Latency Phase Instrumentation

- Status: Proposed
- Date: 2026-05-14
- Deciders: Temper core maintainers
- Related:
  - ADR-0046: Cedar governance without bypass
  - ADR-0052: Instrumentation as policy
  - ADR-0081: Latency and Observability Acceleration Program
  - ADR-0083: Trace Budget and Fanout Summarization
  - `crates/temper-authz/src/engine/mod.rs`
  - `crates/temper-authz/src/metrics.rs`
  - `docs/temper-latency-observability-report.html`

## Context

Live Datadog data confirms that Cedar authorization is already observable with
`temper_cedar_evaluations_total` and `temper_cedar_evaluation_duration`, but
the signal is not sufficient for the latency program:

- `temper_cedar_evaluation_duration` is a Datadog distribution metric with
  percentile aggregations disabled, so p95/p99 dashboard queries and the
  existing p99 monitor can produce no data.
- `temper_cedar_evaluations_total` is emitted from two instrumentation scopes
  (`temper-authz` and `temper.authz`), which can double-count traffic when a
  dashboard does not explicitly filter the scope.
- The duration metric covers the whole request-construction and evaluation
  path, so it cannot distinguish Cedar policy evaluation from string parsing,
  JSON-to-Cedar conversion, context building, entity store creation, or request
  construction.
- Early errors such as invalid principals/actions/resources can return before
  the decision-tagged metric is recorded.

This matters because AuthZ is mission-critical: Temper must keep Cedar
default-deny semantics, tenant policy isolation, auditable decisions, and the
removal of silent System bypasses from ADR-0046. The latency program can only
optimize this path safely if it can see which phase is slow before changing
policy or cache behavior.

## Decision

### Sub-Decision 1: Use one canonical Cedar evaluation counter

Temper will emit `temper_cedar_evaluations_total` only through
`crates/temper-authz/src/metrics.rs` using the `temper.authz`
instrumentation scope. The counter will include a bounded `decision` label:
`allow`, `deny`, or `error`.

**Why this approach**: Existing monitors and dashboards keep their metric name,
but the unlabelled duplicate scope stops inflating totals. Error returns become
visible without inventing a second traffic metric.

### Sub-Decision 2: Preserve the existing seconds metric and add an ms metric

Temper will keep recording `temper_cedar_evaluation_duration` in seconds for
compatibility, and add `temper_cedar_evaluation_duration_ms` with unit `ms`.
Both metrics carry the same `decision` label.

Datadog percentile aggregations must be enabled for both distribution metrics
as part of the release checklist before p95/p99 targets are treated as live.

**Why this approach**: The old metric remains useful for existing assets, while
new work can standardize on the same millisecond convention used by dispatch,
projection, Postgres, WASM, and actor latency metrics.

### Sub-Decision 3: Decompose AuthZ wall time by phase

Temper will add `temper_cedar_evaluation_phase_duration_ms{phase,outcome}` for
bounded AuthZ phases:

- `principal_uid`
- `action_uid`
- `resource_uid`
- `context_attrs`
- `principal_attrs`
- `resource_attrs`
- `entities`
- `request`
- `authorizer`

`outcome` is bounded to `ok` or `error` for phase metrics. It does not include
tenant IDs, entity IDs, policy IDs, action names, resource names, trace IDs, or
arbitrary error text.

**Why this approach**: Operators can determine whether a slow call is expensive
because Cedar evaluation is expensive, or because Temper is doing too much
per-request object construction before Cedar runs.

### Sub-Decision 4: Record request-shape histograms, not high-cardinality tags

Temper will add `temper_cedar_request_attribute_count{source}` where `source`
is `context`, `principal`, or `resource`. This gives Datadog enough context to
correlate high latency with large authorization envelopes without emitting
dynamic attribute names or values.

**Why this approach**: Shape signals help explain latency and correctness risk
while preserving tenant privacy and Datadog cardinality.

## Rollout Plan

1. **Phase 0 (Immediate)** -- Add canonical metrics, phase timers, request
   shape histograms, focused tests, dashboard widgets, and release preflight
   checks.
2. **Phase 1 (Datadog metric config)** -- Enable percentile aggregations for
   `temper_cedar_evaluation_duration` and
   `temper_cedar_evaluation_duration_ms`, then verify p95/p99 queries resolve.
3. **Phase 2 (Evidence capture)** -- Run a live AuthZ-heavy workflow, compare
   total duration, phase duration, request shape, and profiler stacks.
4. **Phase 3 (Optimization decision)** -- Only after phase evidence exists,
   choose among request construction cleanup, Cedar entity reuse, compiled
   policy metadata, or deny-safe decision caching.

## Readiness Gates

- `cargo test -p temper-authz` passes.
- Datadog shows `temper_cedar_evaluation_duration_ms`,
  `temper_cedar_evaluation_phase_duration_ms`, and
  `temper_cedar_request_attribute_count`.
- Datadog percentile aggregations are enabled for Cedar duration distributions.
- The dashboard can show AuthZ p95/p99 and phase breakdowns without No Data.
- Any later cache optimization proves policy-hash, tenant, principal, action,
  resource, and context invalidation before it can be used for correctness
  decisions.

## Consequences

### Positive

- AuthZ latency becomes explainable before optimization.
- Existing Cedar traffic monitors stop double-counting unlabelled scope data.
- Tail-latency monitors can be made real instead of aspirational.
- Request-shape histograms expose large contexts without leaking sensitive
  values or creating high-cardinality tags.

### Negative

- Each authorization call records more metrics.
- Dashboards must be adjusted to use the canonical scope and new ms metric.
- Percentile aggregation enablement remains an external Datadog metric
  configuration step, not something Rust code alone can guarantee.

### Risks

- The phase metrics could add measurable overhead on a very hot path.
  Mitigation: labels are bounded, timer calls are local, and the first
  rollout measures this overhead before deeper optimization.
- Removing the duplicate counter scope can appear to reduce traffic volume.
  Mitigation: dashboards and release notes call out that this is a counting
  correction, not a traffic drop.
- Future decision caching could accidentally bypass Cedar semantics.
  Mitigation: this ADR intentionally instruments first; cache design requires
  a separate ADR and correctness proof.

### DST Compliance

This change is limited to `temper-authz`, which is outside the
simulation-visible crate set. It does not alter actor scheduling, transition
tables, event journals, tenant routing, or deterministic simulation. Existing
AuthZ code already uses wall-clock `Instant` for telemetry timing, and this
ADR keeps that usage inside the AuthZ observability layer.

## Non-Goals

- Adding an AuthZ decision cache.
- Changing Cedar policy semantics, policy loading, or default-deny behavior.
- Adding policy IDs, tenant IDs, entity IDs, or arbitrary resource attributes
  as metric tags.
- Replacing Datadog metric configuration or dashboard deployment automation.

## Alternatives Considered

1. **Optimize before adding phase metrics** -- Rejected because the current
   signal cannot distinguish Cedar cost from Temper request-construction cost.
2. **Use spans only** -- Rejected because spans are subject to sampling and
   high-fanout trace budgets; metrics are needed for p95/p99 and regression
   monitoring.
3. **Emit one metric per resource or policy** -- Rejected because it would
   create cardinality and privacy risk.
4. **Replace the existing duration metric** -- Rejected because existing
   dashboards and monitors reference it. Additive metrics are safer during the
   measurement-repair phase.

## Rollback Policy

Remove the new phase/request-shape metric calls and dashboard widgets. Keep the
canonical counter path unless it causes an unexpected operational issue; if a
rollback must restore the old scope, dashboards must explicitly filter
`instrumentation_scope` to avoid double-counting.
