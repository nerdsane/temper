# ADR-0191: Actor-Spawn State-Timeout Recovery

- Status: Proposed
- Date: 2026-07-13
- Deciders: Temper core maintainers
- Supersedes: the request-traffic-dependent hydration implementation of ADR-0056 Sub-Decision 1
- Related:
  - ADR-0049: State-entry timeouts and durable scheduler
  - ADR-0050: Mandatory liveness coverage for non-terminal states
  - ADR-0056: Durable state timeouts and silent-exit prevention
  - ADR-0028: Memory-bounded lazy hydration and passivation
  - `crates/temper-runtime/src/actor/actor_ref.rs`
  - `crates/temper-runtime/src/actor/cell.rs`
  - `crates/temper-server/src/state/entity_ops.rs`
  - `crates/temper-server/src/state/dispatch/state_timeouts.rs`

## Context

ADR-0056 requires a timed entity to re-arm its deadline when its actor hydrates after process restart or passivation. The implementation reconstructs elapsed time from durable events, but invokes that logic only from `run_post_dispatch_effects`. A restarted entity therefore has no timer until some unrelated action is dispatched. If no request arrives, its declared liveness transition never fires.

Moving a `ServerState` clone into `EntityActor::pre_start` would let the actor call the scheduler directly, but it would also create a strong ownership cycle: `ServerState` owns the actor system and actor registry, while each actor would retain `ServerState`. Directly sending the timeout action to the entity actor is also incorrect because it bypasses the server dispatch path and its authorization, reactions, telemetry, and subsequent timeout arming.

The durable facts are the entity event history and the snapshot of the state rebuilt from it. The missing operations are a lifecycle trigger that asks the existing timeout scheduler to reconcile as soon as a new actor becomes ready, plus a snapshot-carried timeout clock anchor for histories whose entry/reset event is older than the hot replay tail.

## Decision

### Sub-Decision 1: Every new actor spawn schedules timeout reconciliation

Before `ServerState` publishes any newly spawned actor in its registry, `ActorSystem::spawn_with_first_ask` synchronously admits one `GetState` ask to the fresh actor's empty mailbox. The method returns both the `ActorRef` and a `PendingAsk` reply handle only after that admission succeeds. Callers may enqueue later application or lifecycle traffic while `pre_start` is still pending, but FIFO mailbox order guarantees that `ActorCell` processes the hydration read before that traffic. A reconciliation task awaits the already-admitted reply, then calls the state-timeout scheduler in explicit hydration mode. The timeout decision is made from the current table after that ordered read, rather than sampled before actor startup, so a live table swap cannot make admission stale.

The first ask consumes exactly one slot in the actor's fixed mailbox budget. It has no independent response timeout: its reply channel is coupled to the same startup lifecycle, so a slow but successful `pre_start` retains the reconciliation, while permanent startup failure drops the channel and reports `ActorError::Stopped`. No speculative asks or retry messages are queued. The timeout clock observation is captured before awaiting the reply so startup and the first state read remain charged against the original durable deadline. Untimed actors complete the same ordered lifecycle handshake and then require no timer.

This hook applies uniformly to:

- eager server-start hydration;
- lazy loading of durable entities;
- respawn after idle passivation; and
- first creation of an entity whose initial state declares a timeout.

The task owns a temporary `ServerState` clone only until the actor replies to its first message, permanently stops, or remains inside the same unresolved `pre_start` call. `EntityActor` does not retain `ServerState`, so no ownership cycle is introduced. An unresolved startup cannot safely schedule a domain timeout because it has not established a trustworthy current state; the companion reconciliation task adds one reply receiver and no repeated work while it waits on that same lifecycle boundary.

**Why this approach:** actor spawn is the earliest common lifecycle boundary shared by every hydration path. Request handlers are incomplete because an entity may receive no traffic, while actor `pre_start` cannot safely own the server dispatcher.

### Sub-Decision 2: Index-only startup activates timed entities

