# ADR-0170: State-Timeout Resume at Boot

- Status: Accepted
- Date: 2026-07-14
- Deciders: Temper core maintainers
- Related:
  - ADR-0049: state_timeout declarations
  - ADR-0056: hydration re-arm on dispatch
  - ARN-203
  - `crates/temper-server/src/state/dispatch/state_timeouts.rs`

## Context

`[[state_timeout]]` timers (ADR-0049) are in-memory `tokio` tasks. ADR-0056 re-arms on
dispatch when tracker seq is 0, but an entity sitting in a timed state across a **restart**
receives no dispatch by definition — the timeout exists to fire when nothing happens. The
timer dies with the process and never re-arms (ARN-203). Creation into a timed initial
state has the same gap: create is not a dispatch.

## Decision

1. **Boot resume sweep.** `ServerState::resume_pending_state_timeouts(tenant)` runs after
   entity-index population on both tenant boot paths, in a background task. For each type
   with state timeouts, hydrate entities and arm any timed state with no live timer.
2. **Remaining budget.** Reuse `compute_state_clock_reset_ts`; overdue entities fire with
   zero delay. Missing entry event → full budget (safe default).
3. **Single spawn path.** Dispatch arm, hydration re-arm, boot sweep, and creation share
   `spawn_state_timeout_timer`. Untracked arms use `bump_if_zero` for CAS idempotency.
4. **Creation arms.** `get_or_create_tenant_entity` calls `arm_untracked_state_timeouts`.
5. **BTreeMap trackers.** Deterministic iteration for sim-visible crate rules.

## Consequences

- Pending timeouts survive restarts; overdue ones fire at boot.
- Sweep cost scales with entities of timed types (background).
- Spec `schedule` effects remain in-memory only (separate residual).

## Alternatives Considered

- Durable schedule table: second source of truth; event log already reconstructs clocks.
- Sweep only in CLI bootstrap: miss test/embed boot paths.

## DST Compliance

- Wall-clock arm uses `tokio::spawn` with `// determinism-ok` (timer side-effect).
- Tracker maps are `BTreeMap`. Fire path uses existing `__scheduled` dispatch.
