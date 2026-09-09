# ADR-0157: Journaled PATCH/PUT Field Updates

## Status

Accepted (2026-07-12)

## Context

OData PATCH and PUT reach the entity actor as `EntityMsg::UpdateFields`, which
mutated `state.fields` in memory and replied success without appending anything
to the event journal (ARN-189). Entity state is rebuilt exclusively from the
journal (snapshot + event replay) on actor eviction, server restart, and query
projection backfill — so every PATCH/PUT was silently lost the moment any of
those ran. The adjacent `Delete` handler already journals fail-closed
(persist first, mutate on success), making the gap an inconsistency rather
than a design choice.

## Decision

1. **Two new journal event types**, emitted by the `UpdateFields` handler:
   `FieldsUpdated` (PATCH merge) and `FieldsReplaced` (PUT replacement), with
   the update payload carried in `params` and `from_status == to_status`.
   They live outside the spec's action vocabulary, like the existing
   `Deleted` event.
2. **Fail-closed acknowledgment.** The handler applies the update, appends
   the event (co-committing key/vector index rows derived from the NEW
   fields, per ADR-0153/0155), and only then replies success. On append
   failure the in-memory fields are rolled back and the reply is an error —
   an update that is not durable is not acknowledged.
3. **One shared application function.** `apply_field_update` in
   `entity_actor::effects` implements merge/replace semantics (PUT preserves
   `Id` and `Status`) and is called by both the live handler and journal
   replay, so a rehydrated entity reaches exactly the live post-update state.
   Replay handles the two event types explicitly: the generic param-sync path
   can only merge and would resurrect keys a PUT dropped.
4. **Duplicate events are acceptable; conflicting appends fail safe.** The
   real duplicate path is a dispatch-layer ask timeout after a fully
   successful handle: `ask_with_backoff` re-sends `UpdateFields`, and the
   actor appends a second event. Both event types are idempotent in effect
   (replaying the same merge or replacement twice converges), so duplicates
   cost one journal row, never correctness. An actor-level retry after a
   persisted-but-unacknowledged append cannot double-append: every store
   enforces `expected_sequence`, so that retry hits a sequence conflict, which
   is now recovered rather than surfaced (see Consequences).
5. **Field updates consume the event budget.** The handler enforces the same
   `MAX_EVENTS_SINCE_SNAPSHOT` gate as spec actions, rejecting before
   mutating. Without it, sustained PATCH traffic while the snapshot path is
   stalled (queue full, stalled writer, save errors — all soft failures)
   would grow the snapshot replay tail past the budget and make the entity
   permanently unhydratable.

## Consequences

- PATCH/PUT survive actor eviction, server restart, and projection backfill;
  the backfill previously rebuilt projections without the patched fields even
  when the live projection had them.
- Entities without configured persistence keep the previous in-memory-only
  behavior (the append is skipped, as in every other handler).
- `FieldsUpdated`/`FieldsReplaced` are reserved action names, and the reservation
  is **enforced**, not conventional: replay dispatches those names to
  `apply_field_update` before the generic action path, so a spec action of the
  same name would be hijacked on rehydration — its params merged into fields, its
  transition never replayed. The `Action` arm refuses both names.
- Journals written by older builds simply lack the new events; replay of old
  journals is unchanged.
- A sequence conflict on the append (concurrent writer, or a crashed ack whose
  append landed) is **recovered**, not surfaced: the arm rolls back the
  speculative merge, replays to the authoritative sequence, re-applies onto the
  caught-up state, and retries, for the same 1 + 2 attempt budget the `Action`
  arm uses under ADR-0046. Without it a single conflict wedged every later update
  until the actor happened to rehydrate. The refusals checked before the first
  attempt — deletion and the event budget — are rechecked after each replay,
  because the race may have deleted the entity or spent the budget. Beyond the
  retry budget the arm fails closed and rolls back to the caught-up state.

  Two properties of that loop are load-bearing and easy to lose:
  - **Catching up rebuilds from a fresh initial state**, as
    `recover_entity_state_from_store` does. `replay_events` applies onto whatever
    state it is handed and never resets it, so replaying onto the live state
    re-applies every event on top of its own effects — the events deque grows,
    `total_event_count` / `events_since_snapshot` climb, and non-idempotent
    effects (counter increments) fire twice. That corruption would be returned to
    the caller, upserted into the query projection, and made durable by the next
    snapshot.
  - **A conflict under a live `expected_precondition` is refused, not retried.**
    That precondition is a compare-and-set: the caller authorized this write
    against one exact digest. A conflict proves the journal held state the actor's
    memory did not, so replaying and committing would apply the write to state the
    caller never saw and Cedar never evaluated. `entity_ops` already caps
    preconditioned asks at a single attempt for the same reason.