The default CLI boot path populates the durable entity index without hydrating actors. That optimization remains in place for entity types with no `[[state_timeout]]`. For every persisted entity whose registered spec declares a timeout, index population immediately spawns the actor so Sub-Decision 1 can reconcile its current state and deadline before any request traffic.

**Why this approach:** projection-backed reads do not necessarily spawn an actor, so actor-lifecycle reconciliation alone cannot uphold a no-traffic liveness promise. A timed entity carries an active scheduling obligation and therefore cannot be treated as fully dormant while the scheduler remains in-process. Selective activation preserves lazy startup for non-timed data instead of restoring eager hydration for the whole tenant.

### Sub-Decision 3: Hydration is explicit, not inferred from the last event

The shared timeout-arm implementation accepts an explicit hydration cause. In hydration mode it does not treat the replayed last event as a fresh state entry. Instead it:

Timeout declarations resolve from the same effective runtime source as actor spawn: the tenant registry's current table first, then the legacy single-tenant transition table. Registry precedence is preserved even when its current table has no declarations.

1. confirms that the current state declares a timeout;
2. atomically reserves the initial generation only when the per-process tracker has no live timer for this entity;
3. derives the latest state-entry or `reset_on` timestamp from the snapshot-carried clock anchor and replayed durable tail;
4. arms the remaining budget, or dispatches on the next runtime tick when already overdue; and
5. uses monotonic committed-response ownership plus generation and an actor-atomic state/clock precondition to cancel stale or racing timers.

A concurrent real dispatch may arm or invalidate a timer before the readiness task completes. Atomic reservation makes both orderings safe: hydration does nothing when dispatch armed first, while a later dispatch supersedes an earlier hydration reservation without either path erasing the other path's deadline. Actor events commit in order, but post-dispatch integrations and effects may complete out of order. The tracker therefore accepts an entry, exit, or reset callback only when its committed event order is strictly newer than the last accepted response: journaled actors use `sequence_nr`, while supported no-storage actors use `total_event_count`. One transition advances ownership once: leaving a timed state invalidates its timer, and entering another timed state uses that same generation for the replacement. An older or duplicate callback cannot cancel or replace a newer deadline. A same-state response with an unchanged clock only advances the observed order; a missing or changed clock reconciles and arms the required deadline.

Ownership also records the exact `StateTimeout` declaration. The public live `SwapController::swap` primitive has one fixed infrastructure observer and serializes replacement plus notification, so its published table versions cannot regress under concurrent swaps. A second observer install is rejected rather than replacing the server's liveness signal or growing an unbounded callback list. Registry registration and direct documented swaps therefore cannot diverge. Armed tasks select between their absolute deadline and the ordered per-type version signal.

One server-wide consumer also receives the same successful-swap fact through a bounded channel. It subscribes before publishing readiness and begins with a current-table sweep, so a swap before startup or in the subscription window cannot disappear. When the current table has any liveness obligation, the consumer performs a fresh authoritative typed store scan for that version rather than trusting the query index's historical hydration bit. It then sequentially materializes every current durable instance. The ordinary actor-readiness barrier reconciles each current state, including fully ready, out-of-band-created, or passivated entities whose prior untimed table had no timer task to receive a per-type signal. New entities created concurrently still reconcile through their normal spawn or post-dispatch path.

Each tenant/type has one coalesced retry owner keyed in deterministic `BTreeMap` order. A failed store enumeration or actor materialization retains that obligation and retries with capped deterministic exponential backoff until it completes or a newer table version supersedes it. Controller-local versions are not ordered across type lifetimes: if a queued target is superseded, the worker replaces it from the live registry, so removal and re-addition at version 1 cannot lose the replacement sweep. Broadcast lag discards the uncertain pending view and rebuilds it from every currently timed tenant/type. The worker's template retains neither the live registry sender, timeout tracker, nor server-lifetime token; a weak server-lifetime check and shutdown signal end the task even when an external registry or detached public controller outlives `ServerState`.

