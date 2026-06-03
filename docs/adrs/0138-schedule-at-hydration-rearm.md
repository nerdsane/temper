# ADR-0138: Schedule-At Hydration Re-Arm

- Status: Accepted
- Date: 2026-06-03
- Deciders: Temper core maintainers
- Related:
  - ADR-0049: Runtime State Timeouts
  - ADR-0056: State Timeout Hydration Re-Arm
  - `crates/temper-server/src/entity_actor/effects.rs`
  - `crates/temper-server/src/state/entity_ops.rs`
  - `crates/temper-server/src/state/dispatch/effects.rs`

## Context

`schedule_at` effects are the Temper primitive used by cron-style entities to
schedule a future action from an entity field. The action and timestamp are
durable because they are represented by entity state and event history, but the
Tokio task that delivers the action is process-local.

After a server restart or deployment, persisted entities can hydrate back into a
state whose latest transition already declared a `schedule_at` effect. Before
this ADR, hydration rebuilt the entity state but did not restore the in-memory
timer. A cron entity could therefore remain `Active` with a stale `next_run_at`
until a human paused/resumed it or another action happened to schedule a new
timer.

This is the same class of durability gap addressed for `[[state_timeout]]` by
ADR-0056, but `schedule_at` is transition-effect based rather than state based.

## Decision

When an entity is loaded from the event journal, Temper inspects the hydrated
state's recent event history for the current state epoch. It finds the most
recent transition whose IOA rule declared one or more `schedule_at` effects,
resolves the current entity field values into `ScheduledAction`s, and dispatches
those runtime timers under the `schedule-at-hydration` service context.

The entity remains the source of truth:

- no scheduler row or side table is introduced;
- no polling loop is introduced;
- if the timestamp field is missing or invalid, no timer is armed;
- if the timestamp is already in the past, the scheduled action fires with zero
  delay, matching existing `schedule_at` semantics.

Hydration only re-arms after an entity is newly loaded from durable events. If
the entity is already present in memory, a second hydration pass does not create
another timer.

## Rollout Plan

1. **Phase 0 (Immediate)** — Ship event-history recovery for `schedule_at`
   timers and wire it into journal hydration.
2. **Phase 1 (Follow-up)** — Add production telemetry for recovered
   `schedule_at` timers if operational volume requires it.

## Consequences

### Positive

- Active cron entities resume after deploys without manual pause/resume.
- The fix stays inside Temper's declarative entity runtime and preserves the
  trigger boundary.
- Past-due cron jobs catch up immediately on hydration.

### Negative

- Recovery depends on the retained recent event window. If the current-state
  epoch and scheduling event are both outside the retained history, Temper
  cannot infer the transition effect to re-arm.

### Risks

- A malformed or missing timestamp field still leaves the entity unscheduled.
  This is intentional because the entity state is invalid and should be repaired
  by the owning app or integration.

### DST Compliance

- The recovered delay is computed with `sim_now()`, the same clock used by
  normal `schedule_at` resolution.
- Timer delivery remains a runtime side effect; deterministic behavior is still
  exercised through the shared effect-resolution code path.

## Non-Goals

- This ADR does not introduce a durable scheduler table.
- This ADR does not change `[[state_timeout]]` semantics.
- This ADR does not change cron app state machines.

## Alternatives Considered

1. **Manual cron pause/resume after deploys** — Rejected because it leaves
   production correctness dependent on operator action.
2. **Rust polling loop over active cron entities** — Rejected because it violates
   Temper's entity-first architecture and hides flow outside state transitions.
3. **Durable scheduler table** — Deferred. It may be the long-term primitive,
   but hydration re-arm closes the observed production gap with less surface
   area.

## Rollback Policy

Revert the hydration hook and recovery helper. Normal `schedule_at` timers
created by live transitions continue to work, but timers for hydrated entities
would again require a new state transition to be armed after restart.
