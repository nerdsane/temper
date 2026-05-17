# ADR-0098: Background WASM Trace Retention

- Status: Proposed
- Date: 2026-05-17
- Deciders: Temper core maintainers
- Related:
  - ADR-0052: Instrumentation as Policy
  - ADR-0053: Datadog Service Decoupling
  - ADR-0059: Workflow Trace Context Propagation
  - ADR-0081: Latency Observability Acceleration Program
  - ADR-0083: Trace Budget and Fanout Summarization
  - ADR-0086: WASM Host Boundary Observability
  - `crates/temper-observe/src/otel/sampler.rs`
  - `crates/temper-server/src/state/dispatch/mod.rs`
  - `crates/temper-server/src/state/dispatch/wasm.rs`

## Context

PERF-009 removed the eager provider-caller heartbeat from the normal Session
fast path and was verified in production. The follow-up Datadog trace still
showed a roughly one-second cadence between Session workflow actions:

- `ProvisionWorkspace`
- `WorkspaceReady`
- `ContextReadyAuthSkipped`
- `ProviderResponseReady`
- `RecordResult`
- `MarkTrajectoryEmitted`

The retained action dispatch spans were small, usually about 17-21 ms. That
rules out the actor dispatch core as the dominant owner of the remaining
multi-second workflow time, but it does not identify whether the time belongs
to WASM module execution, host callback dispatch, scheduler wakeup, hidden
HTTP calls, or Datadog sampling.

Code inspection showed the normal Session workflow uses `[[action.triggers]]`
with `kind = "wasm"` for `workspace_provisioner`, `context_preparer`,
`provider_caller`, and `provider_response_applier`. Those modules return a
host result with `set_success_result(...)`; Temper then dispatches the callback
through `dispatch_wasm_callback`. That path should appear in APM as:

```text
dispatch.background_wasm_integrations
  -> wasm:<module>
    -> dispatch.dispatch_wasm_callback
      -> dispatch.dispatch_tenant_action_core
```

The production trace did not include that subtree. It did include
`dispatch.background_adapter_integrations`, because adapter background spans
are not reduced-sampled. The missing WASM subtree is explained by the current
name-based sampler: `dispatch.background_wasm_integrations` is included in
`DISPATCH_BACKGROUND_PREFIXES`, whose default keep rate is 25%. Because the
sampler is parent-based below that dropped span, the child module and callback
spans are dropped too.

That sampling policy made sense when background WASM was mostly volume noise.
It is now a blocker for the latency acceleration program: the workflow-critical
Session path lives behind that parent span, and hiding it prevents measured
optimization.

## Decision

### Sub-Decision 1: Keep Background WASM Spans By Default

Remove `dispatch.background_wasm_integrations` from the reduced-sampling
prefix list. Background WASM dispatch becomes delegated to the parent-based
AlwaysOn sampler, matching `dispatch.dispatch_tenant_action_core` and preserving
the complete workflow subtree when the workflow trace is sampled.

While touching this policy, also preserve the pre-existing test intent that
`wasm:monty_repl` module boundaries are not reduced-sampled. Tool execution
latency is part of the same Session workflow diagnosis, and dropping that guest
entry span can sever tool-call child spans.

**Why this approach**: The Session fast path is made of WASM triggers. Sampling
away the parent span erases the child module and callback spans that we need to
attribute latency and correctness. We have no observability budget constraint
for this program, so correctness of diagnosis is more important than reducing
this span volume.

### Sub-Decision 2: Keep Noisy Non-WASM Background Work Reduced

Continue reduced-sampling lower-value background spans such as
`dispatch.phase.query_projection` and `dispatch.scheduled_actions`.

**Why this approach**: We should not turn the sampler into a blanket firehose.
The latency-critical gap is the background WASM parent, not every background
maintenance span. Keeping the narrower reduction avoids unnecessary trace
volume while preserving the path we are actively optimizing.

### Sub-Decision 3: Treat Missing WASM Spans As A Readiness Failure