If an armed declaration changed or disappeared, its task reads the actor's current state and reconciles at the same durable event order: known removal advances cancellation ownership immediately, while replacement recomputes the new declaration's deadline from the existing durable clock anchor. A transient actor-state read cannot consume the one-shot version signal: the same owner retries reconciliation with deterministic bounded backoff until ownership advances, the declaration changes again, or removal is observed. Every exit compares against the task's captured generation, so concurrent reconciliation cannot leave a stale task or pending count resident. A table change that leaves the declaration identical does not disturb the timer. The exact declaration is also carried into the actor-atomic precondition. After an optimistic-concurrency conflict, the actor captures one live table snapshot before authoritative replay and uses that same snapshot for replay, exact-declaration validation, transition evaluation, and committed timeout-clock metadata. An obsolete action can therefore neither commit nor retry forever, and a changed replay effect cannot produce mixed-version state even if a version notification races the deadline.

The readiness task captures both the deterministic event-clock observation and a Tokio monotonic-time anchor before it is spawned. Reconciliation advances the observation by the measured readiness interval and compares that instant directly with the durable anchor. This charges only time after the durable entry/reset event, including when first creation persists `Created` after the observation. Normal post-dispatch entry and `reset_on` arming computes against that same anchor, so persistence and earlier post-dispatch work cannot grant a new relative budget or make a restart change the deadline. Every timer carries its exact declaration, target state, timestamp, and reset version through the complete server dispatch path. The entity actor checks that condition in the same mailbox turn as transition evaluation, then checks it again after any optimistic-concurrency replay before retrying the transition. A replay-discovered remote reset returns a benign cancellation response that reconciles only its replacement deadline and skips ordinary post-dispatch effects. If a newer reset committed while its post-dispatch callback was still delayed, the version mismatch suppresses the stale fire even when logical timestamps and state are unchanged. The scheduler creates one absolute Tokio deadline before spawning the timer task and uses `sleep_until`, so task queueing cannot move the deadline later. Paused Tokio time keeps DST replay exact.

Timer delivery always executes as the named internal `state-timeout-hydration` service principal, whether armed from a live action or restart hydration. Live arming inherits only non-authority observation fields (session, trace, intent, workflow root, and observation metadata) from the initiating request. The timer creates its own stable idempotency key and never inherits caller authority, so restart cannot change Cedar authorization or the authority of reactions that inherit the timeout dispatch context.

**Why this approach:** the existing code inferred hydration from “same state plus tracker sequence zero.” That inference works only after traffic arrives and can misclassify the replayed entry event as a brand-new entry with a full budget.

### Sub-Decision 4: Startup reconciliation is ordered, observable, and recoverable

The first mailbox ask is the startup reconciliation barrier. Actor startup and optimistic-concurrency recovery treat every journal-tail read as strict: a read failure follows supervision and eventually drops the ask reply and closes the mailbox if the incarnation cannot start. The server logs that failure with tenant, entity type, and entity ID; it does not invent state or arm a timer from incomplete replay. An actor that permanently fails startup closes its mailbox; the entity registry treats only an open mailbox as a live incarnation. The next access atomically replaces a stopped registry entry under the existing spawn lock, re-runs hydration, and schedules timeout reconciliation. Concurrent callers still converge on one live actor.

**Why this approach:** readiness asks are not lifecycle probes. Their response budgets can expire while `pre_start` is still making valid progress, leaving a live actor that starts after every probe has been discarded. A readiness notification alone is also insufficient: the actor can consume already-queued restart or application messages before the awakened task admits its state read, recreating the same exhaustion gap. Synchronous first-message admission makes readiness and mailbox ordering one atomic publication contract.

### Sub-Decision 5: Co-commit timeout clock identity with events and snapshots

