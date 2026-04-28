# ADR-0067: Trajectory Outbox

- Status: Accepted
- Date: 2026-04-28
- Deciders: Temper core maintainers
- Related:
  - ADR-0057: Canonical Dispatch Traces and Selective Wide-Event Projection
  - ADR-0058: Query-plane hot field opt-out and stable projections
  - `crates/temper-server/src/state/dispatch/effects.rs`

## Context

Trajectory rows are audit and observability data. They are not required for the correctness of an entity transition, yet synchronous or unbounded trajectory persistence can affect user-visible latency when the backing store stalls. The April 28 incident showed that a low-priority Turso write gate lane could starve trajectory writes and hold the response path long enough for session state timeouts to fire.

Current dispatch code moved trajectory persistence into a spawned task. That removes direct awaiting but is still unbounded: a storage stall can accumulate tasks and memory without a clear drop policy.

## Decision

Trajectory persistence uses a bounded outbox:

- Capacity defaults to 8192 entries.
- `try_record` is synchronous and non-blocking.
- The drop policy is drop-newest with a structured warning and a counter.
- One drainer task batches records and calls the selected trajectory sink.

Trajectory loss is acceptable under overload because trajectories are audit data. Blocking entity transitions on a full audit queue is not acceptable.

## Rollout Plan

1. Add outbox tests for non-blocking enqueue and drop-newest behavior.
2. Add `TrajectoryOutbox` and metrics.
3. Route dispatch and API trajectory call sites through `try_record`.
4. Keep direct persistence available for tests and maintenance commands.

## Readiness Gates

- Tests prove enqueue does not await the store.
- Tests prove full outbox drops newest and reports the drop.
- Local OpenPaw DM flow succeeds with trajectory writes delayed.

## Consequences

### Positive

- User reply latency is bounded by actual workflow work, not audit-store tail latency.
- Backpressure is explicit and observable.

### Negative

- Under severe overload, some trajectory records can be dropped.

### Risks

- Dropped audit rows can reduce forensic fidelity. Metrics and warnings must make the loss visible.

### DST Compliance

- The outbox is an external observability side effect. Simulation-visible state transition semantics do not depend on it.
- Background task annotations use `determinism-ok` comments at spawn points.

## Non-Goals

- This ADR does not make trajectory persistence transactional with entity events.
- This ADR does not guarantee exactly-once trajectory persistence.

## Alternatives Considered

1. **Await trajectory persistence** — rejected because it puts audit writes on the hot path.
2. **Unbounded spawn per entry** — rejected because it hides overload.
3. **Drop oldest** — rejected because newest failures are usually the most relevant signal during an incident.

## Rollback Policy

Disable the outbox by routing `try_record` to direct background persistence. This restores current behavior while retaining Postgres platform parity.
