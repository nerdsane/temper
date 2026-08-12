# ADR-0101: Workflow Root Drain Attribution

- Status: Accepted
- Date: 2026-05-18
- Deciders: Temper core maintainers
- Related:
  - ADR-0059: Workflow Trace Context Propagation
  - ADR-0176: Long-Lived Workflow Root Spans
  - ADR-0098: Background WASM Trace Retention
  - ADR-0100: WASM Invocation Phase Observability
  - `crates/temper-server/src/workflow_tracing.rs`

## Context

ADR-0176 keeps workflow root spans open across asynchronous entity actions. To
avoid dropping final post-dispatch telemetry, the root span currently remains
open for a fixed two-second grace period after the root entity reaches a
terminal state.

Live TemperPaw latency work then exposed a measurement hazard: Datadog correctly
shows `Session.workflow` roots around seconds, but part of that duration is an
intentional observability drain, not product work. The child spans still expose
real latency in Session integrations and OData host calls, but the root duration
can mislead humans and agents into chasing the wrong target.

## Decision

Temper will make workflow-root drain time explicit in telemetry.

### Sub-Decision 1: Keep the drain, label it

The two-second grace remains in place so final dispatch, WASM, database, log,
and trajectory spans can attach before the root span closes. When terminal
cleanup is scheduled for the root entity, the root span records scalar
attributes for the terminal status and configured drain grace.

**Why this approach**: the drain exists for trace completeness. Removing it
without a broader trace-retention design could regress the Datadog hierarchy we
added to understand asynchronous workflows.

### Sub-Decision 2: Emit a child drain span

Temper creates a `temper.workflow.drain_grace` child span parented to the
workflow root and keeps that child open during the fixed grace sleep.

**Why this approach**: Datadog flamegraphs then show the intentional tail as a
named child span instead of hiding it inside the root duration. Root duration can
still represent "logical trace lifetime", while product-latency analysis can
subtract or filter the explicit drain span.

### Sub-Decision 3: Do not change workflow semantics

This change does not alter entity state, transition timing, authorization,
persistence, projection, WASM execution, OTS trajectory emission, or dispatch
ordering. It is telemetry-only.

**Why this approach**: PERF-021 needs clean measurement before the next product
latency slice. The actual speed work remains in Session integration/OData shape,
OTS trajectory cost where trace-proven, and projection correctness proof.

## Rollout Plan

1. **Phase 0 (Immediate)** - Add root-span drain attributes and a named drain
   child span in `temper-server`.
2. **Phase 1 (Immediate)** - Run focused `temper-server` workflow tracing tests
   and formatting.
3. **Phase 2 (Follow-up)** - Roll the Temper revision into TemperPaw, deploy,
   and confirm fixed-version Datadog traces show `temper.workflow.drain_grace`
   under Session roots.

## Readiness Gates

- Unit tests cover the configured drain grace.
- Unit tests prove drain attribution is only scheduled for terminal root entity
  status, not terminal child status.
- Existing workflow root span tests continue passing.
- TemperPaw live proof records a Datadog Session trace where root duration and
  explicit drain child span can be read separately.

## Consequences

### Positive

- Datadog traces explain why workflow roots include a two-second tail.
- Latency slicing can distinguish product work from observability grace.
- The trace-retention behavior from ADR-0176 remains intact.

### Negative

- Each terminal workflow root gets one extra telemetry span.
- Aggregate root-span duration still includes the drain unless dashboards
  filter or subtract the explicit child span.

### Risks

- The new child span could be mistaken for product work if dashboards do not
  label it clearly. Mitigation: use the explicit `temper.workflow.drain_grace`
  name and `workflow.drain_reason=post_dispatch_telemetry`.
- If terminal cleanup runs without a valid OpenTelemetry context, no child span
  is emitted. This matches the existing no-exporter behavior.

### DST Compliance

- Determinism is unchanged. The added span and scalar attributes are
  observability-only and do not affect entity state or transition results.
- The existing delayed cleanup task remains annotated `determinism-ok`; this ADR
  keeps the same simulated-state boundary.

## Non-Goals

- Shortening the workflow-root drain.
- Replacing workflow-root span lifetime with durable cross-process tracing.
- Changing TemperPaw Session specs, OTS trajectory semantics, or projection
  correctness behavior.
- Claiming a product-latency improvement from this measurement correction.

## Alternatives Considered

1. **Remove the drain** - Rejected because final post-dispatch telemetry could
   detach from the workflow root, undoing the trace-shape gains of ADR-0176.
2. **Only document the caveat in the report** - Rejected because future Datadog
   users and agents would still see misleading root spans unless the trace
   itself explains the drain.
3. **Rename the root span** - Rejected because the root still represents the
   logical workflow trace; the issue is an unattributed tail, not the root name.

## Rollback Policy

Remove the drain attributes and `temper.workflow.drain_grace` child span. The
workflow root span registry and delayed cleanup behavior can remain unchanged.
