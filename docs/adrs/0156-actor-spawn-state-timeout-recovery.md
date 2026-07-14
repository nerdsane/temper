# ADR-0156: Actor-Spawn State-Timeout Recovery

- Status: Proposed
- Date: 2026-07-13
- Deciders: Temper core maintainers
- Supersedes: the request-traffic-dependent hydration implementation of ADR-0056 Sub-Decision 1
- Related:
  - ADR-0049: State-entry timeouts and durable scheduler
  - ADR-0050: Mandatory liveness coverage for non-terminal states
  - ADR-0056: Durable state timeouts and silent-exit prevention
  - ADR-0028: Memory-bounded lazy hydration and passivation
  - `crates/temper-server/src/state/entity_ops.rs`
  - `crates/temper-server/src/state/dispatch/state_timeouts.rs`

## Context

ADR-0056 requires a timed entity to re-arm its deadline when its actor hydrates after process restart or passivation. The implementation reconstructs elapsed time from durable events, but invokes that logic only from `run_post_dispatch_effects`. A restarted entity therefore has no timer until some unrelated action is dispatched. If no request arrives, its declared liveness transition never fires.

Moving a `ServerState` clone into `EntityActor::pre_start` would let the actor call the scheduler directly, but it would also create a strong ownership cycle: `ServerState` owns the actor system and actor registry, while each actor would retain `ServerState`. Directly sending the timeout action to the entity actor is also incorrect because it bypasses the server dispatch path and its authorization, reactions, telemetry, and subsequent timeout arming.

The durable facts are the entity event history and the snapshot of the state rebuilt from it. The missing operations are a lifecycle trigger that asks the existing timeout scheduler to reconcile as soon as a new actor becomes ready, plus a snapshot-carried timeout clock anchor for histories whose entry/reset event is older than the hot replay tail.

## Decision

### Sub-Decision 1: Every new actor spawn schedules timeout reconciliation

Immediately after `ServerState` inserts a newly spawned entity actor into the registry, it starts one bounded readiness task. The task asks the actor for its hydrated state using the existing bounded retry policy, then calls the state-timeout scheduler in explicit hydration mode.

This hook applies uniformly to:

- eager server-start hydration;
- lazy loading of durable entities;
- respawn after idle passivation; and
- first creation of an entity whose initial state declares a timeout.

The task is finite and owns a temporary `ServerState` clone only until readiness is resolved. `EntityActor` does not retain `ServerState`, so no ownership cycle is introduced.

**Why this approach:** actor spawn is the earliest common lifecycle boundary shared by every hydration path. Request handlers are incomplete because an entity may receive no traffic, while actor `pre_start` cannot safely own the server dispatcher.

### Sub-Decision 2: Index-only startup activates timed entities

The default CLI boot path populates the durable entity index without hydrating actors. That optimization remains in place for entity types with no `[[state_timeout]]`. For every persisted entity whose registered spec declares a timeout, index population immediately spawns the actor so Sub-Decision 1 can reconcile its current state and deadline before any request traffic.

**Why this approach:** projection-backed reads do not necessarily spawn an actor, so actor-lifecycle reconciliation alone cannot uphold a no-traffic liveness promise. A timed entity carries an active scheduling obligation and therefore cannot be treated as fully dormant while the scheduler remains in-process. Selective activation preserves lazy startup for non-timed data instead of restoring eager hydration for the whole tenant.

### Sub-Decision 3: Hydration is explicit, not inferred from the last event

The shared timeout-arm implementation accepts an explicit hydration cause. In hydration mode it does not treat the replayed last event as a fresh state entry. Instead it:

1. confirms that the current state declares a timeout;
2. atomically reserves the initial sequence only when the per-process tracker has no live timer for this entity;
3. derives the latest state-entry or `reset_on` timestamp from the snapshot-carried clock anchor and replayed durable tail;
4. arms the remaining budget, or dispatches on the next runtime tick when already overdue; and
5. uses the existing sequence and state checks to cancel stale or racing timers.

A concurrent real dispatch may arm or invalidate a timer before the readiness task completes. Atomic reservation makes both orderings safe: hydration does nothing when dispatch armed first, while a later dispatch supersedes an earlier hydration reservation without either path erasing the other path's deadline.

The readiness task captures both the deterministic event-clock observation and a Tokio monotonic-time anchor before it is spawned. Reconciliation advances the observation by the measured readiness interval and compares that instant directly with the durable anchor. This charges only time after the durable entry/reset event, including when first creation persists `Created` after the observation. The scheduler then creates one absolute Tokio deadline before spawning the timer task and uses `sleep_until`, so task queueing cannot move the deadline later. Paused Tokio time keeps DST replay exact.

**Why this approach:** the existing code inferred hydration from “same state plus tracker sequence zero.” That inference works only after traffic arrives and can misclassify the replayed entry event as a brand-new entry with a full budget.

### Sub-Decision 4: Readiness failure is bounded, observable, and recoverable

The readiness ask uses the existing retry budget. Exhaustion logs a structured error with tenant, entity type, and entity ID. It does not invent state or arm a timer from incomplete replay. An actor that permanently fails startup closes its mailbox; the entity registry treats only an open mailbox as a live incarnation. The next access atomically replaces a stopped registry entry under the existing spawn lock, re-runs hydration, and schedules timeout reconciliation. Concurrent callers still converge on one live actor.

**Why this approach:** silently dropping the lifecycle task would recreate the regression; retrying forever would create an unbounded background workload; retaining a stopped actor reference would make a transient persistence failure permanent until process restart.

### Sub-Decision 5: Persist the timeout clock anchor in current snapshots

