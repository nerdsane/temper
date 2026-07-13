# ADR-0169: Journaled PATCH/PUT field updates (ARN-189)

- Status: Accepted
- Date: 2026-07-13
- Deciders: Temper core maintainers
- Related:
  - ARN-189: OData PATCH/PUT field updates never journaled
  - `crates/temper-server/src/entity_actor/actor.rs` (`EntityMsg::UpdateFields`)
  - Delete-path journaling (tombstone fail-closed pattern)
  - ADR-0153 / ADR-0155 (index co-commit on append)

## Context

`EntityMsg::UpdateFields` (OData PATCH merge and PUT replace) mutated
`state.fields` in memory and replied success without appending to the event
journal. Entity state is rebuilt only from snapshot + event replay on
eviction/restart, so field-only edits were silently lost.

The Delete path already journals fail-closed (persist, then mutate). Spec
actions journal + snapshot. Field updates were the inconsistent gap.

## Decision

### Sub-Decision 1: Dedicated journal event types

Emit `FieldsUpdated` (PATCH) and `FieldsReplaced` (PUT) with:

- `from_status == to_status` (status unchanged)
- `params` = the update payload (merge keys for PATCH; full field object for PUT)

These sit outside the IOA action vocabulary (like `Deleted`).

### Sub-Decision 2: Fail-closed acknowledgment

Apply the field update, then `persist_event` (so key/vector index rows co-commit
from the **new** fields). On append failure, roll back in-memory fields and
reply error. Never acknowledge a non-durable update.

### Sub-Decision 3: Replay

`replay_events` recognizes `FieldsUpdated` / `FieldsReplaced` and re-applies via
the same `apply_field_update` helper used live (PUT replace cannot be expressed
by generic action-param merge alone).

### Sub-Decision 4: Event budget

Field updates consume the same `MAX_EVENTS_SINCE_SNAPSHOT` budget as actions;
reject before mutate when exhausted.

## Consequences

### Positive

- PATCH/PUT survive eviction and restart.
- Index co-commit stays correct for field-only writes.
- Fail-closed matches Delete.

### Negative

- Extra journal volume for chatty PATCH clients (mitigated by snapshot interval
  and event budget).

## Non-Goals

- Changing OData HTTP shapes.
- Idempotency keys on field updates (follow-up if needed).
