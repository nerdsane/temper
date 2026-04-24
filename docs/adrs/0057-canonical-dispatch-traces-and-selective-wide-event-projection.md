# ADR-0057: Canonical Dispatch Traces and Selective Wide-Event Projection

- Status: Proposed
- Date: 2026-04-23
- Deciders: Temper core maintainers
- Related:
  - ADR-0052: Instrumentation as policy
  - ADR-0054: Log standard
  - openpaw ADR-0037: End-to-end tracing and traceparent propagation
  - `crates/temper-server/src/state/dispatch/wasm.rs`
  - `crates/temper-observe/src/wide_event.rs`
  - `crates/temper-wasm/src/host_trait.rs`

## Context

The April 23 quality-review investigation showed that Temper was exporting two different APM stories for the same work:

1. The real dispatch/integration/tool tree that operators need for end-to-end debugging.
2. Extra wide-event shadow spans for `LlmCall` and `WasmInvocation`.

That duplication caused two practical problems:

- The Datadog flamegraph for a session was harder to read because LLM and WASM work appeared twice under different span families.
- The shadow LLM spans had to be emitted off-trace to avoid suppressing the real Datadog LLM Observability record, which made the observability model harder to reason about.

At the same time, the dispatch path was still paying avoidable transport cost for internal TemperFS `File/$value` loopback calls made from WASM guests while preparing and applying session context.

We need one canonical APM trace tree for runtime work, while preserving the metrics/log/event benefits of wide events and reducing hot-path loopback overhead.

## Decision

Temper adopts the dispatch/integration trace tree as the single canonical contextual view for runtime debugging, and narrows wide-event span projection accordingly.

### Sub-Decision 1: LLM and WASM runtime spans live on the dispatch trace

`dispatch_single_integration` continues the active trace and creates the content-bearing LLM child span in-place instead of minting a detached root trace. This keeps:

- `temper.action`
- `wasm:{module}`
- tool spans
- provider spans / `gen_ai.*` attributes

inside one connected tree for a session attempt.

**Why this approach**: the dispatch path already knows the workflow context, entity identity, and active parent span. Reusing that tree is simpler and more correct than reconstructing a second trace from side-channel events.

### Sub-Decision 2: Wide events stop projecting shadow spans for `LlmCall` and `WasmInvocation`

`wide_event::emit_span` no longer emits contextual spans for:

- `EventKind::LlmCall`
- `EventKind::WasmInvocation`

Wide events still emit their metrics and retain their structured attributes. Transition, authorization, invariant, and tool-call wide events continue projecting spans because they are still the canonical source for those views today.

**Why this approach**: LLM and WASM runtime work already has first-class spans on the canonical trace. Removing only those two shadow projections eliminates duplication without regressing the other contextual views that still rely on wide events.

### Sub-Decision 3: Internal TemperFS `File/$value` calls may short-circuit in-process

The WASM host may intercept internal UTF-8 `GET/PUT /tdata/Files('{id}')/$value` calls and satisfy them in-process instead of routing through the full loopback HTTP stack.

This optimization is limited to internal loopback calls with the same semantics as the HTTP path. External HTTP traffic still follows the regular transport path.

**Why this approach**: session preparation and response application frequently read and rewrite artifact files. Those calls are infrastructure-local, semantically identical, and expensive to bounce through the full HTTP stack every turn.

## Rollout Plan

1. **Phase 0 (Immediate)** — Ship canonical LLM/WASM tracing and selective wide-event suppression.
2. **Phase 1 (Immediate)** — Ship the loopback `File/$value` fast path and validate with local session E2E.
3. **Phase 2 (Follow-up)** — Add explicit metrics/spans for the fast path so its wins remain visible after the HTTP spans disappear.

## Readiness Gates

- A fresh session attempt shows one connected APM trace from dispatch through LLM/tool work.
- Datadog LLM Observability still records the real content-bearing LLM span.
- `wide_event` metrics for `LlmCall` and `WasmInvocation` remain emitted.
- Local live session E2E completes with built WASM artifacts and the patched Temper server.

## Consequences

### Positive

- The session flamegraph becomes the single source of truth for runtime debugging.
- LLM telemetry is cleaner: one real LLM span instead of one real span plus one shadow span.
- Internal artifact reads/writes become materially cheaper in the common session hot path.

### Negative

- Operators can no longer rely on wide-event APM spans for `LlmCall` and `WasmInvocation`; they must use the canonical dispatch tree.
- The loopback fast path needs its own observability follow-up so it does not silently become invisible.

### Risks

- If any code path still depends on the shadow spans instead of the canonical trace, dashboards or saved searches may briefly look sparse until updated.
- If the fast path diverges semantically from the HTTP path, file read/write behavior could split. Mitigation: keep the interception limited to the same internal `File/$value` operations and preserve the HTTP fallback.

### DST Compliance

- Determinism is unchanged. The tracing projection policy only changes observability output.
- The `File/$value` optimization preserves the same logical read/write result as the prior HTTP loopback path.

## Non-Goals

- Removing all wide-event contextual spans.
- Replacing wide-event metrics with trace-derived metrics.
- General in-process short-circuiting for arbitrary OData routes.

## Alternatives Considered

1. **Keep the shadow spans and live with duplication** — Rejected. It keeps the primary debugging surface noisy and preserves a tracing model that operators already found confusing.
2. **Remove all wide-event span projection** — Rejected. Transition, authz, and invariant views still rely on wide-event spans today.
3. **Fix the problem only in Datadog queries** — Rejected. Query conventions do not solve the underlying duplication and do not produce a true single-trace tree.

## Rollback Policy

Re-enable `LlmCall` and `WasmInvocation` span projection in `wide_event::emit_span`, and disable the loopback interception for `File/$value` calls. No persistent-state migration is involved.
