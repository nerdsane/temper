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
   enforces `expected_sequence`, so that retry fails with a sequence conflict
   until the actor rehydrates — availability degradation, no durability loss,
   the same exposure as the existing `Delete` path.
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
- A spec action literally named `FieldsUpdated`/`FieldsReplaced` would collide
  with the replay dispatch, matching the pre-existing `Deleted` precedent;
  the names are reserved by convention.
- Journals written by older builds simply lack the new events; replay of old
  journals is unchanged.
- A sequence conflict on the append (concurrent writer or a crashed ack)
  wedges further updates with errors until the actor rehydrates with the
  authoritative sequence — the deliberate fail-safe direction; ADR-0046-style
  conflict recovery for this arm is possible follow-up work if it ever shows
  up in metrics.
- ADR numbering: 0156 is used by the concurrently open ARN-179 change
  (`docs/adrs/0156-pg-actor-runtime-effect-vocabulary.md` on PR #370).

## Alternatives Considered

- **Journal a synthetic spec action.** Would push PATCH/PUT through guard
  evaluation and effect derivation that field updates deliberately bypass,
  and would collide with real spec vocabularies.
- **Snapshot-only durability.** Snapshots are throttled (`maybe_save_snapshot`)
  and best-effort; relying on them would leave a loss window and break the
  journal-as-source-of-truth invariant that replay and backfill assume.