`TransitionTable` carries the verified `[[state_timeout]]` declarations into the production actor. As each committed event enters a timed state or executes a declared `reset_on` action, the actor updates `state_timeout_clock_reset_at` and a monotonic `state_timeout_clock_reset_version` in the same state mutation. The version distinguishes separate resets that share one deterministic logical timestamp without letting unrelated events cancel the clock. Replayed tail events advance or clear both values alongside state, including tombstones. Periodic and passivation snapshots use one shared encoder that persists the pair while continuing to omit the bounded hot event deque. Hydration therefore recovers the exact clock identity even when the entry/reset event precedes the current snapshot.

Every current `EntityEvent` journal payload also co-commits the clock outcome that results from applying that event under the exact `TransitionTable` used for the successful append. One crate-internal encoder is shared by ordinary actor appends, atomic composite bootstrap/sub-write batches, atomic File initialization, and the data-only create path; no production entity-event writer may emit a structurally current event through a legacy raw serializer. The tagged outcome distinguishes an inactive commit-time declaration from an active clock pair. It is internal to persistence and is removed when the public `EntityEvent` enters bounded response history. The same computed outcome drives both the appended payload and live staged state, including after optimistic-concurrency replay, so pre-crash and post-restart absolute deadlines are identical.

Journal payloads written before this metadata remain readable. Absence of the reserved field alone selects legacy derivation under the current table, even after a current event or snapshot: a rolling deployment or rollback can legitimately let an old writer append after a new writer. That legacy envelope ends the preceding authority run, and the next present current-format event establishes a new checkpoint. This makes current → legacy → current histories and legacy suffixes after current snapshots hydratable without treating malformed present metadata as legacy. Presence remains a current-format durability claim: `null`, malformed shapes, invalid versions, or an incompatible current event fail hydration instead of silently discarding the clock fact, including for tombstones and timeout-free replacement tables. Within each uninterrupted current-format run, the first present event may retain the reset identity established by older history; later outcomes must retain the exact pair, clear it, or establish a new pair at their own envelope sequence. Decreasing versions and changed timestamps under a reused version still fail hydration. Current snapshots carry the equivalent authority marker. Snapshot parsing validates marker and pair structure immediately, while the clock-version upper bound is validated against the atomically captured journal head: a compare-and-replace legacy repair may legitimately checkpoint the head's clock identity in an older snapshot boundary when the intervening tail contains only replay-skipped composite markers.

An inactive historical outcome remains a truthful table-at-commit fact, but it cannot suppress a timeout added later for that target state. Replay deterministically applies the existing declaration-add migration policy to such an event and leaves the result derived until a current event or snapshot checkpoints it. This preserves journal-only untimed-to-timed upgrades without allowing later `reset_on` edits to reinterpret events committed while a timeout did exist.

If actor startup captured an untimed table and a later hot-swap adds a timeout before the actor recorded clock metadata, the first unrelated event repairs the missing timestamp from the retained durable entry and later declared reset events. A retained `reset_on` self-loop is independently trustworthy even when the older entry predates the bounded tail. The repair event supplies the monotonic clock version but does not replace the historical timestamp. Only when the bounded tail contains neither a trustworthy entry nor a same-state reset does the existing conservative one-budget fallback anchor at the current event.

Legacy snapshots that predate either optional clock field and have no matching state-changing event in their replay tail continue to receive one full timeout budget. Before the actor becomes ready or its timer can be armed, hydration synchronously rewrites the loaded snapshot with the conservative anchor and reset version. A missing clock identity after a successful strict replay proves that every post-snapshot envelope was skipped without mutating domain state: every successfully parsed event updates, repairs, or clears the identity. The replacement payload therefore keeps the loaded boundary's sequence and replay-budget fields, while the live actor retains the journal head and counts the skipped envelopes as its replay tail. Replay returns the exact snapshot payload it loaded; a dedicated compare-and-replace operation accepts that payload as the expected value and atomically updates the current snapshot and its same-sequence history record without creating or rotating an event-segment boundary. Two replicas that loaded the same legacy boundary can therefore race, but only one replacement can succeed. Skipped markers replay again on the next restart, now from a snapshot with the durable clock identity. A failed or concurrent upgrade write fails actor startup instead of exposing a refreshable in-memory budget. This compatibility fallback may delay the first deadline after upgrade, but even an immediate second crash and restart reuses the first upgraded identity.

