# ADR-0171: Actor-Spawn State-Timeout Recovery

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

The readiness ask uses the existing retry budget. Actor startup and optimistic-concurrency recovery treat every journal-tail read as strict: a read failure stops that incarnation instead of publishing snapshot state whose committed tail is unknown. Exhaustion logs a structured error with tenant, entity type, and entity ID. It does not invent state or arm a timer from incomplete replay. An actor that permanently fails startup closes its mailbox; the entity registry treats only an open mailbox as a live incarnation. The next access atomically replaces a stopped registry entry under the existing spawn lock, re-runs hydration, and schedules timeout reconciliation. Concurrent callers still converge on one live actor.

**Why this approach:** silently dropping the lifecycle task would recreate the regression; retrying forever would create an unbounded background workload; retaining a stopped actor reference would make a transient persistence failure permanent until process restart.

### Sub-Decision 5: Persist the timeout clock anchor in current snapshots

`TransitionTable` carries the verified `[[state_timeout]]` declarations into the production actor. As each durable event enters a timed state or executes a declared `reset_on` action, the actor updates one `state_timeout_clock_reset_at` value in the same state mutation. Replayed tail events advance or clear it alongside state, including tombstones. Periodic and passivation snapshots use one shared encoder that persists the timestamp while continuing to omit the bounded hot event deque. Hydration therefore recovers the exact anchor even when the entry/reset event precedes the current snapshot.

Legacy snapshots that predate this optional field and have no matching state-changing event in their replay tail continue to receive one full timeout budget. Before the actor becomes ready or its timer can be armed, hydration synchronously rewrites the loaded snapshot with the conservative anchor. A missing anchor after a successful strict replay proves that every post-snapshot envelope was skipped without mutating domain state: every successfully parsed event updates or clears the anchor. The replacement payload therefore keeps the loaded boundary's sequence and replay-budget fields, while the live actor retains the journal head and counts the skipped envelopes as its replay tail. Replay returns the exact snapshot payload it loaded; a dedicated compare-and-replace operation accepts that payload as the expected value and atomically updates the current snapshot and its same-sequence history record without creating or rotating an event-segment boundary. Two replicas that loaded the same legacy boundary can therefore race, but only one replacement can succeed. Skipped markers replay again on the next restart, now from a snapshot with the durable anchor. A failed or concurrent upgrade write fails actor startup instead of exposing a refreshable in-memory budget. This compatibility fallback may delay the first deadline after upgrade, but even an immediate second crash and restart reuses the first upgraded anchor.

If replay reaches a positive journal sequence but still cannot reconstruct an anchor, hydration requires an existing snapshot boundary for the conservative repair. A journal-only history in that condition may contain only a composite marker or an envelope written by a schema version that a future compatible runtime can replay. Creating a snapshot at the journal head would permanently hide those facts. Startup therefore leaves the journal and snapshot absence unchanged, arms no timeout, and returns an observable actor-start error until a compatible runtime or explicit migration supplies a trustworthy boundary. Journal-only histories with a replayable entry or reset event remain valid because replay reconstructs their exact anchor without creating a boundary. Existing-boundary replacement remains actor-readiness work rather than background maintenance, so stores with priority admission place it ahead of low-priority snapshot traffic.

### Sub-Decision 6: Prove journal-tail completeness against an atomic head

A successful event query does not by itself prove that every committed envelope was returned: a contiguous prefix can look valid while hiding a later transition. The persistence boundary therefore exposes a journal-tail read together with the durable head sequence captured from the same logical store snapshot. Postgres and Turso use one common-table-expression query, Redis uses one Lua script over the sequence key and event list, and Sim captures both values under its deterministic store lock before applying read-truncation faults.

Actor replay requires the returned tail to start immediately after the snapshot boundary, remain contiguous, and end exactly at the captured head. Any gap, prefix truncation, or head preceding the snapshot is a hydration error before state can become ready, a timeout can be armed, or a legacy snapshot can be repaired. This completeness check is unconditional; the older lenient-read option applies only to explicit backend errors in observation-oriented callers, never to a read that proves itself structurally incomplete.

**Why this approach:** a second, independent head query would race a concurrent append and could not prove which journal view the tail represented. Capturing the head and tail in one database statement, Redis script, or simulation lock gives every backend the same durable replay contract.

## Rollout Plan

