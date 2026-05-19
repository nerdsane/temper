# ADR-0109: Event-Driven Observe Entity Wait

- Status: Accepted
- Date: 2026-05-19
- Deciders: Temper core maintainers
- Related:
  - ADR-0108: Native data-only create storage path
  - ADR-0106: WASM integration envelope attribution
  - `crates/temper-server/src/observe/entities.rs`
  - `crates/temper-server/src/state/dispatch/effects.rs`

## Context

The latency program now requires every remaining performance improvement to have before and after proof. PERF-031 is accepted as a targeted storage/write-path win, but the latest production proof on TemperPaw `sha-efc39803` shows that the end-to-end Session proof still has a large terminal-wait component:

- Datadog window `2026-05-19T15:18:30Z..15:21:00Z`
- `GET /observe/entities/{entity_type}/{entity_id}/wait`: avg `758.5 ms`, p50 `520.7 ms`, p95 `1004.4 ms`, p99 `1004.4 ms`, count `10`
- `Session.ProviderResponseReady.integrations`: avg `146.2 ms`, p50 `141.9 ms`, p95 `153.7 ms`, count `8`
- Remaining WASM phases are smaller than the wait span: `engine_invoke_and_handle` avg `72.0 ms`, `host_chain_build` avg `51.7 ms`, `engine_invoke` avg `38.5 ms`, and `dispatch_callback` avg `28.9 ms`

The current wait endpoint polls: read current state, sleep `poll_ms`, repeat until target status or timeout. The proof harness used a `500 ms` poll cadence, so a terminal state can be ready for hundreds of milliseconds before the caller sees it. This is not a fundamental Temper architecture limit: `ServerState` already broadcasts `EntityStateChange` events through `event_tx` on every dispatch state transition.

The endpoint is used by live proof clients and is a natural API for CLI/TUI agents that need to wait for entity completion. Returning as soon as the state-change event arrives improves real perceived latency for those clients without bypassing specs, Cedar, events, or projections.

## Decision

Make `/observe/entities/{entity_type}/{entity_id}/wait` event-driven, with polling retained only as a safety fallback.

### Subscribe Before Reading State

The handler subscribes to `state.event_tx` before the first state read. That avoids the race where a terminal event happens after the handler checks state but before it starts listening.

**Why this approach**: The broadcast channel already exists, is external-observation-only, and carries tenant/entity/status data. Reusing it avoids a new synchronization primitive and keeps the endpoint aligned with the event stream users already see.

### Return Full Current State

The event payload is used only as a wake-up hint. When a matching entity reaches a target status, the handler reloads the current entity state and returns the same JSON shape as today, including `timed_out`.

**Why this approach**: It preserves the existing API contract and avoids returning partial state from the broadcast event. If the event arrives before the cache/read path has the full state available, the state reload remains the source of truth.

### Keep `poll_ms` As Fallback

The handler waits on three things: a matching broadcast event, a fallback sleep using `poll_ms`, and the overall deadline. Channel lag or missed events trigger a state recheck. If the broadcast channel closes unexpectedly, the handler continues using the fallback poll path until timeout.

**Why this approach**: The change should reduce normal latency without making wait correctness depend exclusively on broadcast delivery. The existing timeout and current-state response semantics stay intact.

### Observability

Keep the existing route span `GET /observe/entities/{entity_type}/{entity_id}/wait` and add bounded span attributes for:

- `wait.mode = event_driven`
- `wait.wake_reason = initial_state | event | poll | lagged | timeout`
- `wait.poll_ms`
- `wait.target_status_count`

**Why this approach**: The next after proof can show not only lower duration, but also whether the endpoint was actually woken by events rather than by fallback polling.

## Rollout Plan

1. **Phase 0 (Immediate)** — Implement event-driven wait in Temper core, add tests that prove a large `poll_ms` no longer delays a matching status event, and keep timeout behavior unchanged.
2. **Phase 1 (Rollout)** — Bump TemperPaw to the Temper merge commit and deploy to Railway with exact `BUILD_SHA`, `DD_VERSION`, and `DD_GIT_COMMIT_SHA`.
3. **Phase 2 (Proof)** — Run the same live Session batch shape as PERF-031 with a `500 ms` wait poll, independently read back `SessionEntries`, and query Datadog for before/after `GET /observe/entities/{entity_type}/{entity_id}/wait` durations and `wait.wake_reason`.

## Readiness Gates

- The wait endpoint returns the same state JSON shape as before.
- Existing timeout behavior still returns the current state with `timed_out = true`.
- A new test proves a matching event wakes the endpoint before a long fallback poll would fire.
- Full Temper checks and sim-visible review gates pass.
- Production after proof records before and after p50/p95/p99 for the wait route and confirms event wake-up attributes.
- Live Session correctness still reads back exactly two `SessionEntries` for mock provider runs.

## Consequences

### Positive

- Removes up to one polling interval from user-visible wait latency.
- Uses existing state-change broadcasts instead of adding a new control plane.
- Keeps polling as a fallback for lagged/missed events.
- Produces direct Datadog evidence for wake-up reason and remaining wait time.

### Negative

- The handler now has slightly more concurrency logic than a simple polling loop.
- Broadcast lag behavior needs explicit handling and tests.

### Risks

- A broadcast event may arrive before a subsequent state read observes the new state. Mitigation: use the event as a hint and continue waiting if the reloaded state is not yet in a target status.
- A lagged receiver could miss the terminal event. Mitigation: lag handling triggers an immediate current-state read, and fallback polling remains active.
- Channel close is unexpected but possible during shutdown. Mitigation: continue with fallback polling until timeout.

### DST Compliance

This touches `temper-server`, but only an HTTP observe handler. The existing broadcast channels and `tokio::time` calls are already marked as external observation and HTTP-handler paths. No actor scheduling, deterministic simulation state, event ordering, or transition semantics change.

## Non-Goals

- Do not change entity dispatch, transition semantics, Cedar authorization, or projection writes.
- Do not make the full Session provider path subsecond by itself; this only removes wait-poll latency.
- Do not replace the replayable entity event SSE endpoint.
- Do not remove `poll_ms`; clients that rely on polling semantics keep a bounded fallback.

## Alternatives Considered

1. **Lower the default poll interval** — Reduces delay but increases read load and still quantizes latency. Rejected because event broadcasts already provide a better signal.
2. **Require clients to use SSE directly** — Pushes complexity to every proof/client caller and leaves the convenient wait endpoint slow. Rejected because the endpoint is the right abstraction for CLI/TUI/live proof clients.
3. **Add a new wait-notification registry** — Could be precise but duplicates `event_tx` and creates new lifecycle concerns. Rejected until evidence shows the existing broadcast channel is insufficient.

## Rollback Policy

Restore the old polling loop in `handle_wait_for_entity_state`. The API contract, query parameters, and response shape remain unchanged, so rollback is limited to the handler implementation and tests.