If replay reaches a positive journal sequence but still cannot reconstruct an anchor, hydration requires an existing snapshot boundary for the conservative repair. A journal-only history in that condition may contain only a composite marker or an envelope written by a schema version that a future compatible runtime can replay. Creating a snapshot at the journal head would permanently hide those facts. Startup therefore leaves the journal and snapshot absence unchanged, arms no timeout, and returns an observable actor-start error until a compatible runtime or explicit migration supplies a trustworthy boundary. Journal-only histories with a replayable entry or reset event remain valid because replay reconstructs their exact anchor without creating a boundary. Existing-boundary replacement remains actor-readiness work rather than background maintenance, so stores with priority admission place it ahead of low-priority snapshot traffic.

### Sub-Decision 6: Prove journal-tail completeness against an atomic head

A successful event query does not by itself prove that every committed envelope was returned: a contiguous prefix can look valid while hiding a later transition. The persistence boundary therefore exposes a journal-tail read together with the durable head sequence captured from the same logical store snapshot. Postgres and Turso use one common-table-expression query, Redis uses one Lua script over the sequence key and event list, and Sim captures both values under its deterministic store lock before applying read-truncation faults.

Actor replay requires the returned tail to start immediately after the snapshot boundary, remain contiguous, and end exactly at the captured head. Any gap, prefix truncation, or head preceding the snapshot is a hydration error before state can become ready, a timeout can be armed, or a legacy snapshot can be repaired. This completeness check is unconditional; the older lenient-read option applies only to explicit backend errors in observation-oriented callers, never to a read that proves itself structurally incomplete.

**Why this approach:** a second, independent head query would race a concurrent append and could not prove which journal view the tail represented. Capturing the head and tail in one database statement, Redis script, or simulation lock gives every backend the same durable replay contract.

### Sub-Decision 7: Post-commit lifecycle work is cancellation-safe

Composite batches and atomic File initialization commit outside an entity actor's mailbox. Immediately after the journal append returns, the server starts one bounded reconciliation task that owns the complete post-commit lifecycle: establish inactive timeout high-water marks, recover a poisoned actor-registry guard if necessary, drain every captured pre-commit incarnation through a FIFO stop barrier, publish or hydrate the replacement, activate its timeout decision, and repair the query projection. The request awaits that task for an ordinary success or error response, but dropping the request's future detaches rather than cancels the task. There is no await point between observing append success and starting the task.

The generic mailbox drain remains cancelable before its barrier is committed, because speculative lifecycle callers must be able to restore admission. Durability-sensitive callers instead make the *owner task* non-cancelable. A synchronous compatibility eviction only unregisters an incarnation after its nonblocking stop barrier was accepted; a failed reservation leaves the actor and indexes intact, while an accepted barrier receives UID-checked background cleanup after receiver closure. No path may remove a registry entry merely because it attempted to stop the actor.

**Why this approach:** changing every mailbox drain into a non-cancelable operation would leak fences for speculative callers, while letting request cancellation own post-commit cleanup can reopen an actor whose durable state has already changed behind it.

### Sub-Decision 8: Readiness and timeout ownership share one barrier

The first mailbox `GetState` reply remains the sole authoritative startup observation. A per-incarnation readiness record is registered before the actor becomes registry-visible. The hydration task records its measured observation and startup elapsed time, reconciles timeout ownership, and only then completes that readiness record. Any wrapper that promises a readable materialized actor waits for the same record before using a later `GetState` response. A later response may confirm state, but it cannot win ownership first with a zero elapsed interval and turn the measured callback into a duplicate.