1. **Immediate** — ship actor-spawn reconciliation and deterministic restart coverage.
2. **Observation** — verify hydration-arm and timeout-fire metrics during restart and passivation exercises.
3. **Future** — if exact clock recovery from legacy snapshot-only state becomes necessary, backfill the anchor from pre-snapshot journals or implement ADR-0049's event-log-backed scheduler.

## Readiness Gates

- A behavioral regression reproduces a persisted timed state remaining stuck after restart with no unrelated dispatch.
- The fixed test proves not-yet-overdue state uses its remaining budget and overdue state fires immediately.
- A legacy snapshot regression crashes again immediately after hydration and proves the repaired anchor and journal sequence survive without passivation or another event.
- A legacy snapshot followed by a composite journal marker repairs the loaded boundary and survives restart without appending an event or rotating segments.
- A legacy timed journal with no reconstructable anchor and no snapshot fails hydration cleanly, arms no timer, and remains fully replayable after repeated restart attempts.
- An incompatible no-snapshot envelope remains visible to a future compatible runtime.
- A snapshot-read failure fails hydration without overwriting an existing boundary or rotating its segment metadata.
- A journal-tail read failure fails hydration without rewriting a stale snapshot or arming its timeout.
- A successful-looking truncated journal prefix fails hydration because it does not reach the atomically captured durable head.
- Two repairs that loaded the same legacy boundary have one winner; the stale writer cannot overwrite that anchor.
- Snapshot-boundary replacement enters the persistence writer as actor-readiness work rather than background maintenance.
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
- Hydration uses a head-bearing journal read instead of an event vector alone.
- Legacy snapshot-only entities may receive one conservative full budget after their first upgraded hydration.
- Legacy journal-only timed entities whose replay cannot reconstruct an anchor require a compatible migration before they can hydrate.

### Risks

- **Readiness task exhaustion.** Mitigated by bounded retries, structured errors, stopped-incarnation replacement on the next access, and the existing post-dispatch reconciliation fallback.
- **Legacy snapshot upgrade failure.** The synchronous anchor rewrite fails actor startup; hydration never arms a timer from an anchor that another immediate restart could forget.
- **Missing or unreadable snapshot boundary.** Timeout-anchor repair requires a successfully loaded existing boundary. Missing, unreadable, or otherwise ambiguous boundaries fail actor startup without changing durable history, so a compatible runtime or migration can reconstruct it later.
- **Unreadable journal tail.** Actor hydration and concurrency recovery use strict reads; a transient failure stops the incarnation and leaves both snapshot and timer state unchanged until a bounded retry can replay the full tail.
- **Incomplete journal tail.** Replay validates contiguous sequences through the head captured by the same store operation, so a successful-looking prefix cannot publish stale state or authorize snapshot repair.
- **Concurrent legacy repair.** Replacement compares both the loaded sequence and exact payload in the backend transaction/script, so only the first replica can claim a legacy boundary.
- **Timed-entity startup volume.** Bounded to entity types with declared liveness obligations; non-timed entities retain index-only lazy hydration. A future durable scheduler may avoid one resident actor per persisted instance of a timeout-declaring type.
- **Duplicate timers during startup races.** Mitigated by atomic initial-sequence reservation and the existing fire-time sequence/state checks.
- **Runtime task nondeterminism.** The task coordinates production actor readiness only; state mutation remains actor-serialized, time comes from `sim_now()`, and deterministic tests use a logical clock plus paused Tokio time.

### DST Compliance

- The restart regression uses `SimEventStore`, deterministic IDs, a logical `sim_now()` clock, and paused Tokio time.
- Deterministic truncation faults retain the pre-fault journal head, proving actor hydration rejects an incomplete prefix.
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
6. **Create a first snapshot at a recovered no-anchor journal head** — Rejected because partial replay can skip durable envelopes; sealing the resulting state behind a new boundary would make those facts unreplayable.
7. **Read the journal head in a second call** — Rejected because an append between the tail and head queries would make their completeness relationship ambiguous; the proof must come from one logical store snapshot.

## Rollback Policy

Remove the post-spawn readiness task and explicit hydration cause, and restore `populate_index_from_store` to index-only behavior for every entity type. The snapshot anchor is an optional JSON field; older code ignores unknown fields, so rollback remains code-only and existing event histories and snapshots remain readable.
