# ADR-0083: Trace Budget and Fanout Summarization

- Status: Proposed
- Date: 2026-05-14
- Deciders: Temper core maintainers
- Related:
  - ADR-0052: Instrumentation as policy
  - ADR-0057: Canonical dispatch traces and selective wide-event projection
  - ADR-0081: Latency and Observability Acceleration Program
  - `crates/temper-observe/src/otel.rs`
  - `crates/temper-server/src/trigger/dispatcher.rs`
  - `docs/temper-latency-observability-report.html`

## Context

Live Datadog traces for the latency program showed that Temper can generate
very large traces during high-fanout work. One production trace contained
roughly 166k spans, including roughly 104k field-index insert spans. Those
traces prove that observability is reaching the hot path, but they are too
large to be a routine diagnostic artifact: they make trace navigation harder,
increase ingest volume, and can make observability part of the latency problem.

Temper already has useful canonical spans for dispatch, reaction dispatch,
projection maintenance, WASM invocation, and HTTP/OData routes. It also has a
name-based OTEL sampler that drops known-noisy spans and reduced-samples some
WASM helper prefixes. The gap is governance around that behavior:

- Sampler decisions are not themselves observable.
- Reduced-sampling rates are fixed in code rather than operationally tunable.
- High-fanout reaction spans do not summarize matched/fired/skipped/error
  counts, so operators must inspect many child spans to understand fanout.
- The latency program needs a clear policy for keeping slow/error traces useful
  while preventing routine 100k-span traces.

## Decision

### Sub-Decision 1: Make trace sampling policy explicit and measurable

Temper will keep the existing source-owned sampling policy, but make it
observable with low-cardinality metrics:

- `temper_trace_sampler_decisions_total{rule,decision}` counts drop, reduced
  sample, reduced drop, and delegated decisions.
- `temper_trace_sampler_configured_rules{rule_kind}` records how many exact
  drop and reduced-prefix rules are active.
- `temper_trace_sampler_reduced_sample_rate_pct{rule}` records each reduced
  prefix rule's effective keep rate.

Metric labels must stay low cardinality. They may include rule names such as
`drop_exact`, `wasm_auxiliary`, and `dispatch_background`, but not entity IDs,
trace IDs, span IDs, URLs, or arbitrary span names.

**Why this approach**: The sampler becomes auditable in Datadog without
emitting a metric series per dynamic resource. Operators can see whether trace
volume reduction is active before relying on sampled traces for diagnosis.

### Sub-Decision 2: Keep critical request roots and reduce noisy children

Temper will continue to keep canonical request/dispatch roots through the
parent-based sampler while reducing helper spans that are already covered by
metrics or higher-level summary spans:

- Exact drop remains reserved for span names with no diagnostic value, such as
  `turso.configured_connection` and high-volume WASI syscalls.
- Reduced-prefix sampling covers known noisy helper spans, including WASM
  auxiliary host spans and background dispatch fanout spans.
- The default keep rate is intentionally conservative and can be overridden at
  startup through environment variables, without code changes.

**Why this approach**: Slow/error root traces remain available, while the
debug-only children that dominate trace size can be sampled down. Metrics carry
the percentile and count view; traces carry representative causal context.

### Sub-Decision 3: Summarize fanout on parent spans

High-fanout dispatch surfaces should record bounded summary fields on parent
spans instead of relying only on many child spans. Reaction dispatch records:

- rule count,
- fired count,
- guard-skip count,
- target-resolution failures,
- authorization denials,
- dispatch errors,
- successful target dispatches,
- total result count.

**Why this approach**: A sampled trace can explain the shape of fanout even
when some children are reduced-sampled or dropped. It also helps operators
distinguish "many rules matched" from "one slow child" from "authz denied most
of the cascade."

## Rollout Plan

1. **Phase 0 (Immediate)** -- Add the sampler metrics/config helpers, extend
   reduced-prefix rules, add reaction fanout span fields, and cover them with
   focused tests.
2. **Phase 1 (Datadog config)** -- Add dashboard widgets and monitors for
   sampler decision rates, configured rules, and abnormal reduced-drop volume.
3. **Phase 2 (Live verification)** -- Deploy to staging/live, run a high-fanout
   workflow, and verify that trace span counts are bounded while slow/error
   traces retain root, dispatch, projection, WASM, and fanout summary context.

## Readiness Gates

- Sampler unit tests pass and verify deterministic trace-id sampling.
- Reaction fanout summaries are present in local tracing tests or manual span
  inspection.
- Datadog shows sampler decision/config metrics after deployment.
- No routine high-fanout workflow emits 100k-span traces after the policy is
  active, unless operators temporarily raise the sampling budget.

## Consequences

### Positive

- Trace-volume policy becomes visible and tunable.
- Operators can debug fanout from parent-span summaries.
- High-fanout workflows stop using raw span count as their only explanation.

### Negative

- Some helper child spans will be absent from routine traces.
- Sampler metrics add a small amount of per-span overhead.

### Risks

- A reduced-sampling rule could hide a child span needed during an incident.
  Mitigation: startup env overrides can temporarily raise a rule's keep rate.
- Metrics emitted from sampler decisions could become noisy if rule labels are
  expanded carelessly. Mitigation: keep rule labels source-owned and bounded.

### DST Compliance

This change touches `temper-server`, a simulation-visible crate, only for
observability fields on production tracing spans. It does not influence actor
state, transition legality, event journals, tenant routing, or deterministic
simulation. Any startup environment reads are in `temper-observe` OTEL setup,
outside the simulation core.

## Non-Goals

- Replacing Datadog APM sampling controls.
- Adding per-entity or per-span-name metric labels.
- Changing entity transition semantics, projection correctness, or event
  persistence.

## Alternatives Considered

1. **Drop all child spans under high-fanout traces** -- Rejected because it
   removes too much causal context during incidents.
2. **Keep all spans and rely on Datadog ingestion controls** -- Rejected
   because Temper already knows which spans are helper noise and can preserve
   mission-critical roots before telemetry leaves the process.
3. **Emit metrics for every raw span name** -- Rejected because it creates a
   new cardinality risk while trying to solve a cardinality problem.

## Rollback Policy

Disable reduced sampling by setting the relevant sample-rate environment
variables to `100`, or revert the sampler rule additions. Fanout summary fields
are additive and can remain even if reduced sampling is disabled.