**Why this approach:** FIFO orders actor replies, not the executor turns of two separate response consumers. Explicit completion closes that scheduler race without adding another actor message or changing the durable deadline.

### Sub-Decision 9: Terminality and derived query visibility are monotonic

All replay, lazy-load, listing, and DST-oracle paths use one semantic rule: an envelope is terminal when its string `payload.to_status` is `Deleted`, or when its canonical `event_type` is `Deleted` and there is no string target. The first such envelope is authoritative even if a legacy or corrupt tail follows it. A payload action merely named `Deleted` does not make a transition to a live state terminal.

Query-projection removals carry the terminal journal sequence. PostgreSQL and Turso persist that sequence in a projection-tombstone table in the same transaction that removes catalog and field-index rows. Storage triggers suppress an insert or update whose sequence is not newer than the tombstone, so delayed work from another process cannot resurrect query visibility after the deleting worker or process has gone away. The background queue still coalesces by sequence, but correctness does not depend on one process retaining an in-memory high-water mark.

**Why this approach:** physical deletion discards the only fact that can distinguish a delayed older upsert from a legitimate later projection. A durable sequence tombstone preserves that comparison while keeping deleted rows out of query tables.

### Sub-Decision 10: Redis live indexes are co-committed and legacy migration is resumable

New Redis appends update the historical entity set, tenant and typed live sorted sets, tombstone set, and index-version metadata in the same append Lua script. Listing therefore never scans journals for upgraded tenants. Legacy tenants migrate through a Redis-side pending set plus per-entity journal cursors. Each bounded call transfers at most one entity reference and decodes at most a fixed event budget; scalar and null payloads are valid non-target payloads. Until the historical set has been scanned, every pending journal is classified, and an atomic cardinality check confirms every historical reference is represented as live or terminal, the bounded listing returns an explicit retryable migration-incomplete error rather than an authoritative-looking subset.

Completion is revalidated on reads. If a mixed-version writer adds a historical reference without the new indexes, the count mismatch atomically clears completion and restarts the bounded scan. Upgraded appenders converge with migration because both update the same live/tombstone metadata atomically. Segment records remain derived metadata: once the append Lua script commits the journal, a later segment-reconciliation failure is logged for repair but cannot turn the committed append into an acknowledged failure.

**Why this approach:** an unordered legacy set cannot produce the globally first authoritative live IDs without completing classification, and an unbounded `LRANGE` merely moves the memory/latency failure into Redis. Explicit incomplete results preserve both the API truthfulness and a fixed work budget.

### Sub-Decision 11: Touched legacy modules cross the repository boundary now

Every Rust file changed by this effort must remain below the repository's 500-line module budget. Existing large modules touched for timeout, lifecycle, tombstone, projection, or store behavior are split into domain-named child modules; tests are split by behavior. The split is mechanical where possible and retains one production code path.

