# ADR-0158: State-Timeout Resume at Boot

## Status

Accepted (2026-07-12)

## Context

`[[state_timeout]]` declarations (ADR-0049) arm in-memory tokio timers when a
transition enters a timed state. ADR-0056 added hydration re-arm, but it only
runs inside the post-dispatch hook — it requires traffic to the entity. A
state timeout exists precisely to fire when *nothing* happens, so an entity
sitting in a timed state across a server restart never received its
`on_timeout`: the timer died with the old process and no dispatch ever
triggered the re-arm (ARN-203). The `Ticket stuck in Open forever after a
deploy` failure mode.

## Decision

1. **Boot resume sweep.** `ServerState::resume_pending_state_timeouts(tenant)`
   runs after the boot entity-index population (both tenant boot paths:
   `populate_index_from_store` and `hydrate_from_store`), as a background
   task so boot latency is unaffected. For each entity type whose spec
   declares state timeouts, it hydrates the type's entities and arms a timer
   for every entity in a declared state with no live timer.
2. **Remaining budget, not a fresh one.** The sweep reuses the ADR-0056
   clock-reset reconstruction (`compute_state_clock_reset_ts` over the
   retained event log): entities get `budget − elapsed`; overdue entities
   fire on the next tick. When no entry event is retained the full budget is
   armed (safe: worst case one extra budget of wait).
3. **One arm/spawn implementation.** The dispatch-time arm, the ADR-0056
   hydration re-arm, and the boot sweep all call the same
   `spawn_state_timeout_timer` (seq bump + cancellation-checked fire task),
   so the three paths cannot drift.
4. **Idempotent per process.** Armed entities have tracker seq > 0 and are
   skipped by any repeat sweep; the seq-based cancellation makes a
   sweep-armed timer and a later dispatch-armed timer race-free (newest arm
   wins).
5. **Creation arms too.** An entity created into a timed INITIAL state has
   no dispatch to arm its timer (creation is not a dispatch), so
   `get_or_create_tenant_entity` now calls the same untracked-arm helper —
   without it, `on_timeout` never fired for an untouched entity even with
   the server up the whole time. Same defect class, found while designing
   the live E2E for the restart fix.
6. **Tracker maps are `BTreeMap`.** The tracker previously used `HashMap`;
   converted for deterministic iteration per the sim-visible crate rules.

## Consequences

- Pending timeouts now survive restarts; overdue ones fire promptly at boot.
- The sweep hydrates every entity of types that declare timeouts (only
  those types). Cost scales with the number of entities of timed types, in
  the background. If a deployment ever holds very large timed types, the
  natural optimization is filtering by status through the query-plane
  projection before hydrating — deliberately not done now to avoid a second
  status source of truth in the sweep.
- `Effect::ScheduleAction` / `ScheduleAtAction` timers (spec `schedule`
  effects, a separate mechanism in `dispatch_scheduled_actions`) remain
  in-memory only and do NOT survive restarts. That is a distinct defect
  surface tracked outside this change — recorded as a residual on the PR.

## Alternatives Considered

- **Durable schedule table** (persist every armed timer, delete on fire).
  More machinery and a second source of truth; the event log already carries
  everything needed to reconstruct pending state timeouts, which is the
  event-sourced direction ADR-0056 pointed at.
- **Sweep inside the serve bootstrap only.** Wiring inside the two tenant
  hydration functions covers every boot path (serve, tests, future
  embeddings) instead of one caller.