For Session latency work, a live trace is not considered diagnostic unless it
contains the relevant WASM module spans or explicit evidence explaining why no
WASM trigger was expected.

**Why this approach**: Otherwise we can mistake an observability artifact for a
runtime architecture limit. The next optimization slice must be selected from a
trace that can show module duration, callback duration, and action dispatch
duration in the same workflow.

## Rollout Plan

1. **Phase 0 (Immediate)** - Remove background WASM dispatch from reduced
   sampling, add sampler tests, and document the decision.
2. **Phase 1 (TemperPaw Uptake)** - Bump TemperPaw to the Temper revision that
   contains this change, rebuild the runtime image, and deploy to production.
3. **Phase 2 (Live Evidence)** - Run the same mock Session proof used in
   PERF-009 and confirm Datadog retains `dispatch.background_wasm_integrations`,
   `wasm:workspace_provisioner`, `wasm:context_preparer`,
   `wasm:provider_caller`, and `wasm:provider_response_applier` spans.
4. **Phase 3 (Latency Improvement)** - Use the complete trace to choose the
   next actual latency optimization: inline/synchronous callback chaining,
   scheduler cadence, module I/O reduction, or provider/context fast paths.

## Readiness Gates

- Sampler unit tests prove background WASM dispatch delegates to AlwaysOn even
  for trace IDs that a reduced rule would drop.
- Existing reduced-sampling tests still prove noisy auxiliary/background spans
  are bounded.
- Live Datadog trace for a mock Session includes the WASM subtree and keeps the
  PERF-009 no-eager-heartbeat invariant.
- Dashboard records the before/after traces and the selected next latency owner.

## Consequences

### Positive

- Session workflow traces become complete enough to identify the owner of the
  remaining seconds.
- The latency program stops relying on inference from action gaps alone.
- Production Datadog evidence can directly compare module time, callback time,
  dispatch time, and database/API side effects.

### Negative

- Trace volume increases for background WASM workflows.
- Very high-volume tenants may need a future tenant-level or workflow-level
  sampling policy instead of the current global default.

### Risks

- Increased trace volume may expose exporter or Datadog ingestion pressure. The
  mitigation is to keep other noisy background spans reduced and to add a
  future targeted override only if Datadog pressure becomes measurable.
- If some WASM workflows are intentionally high-volume and not latency-critical,
  full retention may be more than they need. The mitigation is to introduce
  module- or tenant-scoped sampling later, without hiding Session-critical spans.

### DST Compliance

- This change is in observability sampling and does not touch the deterministic
  simulation runtime, transition tables, actor scheduling semantics, or entity
  state.
- No new nondeterministic runtime behavior is introduced.

## Non-Goals

- This ADR does not change Session state semantics, IOA specs, Cedar policy, or
  WASM callback behavior.
- This ADR does not claim that the remaining latency is caused by sampling. It
  only removes the sampling blind spot so the next optimization is measured.
- This ADR does not disable all trace sampling across Temper.

## Alternatives Considered

1. **Set `TEMPER_TRACE_DISPATCH_BACKGROUND_SAMPLE_PCT=100` only in production**
   - This is a useful emergency override, but it keeps the default code policy
     misleading for future latency work and still groups WASM with unrelated
     background spans.
2. **Keep the sampler unchanged and rely on `wasm_invocation_logs`**
   - Invocation logs can prove that modules ran, but they do not give the same
     flamegraph shape, parent/child timing, callback dispatch timing, or DB/API
     correlation as APM spans.
3. **Wait for a trace ID that happens to pass the 25% sample**
   - This wastes time and makes live proof non-repeatable. The program needs
     deterministic, repeatable diagnostic evidence.

## Rollback Policy

Re-add `dispatch.background_wasm_integrations` to the reduced-prefix list or
set a production override for the dispatch background sample percentage if
Datadog ingestion pressure becomes measurable and more important than Session
latency diagnosis.