- A field-update event whose payload no longer deserializes is skipped under a
  lenient replay policy (so hydration survives spec evolution) and **fails** under
  a strict one, matching the tombstone and generic-event arms. Strict replay backs
  authoritative state resolution, where identity and authority are read: silently
  dropping a `FieldsReplaced` there could preserve exactly the authority it was
  written to revoke. Lenient skips are counted
  (`temper_entity_field_update_replay_skipped_total`).

- **Commit ambiguity is not resolved.** If the store commits the append but returns
  a generic error (a client timeout after commit), the arm rolls back and reports
  failure while the journal holds the update. A quiet entity then serves
  pre-update fields from memory until it next rehydrates, at which point the
  "failed" write appears. Inherent to a non-idempotent append over an unreliable
  channel; called out so it is not discovered as a surprise.

- **A rolling deploy can transiently mis-replay a PUT.** An older build replaying a
  `FieldsReplaced` event has no arm for it, so it falls to the generic param-sync
  path, which only merges — keys the PUT dropped reappear until a new-build actor
  hydrates the entity.

- **Reserved names are enforced at invocation, not at deployment.** The `Action`
  arm refuses `FieldsUpdated` / `FieldsReplaced`, which stops new collisions. It
  does not help a tenant whose spec already declared such an action and whose
  journal already holds those events; separating them needs a discriminator on the
  event envelope, which is a migration, not a guard.
- ADR numbering: 0156 is used by the concurrently open ARN-179 change
  (`docs/adrs/0156-pg-actor-runtime-effect-vocabulary.md` on PR #370).

## Invariants this arm must keep

Stated as invariants rather than as a changelog, because each one was violated by
at least one implementation of this fix and none of them is obvious from reading
the happy path.

1. **A non-object payload never reaches state or the journal.** `parse_json_body_or_400`
   accepts any valid JSON, so a `PUT` body of `[1,2,3]` arrives here. With
   `replace` it would set `fields` to the array, `canonicalize_entity_fields`
   could not restore `Id`/`Status` (no object to insert into), and the append
   would co-commit zero key and zero vector rows — purging the entity's index.
   Journaling is what makes that permanent, so the guard is a precondition of
   this decision, not an extra.
2. **Live and replay run the same transformation.** `apply_field_update` is
   shared, sanitizes runtime-owned fields, and canonicalizes identity/lifecycle.
   Any step applied on only one of the two paths silently rewrites the entity at
   the next rehydration. It returns whether it applied, so a caller cannot treat
   a declined update as a successful one.
3. **The event is sanitized before it is written**, not only on the way into
   state, so the journal never records a second claimed truth for identity or
   lifecycle.
4. **Catching up after a conflict rebuilds from a fresh initial state.**
   `replay_events` applies onto whatever state it is given and never resets it.
5. **A conflict under a live `expected_precondition` is refused, not retried.**
6. **Every refusal checked before the first attempt is rechecked after a replay** —
   deletion and the event budget — because the race may have caused either.
7. **A dropped update is counted, never silent** — malformed payload or
   non-object payload, under a lenient policy; under a strict policy it fails.

## Alternatives Considered

- **Journal a synthetic spec action.** Would push PATCH/PUT through guard
  evaluation and effect derivation that field updates deliberately bypass,
  and would collide with real spec vocabularies.
- **Snapshot-only durability.** Snapshots are throttled (`maybe_save_snapshot`)
  and best-effort; relying on them would leave a loss window and break the
  journal-as-source-of-truth invariant that replay and backfill assume.
