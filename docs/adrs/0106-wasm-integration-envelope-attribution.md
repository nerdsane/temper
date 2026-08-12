# ADR-0106: WASM Integration Envelope Attribution

- Status: Proposed
- Date: 2026-05-19
- Deciders: Temper core maintainers
- Related:
  - ADR-0175: WASM Host Span Hint Datadog Fields
  - ADR-0086: WASM Host Boundary Observability
  - ADR-0087: WASM Guest Observability Host API
  - ADR-0100: WASM Invocation Phase Observability
  - ADR-0105: Data-Only Entity Create Fast Path
  - `crates/temper-server/src/state/dispatch/wasm.rs`
  - `crates/temper-wasm/src/engine/mod.rs`

## Context

The latency program now requires every improvement to carry before/after evidence from live runs
and Datadog. PERF-028 proved this discipline is necessary: after replacing strict SessionEntry
read-back with an acknowledgement contract, live production sessions remained correct, but the
`Session.ProviderResponseReady.integrations` span did not materially improve.

Datadog now shows a sharper split:

- The broad `Session.ProviderResponseReady.integrations` and `wasm:provider_response_applier`
  spans remain about 400-600 ms on warm live sessions.
- Guest-level TemperPaw logs show the hot business step, `append_session_tree`, is usually about
  50-91 ms, with artifact reads at 0-1 ms.
- ADR-0100 added `wasm.invoke.*` engine phase spans, but the production trace still has a large
  un-attributed envelope outside guest business logic and outside the engine child spans.

This means the next responsible step is not another speculative speed patch. We need to attribute
the server-side integration envelope around module cache resolution, context building, blob
hydration, secret resolution, host construction, observe-event writes, engine invocation, result
recording, and callback dispatch. Without this attribution, the program cannot distinguish an
architectural limitation of Temper's WASM/spec model from an overlooked synchronous step in the
host bridge.

## Decision

### Add Dispatch Envelope Phase Spans

`dispatch_single_integration` will emit child spans under the existing `wasm:<module>` span for the
major server-side envelope phases:

- `dispatch.wasm.phase.module_cache`
- `dispatch.wasm.phase.replay_input_injection`
- `dispatch.wasm.phase.invocation_context_build`
- `dispatch.wasm.phase.blob_ref_hydration`
- `dispatch.wasm.phase.authz_secret_resolution`
- `dispatch.wasm.phase.host_chain_build`
- `dispatch.wasm.phase.integration_observe_start`
- `dispatch.wasm.phase.engine_invoke_and_handle`

Where practical, result handling will also emit child spans for the large post-engine phases:

- `dispatch.wasm.phase.engine_invoke`
- `dispatch.wasm.phase.result_observe_complete`
- `dispatch.wasm.phase.record_invocation`
- `dispatch.wasm.phase.dispatch_callback`
- `dispatch.wasm.phase.llmobs_submit`

Each span will record safe operational attributes only: phase name, module name, tenant, entity
type, entity id, trigger action, result status, duration in milliseconds, and count/byte-size
metadata where useful. The spans must not record entity payloads, prompts, secrets, reply content,
trajectory JSON, artifact bodies, or WASM result bodies.

**Why this approach**: The missing latency is currently between the high-level module span and the
guest/engine child spans. Envelope spans put Datadog evidence exactly at that boundary without
changing the IOA spec model, guest WASM contract, or production semantics.

### Treat This As A Measurement Slice, Not A Speed Claim

This PR will not claim a latency reduction unless live before/after data proves one. Its success
condition is attribution: after deployment, Datadog must show which envelope phase consumes the
remaining hundreds of milliseconds. The next optimization will target the dominant proven phase and
will carry its own before/after proof.

**Why this approach**: The program has already found a correctness-positive change that was not a
latency win. Separating measurement from optimization keeps the work honest and prevents premature
architecture changes.

## Rollout Plan

1. **Phase 0 (Immediate)** — Add a source-level observability contract test that fails if required
   dispatch envelope phase names disappear.
2. **Phase 1 (Temper core PR)** — Add the envelope spans around existing dispatch phases without
   changing behavior.
3. **Phase 2 (TemperPaw bump and deploy)** — Build TemperPaw against the Temper revision, deploy,
   and run the same live Session proof used for PERF-028.
4. **Phase 3 (Datadog readout)** — Compare the new trace shape to the PERF-028 baseline and select
   the next latency PR from the dominant envelope phase.

## Readiness Gates

- A focused `temper-server` test asserts the required dispatch envelope span names exist.
- Existing WASM dispatch tests pass.
- `cargo fmt --all -- --check` passes.
- Live TemperPaw sessions complete correctly after deployment.
- Datadog traces for live `Session.ProviderResponseReady` executions show the new
  `dispatch.wasm.phase.*` child spans.
- The living latency report records both the PERF-028 baseline and the new after-deploy attribution.

## Consequences

### Positive

- Turns the current 400-600 ms parent-span gap into actionable phase evidence.
- Preserves Temper's core mission: generated specs, WASM integrations, and governed runtime behavior
  remain intact.
- Makes out-of-the-box future options easier to evaluate because the cost boundary is explicit.

### Negative

- Adds trace volume to each WASM integration invocation.
- Does not directly reduce latency in this PR.
- Dense traces require disciplined span naming so the flamegraph stays readable.

### Risks

- Span fields could accidentally leak sensitive data. The mitigation is to record identifiers,
  sizes, counts, phase names, and status only.
- Holding tracing span guards across `.await` points can distort async context. The implementation
  should use `Instrument` for async work and short enter guards for synchronous blocks.
- Some phases may initially show small durations because deeper child spans already cover the work.
  That is acceptable; the goal is to close the un-attributed gap.

### DST Compliance

- This change touches `temper-server`, which is simulation-visible.
- The implementation adds observability spans and elapsed-time fields only. It does not change actor
  transitions, scheduling, mailbox behavior, random sources, persistence semantics, network calls, or
  tenant scoping.
- Wall-clock duration is observability-only and does not feed decisions in simulation-visible logic.

## Non-Goals

- This ADR does not introduce slim invocation contexts.
- This ADR does not bypass WASM integrations or generated specs.
- This ADR does not move SessionEntry materialization to a new storage model.
- This ADR does not change OData, projection, or Cedar authorization semantics.
- This ADR does not claim a performance improvement without live before/after evidence.

## Alternatives Considered

1. **Optimize the TemperPaw guest helper again** — Rejected for this slice because the helper already
   batches initial SessionEntry creation and guest logs show the dominant parent-span gap is outside
   the obvious guest step.
2. **Jump directly to native provider-response adapters** — Still a possible future direction, but
   premature until Datadog proves the dispatch envelope or WASM architecture is the limiting factor.
3. **Rely on profiler data only** — Profiling is useful, but request-scoped spans are required for
   live before/after comparison, PR review, and production correctness correlation.

## Rollback Policy

Remove the `dispatch.wasm.phase.*` spans if trace volume is too high or if a field is found to be
unsafe. The runtime behavior is unchanged, so rollback does not require data migration, spec changes,
or TemperPaw app changes.