**Why this approach:** these changes already require reasoning across lifecycle, timeout, and storage boundaries. Leaving new protocol code inside multi-thousand-line files would make later durability review materially less reliable.

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
- A readiness wrapper cannot arm before the first-mailbox hydration callback records its measured elapsed interval.
- Dropping a composite or File request after its durable append cannot reopen admission on the stale incarnation or strand timeout ownership.
- A fallible inline integration cannot return after a timed transition without that durable transition owning its timer.
- A later untimed response advances an already-inactive timeout high-water and rejects an older timed callback.
- Projection deletion at sequence N rejects an upsert from any process at sequence N or below.
- An earlier canonical or legacy tombstone followed by a live-looking corrupt tail remains absent from actor, in-memory index, durable listings, query projection, and DST oracles.
- Each Redis legacy-migration call decodes a bounded journal chunk; bounded listing reports incomplete until global/type results are authoritative.
- A committed Redis append remains successful when derived segment metadata is malformed or temporarily unavailable.
- Two repairs that loaded the same legacy boundary have one winner; the stale writer cannot overwrite that anchor.
- Snapshot-boundary replacement enters the persistence writer as actor-readiness work rather than background maintenance.
- An injected repair failure followed by store recovery in the same server replaces the stopped actor incarnation, persists the anchor, and arms exactly one timer.
- A slow but successful `pre_start` that outlasts the maximum readiness-ask schedule still arms exactly one timer without request traffic, charges startup time against the original deadline, and durably fires the timeout action.
- A bounded queue of restart signals submitted immediately after spawn cannot overtake the first hydration read, exhaust reconciliation, or move the durable timeout deadline.
- A live untimed-to-timed table swap before `pre_start` cannot invalidate startup reconciliation or lose the initial-state deadline.
- A live untimed-to-timed table swap after startup materializes and arms fully ready and passivated durable instances without unrelated traffic.
- The documented direct `swap_controller().swap` path publishes the same timeout-reconciliation notification as tenant registration.
- A post-snapshot untimed-to-timed swap followed by an unrelated same-state event retains the original durable entry deadline.
- Current → legacy → current journal histories and a legacy writer suffix after an authoritative snapshot remain hydratable across mixed-version rollout, rollback, and roll-forward.
- A changed declaration rebinds immediately at the table-version signal and uses the new budget from the existing anchor; a removed declaration cancels and releases pending ownership without waiting for its old deadline.
- A transient actor-state read failure after a table-version signal retries reconciliation and still fires a shorter replacement at its original-anchor deadline.
- A declaration replaced while the old action is in delivery backoff cancels that retry and executes only the replacement action.
- Adding or removing a `reset_on` action followed by abrupt restart before another event or snapshot preserves the table-at-commit absolute deadline and delivers exactly one timeout.
- Optimistic-concurrency retry captures one live table before replay, rejects a timeout declaration removed during the failed append, and reproduces changed replay effects exactly.
- Missing-clock repair uses a retained same-state reset even when the older state-entry event is outside the bounded tail.
- Live and restart-hydrated timeout reactions execute under the same Cedar service principal and produce the same authorized target state.
- Legacy `with_specs` actors arm and durably fire initial-state timeouts without a `SpecRegistry` entry.
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

- Each new actor reserves one mailbox slot for its first `GetState` ask and retains one reply receiver until startup reconciliation completes.
- Persisted timed entities consume actor and timer memory while their liveness obligation is active.
- Timeout recovery is asynchronous with respect to registry insertion, so readiness may briefly precede timer-arm observability.
- Current snapshots and entity-event journal payloads gain internal timeout-clock authority metadata; legacy payloads remain identifiable by absence and safely delimit current-format authority runs during mixed-version operation.
- Hydration uses a head-bearing journal read instead of an event vector alone.
- Legacy snapshot-only entities may receive one conservative full budget after their first upgraded hydration.
- Legacy journal-only timed entities whose replay cannot reconstruct an anchor require a compatible migration before they can hydrate.

### Risks

