# ADR-0059: Workflow Trace Context Propagation

- Status: Accepted
- Date: 2026-04-24
- Deciders: Temper core maintainers
- Related:
  - ADR-0052: Instrumentation as policy
  - ADR-0057: Canonical Dispatch Traces and Selective Wide-Event Projection
  - openpaw ADR-0037: End-to-end tracing and traceparent propagation
  - `crates/temper-server/src/request_context.rs`
  - `crates/temper-server/src/state/dispatch/`
  - `crates/temper-wasm/src/host_trait.rs`

## Context

Temper already propagates W3C `traceparent` across HTTP requests and injects
that context into WASM outbound HTTP calls. That was enough for short request
traces, but not enough for long-running entity workflows.

The missing piece was the dispatch context itself. Background WASM, adapter
callbacks, reactions, scheduled actions, and spawned child entities could cross
`tokio::spawn` or service-principal boundaries that replaced the caller context.
When that happened, Datadog saw valid local spans but not one connected
workflow flamegraph. Product workflows such as OpenPaw curation jobs need the
same answer as every other Temper app: the whole workflow must be explainable
from entity transitions and the platform trace context, not from bespoke
orchestration code.

## Decision

Temper makes workflow tracing a dispatch-level primitive.

### Sub-Decision 1: `AgentContext` carries workflow trace metadata

`AgentContext` now carries:

- `trace_id`
- `parent_span_id`
- `workflow_root_entity_type`
- `workflow_root_entity_id`
- `workflow_run_id`

The workflow fields are observability metadata. They are not entity business
state and they are not an orchestration layer.

**Why this approach**: dispatch is the one path shared by HTTP triggers,
reactions, WASM callbacks, adapters, timers, and spawned entities. Carrying the
context there makes the feature app-neutral.

### Sub-Decision 2: core dispatch is the workflow root boundary

`dispatch_tenant_action_core` adopts any incoming remote parent context, then
enriches the caller context with the current OpenTelemetry span ids. If a
workflow root is not already present, the dispatched entity becomes the root and
`workflow_run_id` defaults to `{entity_type}:{entity_id}`.

**Why this approach**: the first governed entity action is Temper's canonical
workflow boundary. Follow-on work preserves that root instead of minting a new
trace for each callback.

### Sub-Decision 3: service callbacks inherit observability, not authority

Platform service callbacks use `AgentContext::for_service_inheriting` when they
need a service principal but still belong to the caller's workflow trace.
Authority comes from the service identity; observability comes from the parent
workflow context.

**Why this approach**: service callbacks should remain auditable as service
actions without severing the flamegraph.

### Sub-Decision 4: async boundaries must preserve workflow context

Fire-and-forget WASM integrations, adapter integrations, scheduled actions, and
spawned child entity work attach workflow attributes to their background spans
and pass the enriched `AgentContext` into any follow-on dispatch.

**Why this approach**: `tokio::spawn` is permitted only for platform
side-effects that are outside deterministic simulation. When used, it must not
drop the workflow's trace identity.

## Rollout Plan

1. **Phase 0 (Immediate)** — Ship dispatch workflow context propagation in
   Temper and update OpenPaw to consume the Temper main revision.
2. **Phase 1 (Immediate)** — Verify a local OpenPaw curation-style workflow can
   complete with connected dispatch, WASM, callback, and publish spans.
3. **Phase 2 (Follow-up)** — Add saved Datadog views that query
   `workflow.run_id` and `workflow.root_entity_*` attributes.

## Readiness Gates

- Unit tests prove service contexts preserve workflow trace metadata.
- Unit tests prove a dispatch root records the active OpenTelemetry trace and
  span ids.
- A local E2E run exercises a real workflow and records proof output.
- OpenPaw consumes the merged Temper revision rather than adding product-local
  tracing code.

## Consequences

### Positive

- Datadog can show one trace for an entire logical workflow, even when the work
  spans callbacks and background tasks.
- Operators can query by `workflow.run_id` instead of reconstructing work from
  disconnected request spans.
- Product apps do not need custom tracing orchestration.

### Negative

- `AgentContext` now contains observability fields in addition to identity and
  idempotency fields, so callers must be careful not to treat all fields as
  authority.
- Some background spans become more verbose because they carry workflow
  attributes explicitly.

### Risks

- A caller could accidentally copy idempotency semantics while only intending to
  copy observability. Mitigation: `for_service_inheriting` copies observability
  fields only; callers that intentionally preserve idempotency must do so
  explicitly.
- A future dispatch path could bypass `dispatch_tenant_action_core`. Mitigation:
  dispatch wrappers should converge on `DispatchCommand` and core dispatch.

### DST Compliance

- Determinism is unchanged. Trace ids, span ids, and workflow attributes are
  observability metadata only.
- Existing `tokio::spawn` boundaries remain side-effect execution paths outside
  the simulation core. The change adds span instrumentation and context
  propagation, not simulation-visible behavior.

## Non-Goals

- Keeping a single in-memory root span open for the full wall-clock duration of
  a workflow.
- Introducing workflow orchestration outside entity transitions.
- Product-specific Datadog query conventions in Temper.

## Alternatives Considered

1. **Open a durable root span per workflow** — Rejected. Spans are process-local
   telemetry objects and are not durable workflow state.
2. **Add product-local tracing in OpenPaw** — Rejected. The same trace continuity
   problem exists for every Temper workflow.
3. **Rely only on Datadog log correlation** — Rejected. Logs can aid search, but
   they do not produce the single flamegraph operators asked for.

## Rollback Policy

Remove the workflow fields from `AgentContext`, stop enriching contexts in core
dispatch, and return service callbacks to plain `for_service` contexts. No
persistent state migration is required.
