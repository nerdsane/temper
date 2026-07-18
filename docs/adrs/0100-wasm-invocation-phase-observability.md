# ADR-0100: WASM Invocation Phase Observability

- Status: Proposed
- Date: 2026-05-18
- Deciders: Temper core maintainers
- Related:
  - ADR-0175: WASM Host Span Hint Datadog Fields
  - ADR-0086: WASM Host Boundary Observability
  - ADR-0087: WASM Guest Observability Host API
  - ADR-0099: Local WASM TData Host Path
  - `crates/temper-wasm/src/engine/mod.rs`
  - `crates/temper-server/src/state/dispatch/wasm.rs`

## Context

TemperPaw production traces after the inline terminal reply delivery change show that the direct
Channel delivery hop is no longer the dominant terminal cost. The remaining user-visible terminal
heat is inside generic Session WASM integration execution:

- `Session.RecordResult.integrations`: about 696 ms on the routed live proof.
- `wasm:agent_reply`: about 342 ms, while its inline `ReplyDelivered` HTTP/action work is about 24 ms.
- `wasm:emit_ots_trajectory`: about 354 ms, while `POST /api/ots/trajectories` is about 22 ms.

Temper already has module-level `wasm:<module>` spans, `wasm.host.*` spans, cached compiled
modules, and pre-linked `InstancePre` templates. However, the hot path used by the server calls
`WasmEngine::invoke_with_blobs` directly, while the broad `wasm.invoke` instrumentation currently
sits on `WasmEngine::invoke`. Datadog therefore shows the module boundary and host calls, but not
the engine phases inside that boundary: context serialization, store setup, instantiation, guest
context copy, guest `run`, result extraction, and result parsing.

Without those phase spans, we cannot responsibly choose between larger architectural moves such as
slim invocation contexts, native terminal adapters, background OTS emission, SDK ABI-context parsing,
or guest business-logic refactors.

## Decision

### Instrument The Actual Invocation Path

Move the broad `wasm.invoke` span to `WasmEngine::invoke_with_blobs`, the path used by server
dispatch and direct router invocation. Keep `WasmEngine::invoke` as a convenience wrapper without
adding a duplicate parent span.

**Why this approach**: This makes production traces reflect the code path that actually runs. It
also keeps public wrapper callers and blob-aware callers under one observability contract.

### Add Engine Phase Child Spans

`WasmEngine::invoke_blocking` will emit child spans under `wasm.invoke` for the main phases:

- `wasm.invoke.serialize_context`
- `wasm.invoke.prepare_host_state`
- `wasm.invoke.create_store`
- `wasm.invoke.instantiate`
- `wasm.invoke.bind_exports`
- `wasm.invoke.write_context`
- `wasm.invoke.run`
- `wasm.invoke.read_result`
- `wasm.invoke.parse_result`

The spans will include safe operational fields such as `context_bytes`, `module_hash`,
`needs_wasi`, `blob_cache_entries`, `blob_cache_bytes`, `result_source`, and `result_bytes`. They
will not include context payloads, prompts, secrets, reply text, trajectory JSON, or result bodies.

**Why this approach**: Phase-level evidence lets us distinguish platform overhead from guest work.
It preserves the mission-critical spec/WASM execution model while giving us the detail needed to
pick the next speed slice.

## Rollout Plan

1. **Phase 0 (Immediate)** — Ship phase spans in Temper core with existing WASM tests.
2. **Phase 1 (TemperPaw bump)** — Update TemperPaw to the new Temper revision and deploy to Railway.
3. **Phase 2 (Live proof)** — Run direct and routed TemperPaw sessions and compare phase spans
   against the PERF-017 baseline.
4. **Phase 3 (Next optimization)** — Use the phase split to choose between slim contexts,
   SDK ABI-context parsing, native terminal delivery, or background OTS emission.

## Readiness Gates

- Existing `temper-wasm` unit and E2E tests pass.
- `temper-server` WASM dispatch tests pass.
- Datadog traces show `wasm.invoke.*` child spans for live TemperPaw Session integrations.
- No payloads, secrets, prompts, reply content, or trajectory bodies are emitted in span attributes.

## Consequences

### Positive

- Gives Datadog actionable phase-level evidence for the largest remaining terminal latency bucket.
- Makes the already-existing `wasm.invoke` observability land on the production path.
- Preserves the existing WASM integration contract and generated-spec behavior.

### Negative

- Adds several spans to each WASM invocation trace, increasing trace volume modestly.
- Does not itself reduce latency; it enables the next measured optimization.

### Risks

- Traces can become visually denser. Span names are grouped under `wasm.invoke.*` to keep flamegraphs
  navigable.
- Misplaced fields could leak payload data. The implementation records byte counts and booleans only.

### DST Compliance

- `temper-wasm` is sandbox infrastructure and uses wall-clock timing only for execution budgets and
  observability.
- This change adds tracing spans only. It does not introduce new state transitions, random sources,
  filesystem access, network access, or nondeterministic simulation-visible behavior.

## Non-Goals

- This ADR does not change the IOA spec format or integration trigger semantics.
- This ADR does not introduce slim invocation contexts.
- This ADR does not move OTS trajectory emission out of the terminal inline path.
- This ADR does not convert TemperPaw Session terminal behavior to a native adapter.

## Alternatives Considered

1. **Jump directly to native terminal adapters** — May become right, but it bypasses the
   spec/WASM model before we know which phase dominates.
2. **Slim context first** — Likely high leverage if serialization or guest parsing dominates, but
   correctness depends on each module's data needs. Phase spans should prove the bottleneck first.
3. **Use profiling only** — Profiling is useful where available, but span-level phase evidence ties
   directly to request traces and can be compared in live end-to-end proofs.

## Rollback Policy

Remove the `wasm.invoke.*` phase spans and move the broad `wasm.invoke` instrumentation back to the
public wrapper if trace volume is too high. The runtime contract for WASM modules remains unchanged,
so rollback does not require app spec changes.
