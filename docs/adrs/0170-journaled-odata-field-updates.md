# ADR-0170: Journal OData field updates as entity events

- Status: Proposed
- Date: 2026-07-13
- Deciders: Temper core maintainers
- Related:
  - ADR-0046: Optimistic concurrency recovery for entity writes
  - ADR-0153: Declared composite key index
  - ADR-0155: Declared vector access path
  - `crates/temper-server/src/entity_actor/actor.rs`
  - `crates/temper-server/src/state/entity_ops.rs`
  - `crates/temper-server/src/odata/write.rs`
  - `crates/temper-runtime/src/persistence/mod.rs`
  - `crates/temper-store-postgres/src/store.rs`
  - `crates/temper-store-sim/src/lib.rs`

## Context

OData PATCH and PUT reach `EntityMsg::UpdateFields`, which currently mutates an
actor's in-memory `EntityState::fields` and immediately reports success. Unlike
state-machine actions and deletion, that write appends no journal event and does
not advance the snapshot boundary. Actor eviction or process restart therefore
rehydrates the entity without the acknowledged field update. A PATCH-only entity
loses every edit because no later action happens to create durable history.

Persisting only a snapshot would close the immediate restart gap, but it would
make field writes depend on a second persistence mechanism and omit them from the
ordered entity history. Field updates instead need the same durable ordering as
the entity's other writes.

## Decision

### Sub-Decision 1: PATCH and PUT append a reserved field-update event

Each successful `UpdateFields` message appends one entity-journal envelope with a
reserved internal event type and a versioned payload containing:

- the caller's field object; and
- whether the operation is merge (PATCH) or replacement (PUT).

The event retains the entity's current status as both its source and target
status. It is an entity mutation, but not a state-machine transition, so replay
must recognize the reserved event type before transition-table lookup. Keeping
the operation explicit preserves PUT replacement semantics; storing only the
resulting object would erase the caller-visible history of how the state changed.

**Why this approach**: one ordered journal remains the source of truth for
actions, deletion, and direct OData field writes. The actor rejects dispatch of
an automaton action whose name equals the reserved event type, so ordinary action
history cannot enter the field-update replay decoder.

### Sub-Decision 2: build a candidate state and commit it atomically

The actor clones its current state, applies the PATCH or PUT to that candidate,
and passes the candidate to the existing journal append path. This has three
properties:

1. journal append and declared key/vector rows are derived from the updated
   fields, so projections match the committed entity state;
2. the live actor state is replaced only after the append succeeds; and
3. serialization, storage, and optimistic-concurrency failures leave the
   caller's speculative PATCH/PUT fields unpublished and return a failed
   response.

An optimistic-concurrency retry first recovers the latest durable entity state.
That authoritative history becomes the actor's live state even when a later
retry fails or exhausts its budget; only the uncommitted PATCH/PUT fields are
discarded. This prevents a rejected field update from making the actor continue
serving an older view than its own journal.

The co-commit API carries explicit `reconcile_keys` and `reconcile_vectors`
flags. When the entity type declares keys, its candidate `key_rows` are the exact
current set: Postgres and the simulation store delete every prior row for that
entity before inserting the candidate rows. An empty set therefore purges stale
keys after PUT removes key properties or deletion tombstones the entity. Stores
that do not maintain the declared-key index continue to ignore both rows and the
reconciliation flag.

The state layer converts `EntityResponse { success: false }` into its public
error result before OData response mapping. A failed append is therefore a 5xx,
never an HTTP success containing an internally failed response.

After append success, the actor records the event in its bounded recent history
and runs the existing snapshot policy. Field writes consume the same bounded
replay budget as every other entity event.

### Sub-Decision 3: replay applies field updates in journal order

Hydration recognizes the reserved event, validates its versioned payload, and
applies the same PATCH/PUT helper used by the live path. The replayed update is
inserted into bounded recent history and advances the sequence number before the
next envelope is processed. Ordinary action and tombstone replay remain
unchanged.

Malformed field-update history is treated like other schema-incompatible
history: it is logged with the event identity and skipped while replay continues.

## Rollout Plan

The new envelope is forward-readable only by binaries containing this decision's
replay branch. Deployment is therefore a coordinated reader/writer cutover, not a
mixed-version rolling write:

1. **Pre-cutover** — keep the existing binary serving traffic. No field-update
   envelopes exist because the old writer cannot emit them.
2. **Cutover** — stop accepting writes and drain every old server process. Deploy
   the new binary to the full server fleet, then reopen writes only after every
   process reports the new revision healthy.
3. **Post-cutover** — new readers continue to understand all historical action
   and tombstone envelopes and additionally replay field-update envelopes. Once
   the first new envelope is written, do not roll back to a binary without this
   reader.

This arena PR remains unmerged and undeployed; the cutover applies when a
maintainer later chooses to ship it.

## Readiness Gates

- The field-update replay test passes from an empty journal and from history that
  begins with the existing `Created` event.
- PATCH merge and PUT replacement both survive actor replacement without a later
  state-machine action.
- Persistence failure is fail-closed and leaves the actor's live fields and
  sequence unchanged when no newer durable history exists, and the public OData
  request returns a server error.
- Retry exhaustion retains any unrelated durable history recovered after a
  concurrency conflict without publishing the rejected PATCH/PUT fields.
- PUT removal of declared-key properties purges the prior key row atomically in
  Postgres and the deterministic simulation store.
- The reserved event type cannot be dispatched as a domain action.
- The deployment procedure can drain old processes before reopening the OData
  write surface; mixed old/new readers are not permitted after writer enablement.
- Rollback tooling selects a revision containing the field-update reader after
  any new envelope has been committed.
- All simulation, workspace, strict-Clippy, review, and CI gates are green.

## Consequences

### Positive

- An acknowledged PATCH or PUT survives actor eviction and process restart.
- Journal order defines the result when field updates and actions both write an
  entity.
- Declared key/vector projections are committed from the same candidate fields.
- Storage failure cannot leave a successful but non-durable in-memory write.

### Negative

- PATCH and PUT now pay one synchronous journal-append round trip.
- Field updates consume event and replay budgets, as durable mutations should.

### Risks

- A future change to the field-update payload must preserve versioned replay
  semantics. The reserved event type and decoder make that compatibility point
  explicit.

### DST Compliance

- Live application and replay share one deterministic field-update helper.
- Event timestamps and IDs continue to use `sim_now()` and `sim_uuid()`.
- No wall clock, random source, filesystem access, or unordered collection is
  introduced in the simulation-visible server path.
- A `temper-store-sim` regression test proves PATCH-only history survives actor
  replacement and deterministic replay.

## Non-Goals

- Changing OData authorization or request-body validation.
- Converting direct OData writes into user-declared automaton actions.
- Changing the snapshot interval or adding declared-key indexing to backends
  that do not already maintain it.

## Alternatives Considered

1. **Force a snapshot after every field update** — rejected because snapshots
   are derived acceleration state, not the ordered mutation log, and a snapshot
   failure would create a second durability protocol.
2. **Mutate live state, then append and roll back on failure** — rejected because
   candidate-state commit makes fail-closed behavior structural and ensures
   index rows are derived from the new fields without exposing speculative state.
3. **Encode field updates as a declared automaton action** — rejected because
   existing entity specs do not declare PATCH/PUT transitions, and a synthetic
   user action could collide with domain action names or guards.

## Rollback Policy

Before the first field-update event, rollback may restore the old binary. After
cutover, rollback must target a revision that retains the field-update replay
branch, even if it disables new writes. Existing events cannot be discarded or
served by an older reader without reintroducing acknowledged-write loss.