- **Startup never completes.** The reconciliation task waits on the same unresolved lifecycle boundary and retains one reply receiver, one bounded mailbox slot, and its temporary state handle, but emits no repeated asks. A trustworthy domain timeout cannot be derived until startup establishes current state; permanently failed startup closes the reply and enables stopped-incarnation replacement.
- **First state read fails.** `EntityMsg::GetState` is an in-memory, infallible actor operation after successful startup. A handler or actor stop still propagates through the reply channel and is logged instead of arming from invented state.
- **Legacy snapshot upgrade failure.** The synchronous anchor rewrite fails actor startup; hydration never arms a timer from an anchor that another immediate restart could forget.
- **Missing or unreadable snapshot boundary.** Timeout-anchor repair requires a successfully loaded existing boundary. Missing, unreadable, or otherwise ambiguous boundaries fail actor startup without changing durable history, so a compatible runtime or migration can reconstruct it later.
- **Unreadable journal tail.** Actor hydration and concurrency recovery use strict reads; a transient failure stops the incarnation and leaves both snapshot and timer state unchanged until a bounded retry can replay the full tail.
- **Incomplete journal tail.** Replay validates contiguous sequences through the head captured by the same store operation, so a successful-looking prefix cannot publish stale state or authorize snapshot repair.
- **Concurrent legacy repair.** Replacement compares both the loaded sequence and exact payload in the backend transaction/script, so only the first replica can claim a legacy boundary.
- **Timed-entity startup volume.** Bounded to entity types with declared liveness obligations; non-timed entities retain index-only lazy hydration. A future durable scheduler may avoid one resident actor per persisted instance of a timeout-declaring type.
- **Duplicate timers during startup races.** Mitigated by atomic initial-sequence reservation and the existing fire-time sequence/state checks.
- **Runtime task nondeterminism.** The task coordinates production actor readiness only; state mutation remains actor-serialized, time comes from `sim_now()`, and deterministic tests use a logical clock plus paused Tokio time.
- **Detached post-commit reconciliation.** Exactly one bounded task is started synchronously after each successful out-of-band append. It owns no new domain decision; it completes already-required fencing, actor replacement, timeout activation, and derived projection repair when the initiating request disappears.
- **Legacy Redis writers.** Completion validation detects historical references added without upgraded index metadata and restarts migration. Rolling deployments must still converge writers promptly because an old writer mutating an already-known journal has no legacy global mutation counter; the next full validation/migration pass reclassifies known references before completion is trusted.

### DST Compliance

- The restart regression uses `SimEventStore`, deterministic IDs, a logical `sim_now()` clock, and paused Tokio time.
- Deterministic truncation faults retain the pre-fault journal head, proving actor hydration rejects an incomplete prefix.
- Randomized scenarios are seed-derived and assert the same state and timer outcomes for the same seed.
- No filesystem, network, wall-clock, or random source is introduced into state mutation.
- The readiness `tokio::spawn` awaits one first-mailbox reply and emits no probe loop; it does not execute state-machine mutation outside the actor.

## Non-Goals

- A separate persistent timer table.
- A continuous or cluster-wide scan beyond the existing one-time boot index scan.
- Changing `[[state_timeout]]` syntax or liveness validation.
- Backfilling an exact timer anchor into legacy snapshots that contain no retained entry or same-state reset event.

## Alternatives Considered

1. **Store `ServerState` inside every `EntityActor`** — Rejected because it creates a strong ownership cycle and couples the kernel actor to the HTTP/server aggregate.
2. **Send timeout actions directly to the actor** — Rejected because it bypasses the shared dispatch path and its reactions, telemetry, integration behavior, and follow-on timer arming.
3. **Re-arm only in `hydrate_from_store`** — Rejected because default boot uses index-only startup, while lazy respawn and direct actor creation paths would remain uncovered.
4. **Wait for the next action** — Rejected because that is the current liveness failure.
5. **Build the full durable scheduler now** — Deferred to ADR-0049's longer-term direction; it is broader than the actor-lifecycle regression.
6. **Create a first snapshot at a recovered no-anchor journal head** — Rejected because partial replay can skip durable envelopes; sealing the resulting state behind a new boundary would make those facts unreplayable.
7. **Read the journal head in a second call** — Rejected because an append between the tail and head queries would make their completeness relationship ambiguous; the proof must come from one logical store snapshot.
8. **Publish a readiness watch before submitting `GetState`** — Rejected because readiness and mailbox admission are separate scheduler turns. Traffic queued during slow startup can run first, restart the actor, and exhaust every later hydration ask while the same incarnation remains live.

## Rollback Policy

Remove the post-spawn and table-change readiness tasks and explicit hydration cause, and restore `populate_index_from_store` to index-only behavior for every entity type. Snapshot anchors and event clock outcomes are optional JSON fields; older code ignores unknown fields. A rolled-back writer may therefore append a legacy suffix, which the upgraded reader explicitly supports, so rollback remains code-only and existing event histories and snapshots remain readable.
