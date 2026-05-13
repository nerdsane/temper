# ADR-0084: Long-Lived Workflow Root Spans

- Status: Accepted
- Date: 2026-05-12
- Deciders: Temper core maintainers
- Related:
  - ADR-0052: Instrumentation as policy
  - ADR-0057: Canonical Dispatch Traces and Selective Wide-Event Projection
  - ADR-0059: Workflow Trace Context Propagation
  - ADR-0083: WASM Host Span Hints Must Be Datadog-Visible
  - `crates/temper-server/src/state/dispatch/actions.rs`
  - `crates/temper-server/src/workflow_tracing.rs`

## Context

ADR-0059 made workflow trace context a dispatch primitive. Live TemperPaw
Datadog verification then exposed a trace-shape gap: asynchronous agent-session
work was correlated, but it could still appear beneath a short inbound HTTP
request span. That made the flamegraph misleading because the request root ended
while child dispatch, WASM, database, and LLM work continued under the same
logical session.

The telemetry was technically joinable by `workflow.run_id`, `entity_id`, and
logs, but the primary APM shape was not yet good enough for humans or agents
trying to understand a session chronologically.

## Decision

Temper keeps an in-memory workflow root span open for active workflow runs and
parents workflow dispatch spans to it when the dispatch belongs to an explicit
workflow context or a known agent-cycle root entity.

### Sub-Decision 1: Workflow roots are process-local telemetry objects

`WorkflowSpanRegistry` stores open `temper.workflow` spans keyed by
`workflow.run_id`. The root span uses `parent: None`, so it is not a child of
the inbound HTTP request that happened to trigger the first action.

**Why this approach**: the root span is observability only. Entity state remains
the durable source of truth, and workflow execution is still represented by
entity transitions and WASM integrations.

If no OpenTelemetry span context is active, the registry does not retain a root
span. This keeps deterministic simulation and local tests from accumulating
no-op tracing spans when no exporter/subscriber can ever observe them.

### Sub-Decision 2: Dispatch adopts the workflow root before context enrichment

`dispatch_tenant_action_core` decides whether a dispatch belongs under a
workflow root, sets the current span parent to that root, then refreshes
`AgentContext` trace ids from the current span. Follow-on WASM, host function,
database, and LLM telemetry inherit the workflow trace instead of the short
request trace.

**Why this approach**: core dispatch is still the one shared path for HTTP
actions, reactions, callbacks, adapters, timers, and spawned entities. Keeping
the change there avoids product-local tracing code.

### Sub-Decision 3: Root spans close only on terminal root transitions

A workflow root closes only when the root entity reaches a terminal status:
`Completed`, `Failed`, `Cancelled`, or `Terminated`. Terminal child entities do
not close the root. Cleanup is delayed briefly after the terminal dispatch so
post-dispatch telemetry can attach before the root span is exported.

**Why this approach**: agent-session traces should cover the whole logical
session, including final effects and emitted logs, without forcing every small
child entity to mint its own top-level trace.

Dispatch schedules terminal cleanup only when it actually adopted a valid
workflow root context. This avoids observability-only cleanup tasks in
no-exporter test runs and non-traced dispatch paths.

## Rollout Plan

1. **Phase 0 (Immediate)** - Ship the workflow root span registry and dispatch
   parent selection in Temper.
2. **Phase 1 (Immediate)** - Update TemperPaw to consume the Temper revision and
   run a real agent-session proof.
3. **Phase 2 (Immediate)** - Verify Datadog shows the session under a long-lived
   `temper.workflow` / `{RootEntity}.workflow` root rather than a short HTTP
   root.

## Readiness Gates

- Unit tests prove workflow roots are not children of short HTTP request spans.
- Unit tests prove workflow roots are limited to explicit workflow context or
  known agent-cycle entities.
- Unit tests prove terminal child entities do not close the root span.
- TemperPaw live proof records a session id, trace id, Datadog queries, and the
  observed hierarchy.

## Consequences

### Positive

- Datadog APM can present a chronological, expandable agent-session trace rooted
  in the logical workflow.
- WASM host spans, database spans, logs, and LLM telemetry inherit the same trace
  context after dispatch enrichment.
- Product apps do not need to add their own session-root tracing layer.

### Negative

- Active workflow roots are process-local and held in memory until terminal
  state or process exit.
- A process crash can end a root span early; entity state remains correct, but
  the exported trace may be partial.

### Risks

- Long-running workflows can keep spans open for a long time. Mitigation: the
  registry is keyed by `workflow.run_id` and closes on terminal root status.
- New agent-cycle root entity types could be omitted from the built-in allowlist.
  Mitigation: explicit workflow context always opts in, and the allowlist is
  covered by tests.

### DST Compliance

- Determinism is unchanged. The registry stores only telemetry spans and does
  not affect entity state, authorization, persistence, or transition outcomes.
- The delayed cleanup task is marked `determinism-ok` because it only controls
  when an observability span is dropped.

## Non-Goals

- Durable span storage or cross-process span recovery.
- New workflow orchestration outside entity transitions.
- Inside-WASM guest-created APM spans; ADR-0083 still owns that limitation and
  follow-up path.

## Alternatives Considered

1. **Use the inbound HTTP request as the root** - Rejected because agent-session
   work continues after the request returns, producing misleading flamegraphs.
2. **Rely only on `workflow.run_id` queries and logs** - Rejected because users
   need the primary trace hierarchy itself to be useful.
3. **Create a product-local TemperPaw session span** - Rejected because the
   dispatch trace-shape problem is platform-level and affects any Temper-native
   app with asynchronous workflows.

## Rollback Policy

Remove `WorkflowSpanRegistry`, return dispatch parent selection to the previous
remote-parent-only behavior, and remove the workflow root tests. No persistent
state migration is required.