`TransitionTable` carries the verified `[[state_timeout]]` declarations into the production actor. As each durable event enters a timed state or executes a declared `reset_on` action, the actor updates one `state_timeout_clock_reset_at` value in the same state mutation. Replayed tail events advance or clear it alongside state, including tombstones. Periodic and passivation snapshots use one shared encoder that persists the timestamp while continuing to omit the bounded hot event deque. Hydration therefore recovers the exact anchor even when the entry/reset event precedes the current snapshot.

Legacy snapshots that predate this optional field and have no matching state-changing event in their replay tail continue to receive one full timeout budget. Before the actor becomes ready or its timer can be armed, hydration synchronously rewrites the loaded snapshot with the conservative anchor. A missing anchor after replay proves that every post-snapshot envelope was skipped without mutating domain state: every successfully parsed event updates or clears the anchor. The replacement payload therefore keeps the loaded boundary's sequence and replay-budget fields, while the live actor retains the journal head and counts the skipped envelopes as its replay tail. A dedicated replacement operation atomically updates the current snapshot and its same-sequence history record without creating or rotating an event-segment boundary. Skipped markers replay again on the next restart, now from a snapshot with the durable anchor. A failed or concurrent upgrade write fails actor startup instead of exposing a refreshable in-memory budget. This compatibility fallback may delay the first deadline after upgrade, but even an immediate second crash and restart reuses the first upgraded anchor.

## Rollout Plan

1. **Immediate** — ship actor-spawn reconciliation and deterministic restart coverage.
2. **Observation** — verify hydration-arm and timeout-fire metrics during restart and passivation exercises.
3. **Future** — if exact clock recovery from legacy snapshot-only state becomes necessary, backfill the anchor from pre-snapshot journals or implement ADR-0049's event-log-backed scheduler.

## Readiness Gates

- A behavioral regression reproduces a persisted timed state remaining stuck after restart with no unrelated dispatch.
- The fixed test proves not-yet-overdue state uses its remaining budget and overdue state fires immediately.
- A legacy snapshot regression crashes again immediately after hydration and proves the repaired anchor and journal sequence survive without passivation or another event.
- A legacy snapshot followed by a composite journal marker repairs the loaded boundary and survives restart without appending an event or rotating segments.
- An injected repair failure followed by store recovery in the same server replaces the stopped actor incarnation, persists the anchor, and arms exactly one timer.
- Randomized deterministic seeds cover elapsed times before, at, and after the deadline.
- Actor spawn, action dispatch, and timer firing continue to use shared production code paths.
- Full workspace tests, strict Clippy, readability, DST review, and code-quality review pass.

## Consequences

### Positive

- Declared state deadlines survive restart without depending on later request traffic.
- Default index-only startup activates only timed entity types; non-timed entities remain lazy.
- Eager hydration and lazy respawn use one lifecycle rule.
- Timer dispatch continues through the complete server action path.
- Racing hydration and real actions remain idempotent through the existing sequence tracker.

### Negative

- Each new actor performs one additional bounded `GetState` ask after startup.
- Persisted timed entities consume actor and timer memory while their liveness obligation is active.
- Timeout recovery is asynchronous with respect to registry insertion, so readiness may briefly precede timer-arm observability.
- Current snapshots gain one optional timeout-anchor timestamp.
- Legacy snapshot-only entities may receive one conservative full budget after their first upgraded hydration.

### Risks

- **Readiness task exhaustion.** Mitigated by bounded retries, structured errors, stopped-incarnation replacement on the next access, and the existing post-dispatch reconciliation fallback.
- **Legacy snapshot upgrade failure.** The synchronous anchor rewrite fails actor startup; hydration never arms a timer from an anchor that another immediate restart could forget.
- **Timed-entity startup volume.** Bounded to entity types with declared liveness obligations; non-timed entities retain index-only lazy hydration. A future durable scheduler may avoid one resident actor per persisted instance of a timeout-declaring type.
- **Duplicate timers during startup races.** Mitigated by atomic initial-sequence reservation and the existing fire-time sequence/state checks.
- **Runtime task nondeterminism.** The task coordinates production actor readiness only; state mutation remains actor-serialized, time comes from `sim_now()`, and deterministic tests use a logical clock plus paused Tokio time.

### DST Compliance

- The restart regression uses `SimEventStore`, deterministic IDs, a logical `sim_now()` clock, and paused Tokio time.
- Randomized scenarios are seed-derived and assert the same state and timer outcomes for the same seed.
- No filesystem, network, wall-clock, or random source is introduced into state mutation.
- The readiness `tokio::spawn` is a bounded production lifecycle task; it does not execute state-machine mutation outside the actor.

## Non-Goals

- A separate persistent timer table.
- A continuous or cluster-wide scan beyond the existing one-time boot index scan.
- Changing `[[state_timeout]]` syntax or liveness validation.
- Backfilling an exact timer anchor into legacy snapshots that contain no retained entry/reset event.

## Alternatives Considered

1. **Store `ServerState` inside every `EntityActor`** — Rejected because it creates a strong ownership cycle and couples the kernel actor to the HTTP/server aggregate.
2. **Send timeout actions directly to the actor** — Rejected because it bypasses the shared dispatch path and its reactions, telemetry, integration behavior, and follow-on timer arming.
3. **Re-arm only in `hydrate_from_store`** — Rejected because default boot uses index-only startup, while lazy respawn and direct actor creation paths would remain uncovered.
4. **Wait for the next action** — Rejected because that is the current liveness failure.
5. **Build the full durable scheduler now** — Deferred to ADR-0049's longer-term direction; it is broader than the actor-lifecycle regression.

## Rollback Policy

Remove the post-spawn readiness task and explicit hydration cause, and restore `populate_index_from_store` to index-only behavior for every entity type. The snapshot anchor is an optional JSON field; older code ignores unknown fields, so rollback remains code-only and existing event histories and snapshots remain readable.
