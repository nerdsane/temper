# ADR-0058: Query-Plane Hot-Field Opt-Out and Stable Projections

- Status: Proposed
- Date: 2026-04-24
- Deciders: Temper core maintainers
- Related:
  - ADR-0168: Optimistic concurrency retry
  - ADR-0056: Durable state timeouts and silent-exit prevention
  - openpaw ADR-0026: durable query plane and bounded actor residency
  - `crates/temper-spec/src/automaton/types.rs`
  - `crates/temper-server/src/state/mod.rs`
  - `crates/temper-store-turso/src/store/field_index.rs`

## Context

The April 23 quality-review investigation showed that some session writes were paying too much query-plane cost for too little operational value.

The worst offenders were hot fields that change frequently during healthy execution:

- liveness timestamps
- progress timestamps
- monotonic progress counters

Those fields are useful on the entity itself, but they are poor candidates for durable collection filtering. Re-indexing them on every heartbeat/progress write caused avoidable `entity_field_index` churn.

At the same time, the durable query plane had no stable notion of "the projection did not actually change." Even when the indexed view was identical, Temper still rebuilt the field rows.

We need a platform-level way to declare that a state field should not participate in durable query indexing, plus a store-level way to cheaply detect no-op projection updates.

## Decision

Temper adds a selective query-plane participation flag to state variables and uses projection hashes to avoid rebuilding unchanged field-index rows.

### Sub-Decision 1: state variables may opt out of durable query indexing

`[[state]]` declarations may now set:

```toml
query_indexed = false
```

When present, the field remains part of entity state and snapshots, but it is omitted from the durable query projection written to `entity_field_index`.

**Why this approach**: the entity remains fully expressive while the query plane stays focused on fields that are useful for collection discovery and filtering.

### Sub-Decision 2: the durable query catalog stores a projection hash

`entity_catalog` now stores a `projection_hash` derived from:

- projected status
- projected indexed fields

If a write would produce the same projected view, Temper updates the catalog metadata (`status`, `updated_at`, `sequence_nr`, `projection_hash`) without deleting and reinserting the field-index rows.

**Why this approach**: it preserves correct sequence tracking while removing unnecessary index churn for no-op projection updates.

### Sub-Decision 3: projection filtering happens before store writes

Temper computes the query projection from the automaton definition and excludes any state vars marked `query_indexed = false` before calling `upsert_query_projection`.

This keeps the store contract simple: the store receives the already-filtered projection and decides whether it must rewrite the durable index.

**Why this approach**: spec intent belongs at the projection boundary, not in store-specific policy code.

## Consequences

### Positive

- Hot-path heartbeat/progress writes no longer thrash the durable field index when those fields are marked non-queryable.
- No-op projection updates become cheap while `entity_catalog.sequence_nr` still advances correctly.
- Query-plane behavior is explicit in the automaton schema instead of being implied by ad hoc exclusion lists.

### Negative

- Operators cannot filter collections by fields that explicitly opt out of query indexing.
- The query-plane contract is more nuanced: "stored in entity state" no longer always means "indexed for collection queries."

### Risks

- Marking the wrong field `query_indexed = false` could silently remove a useful filter surface. Mitigation: keep the flag opt-in and expose it in spec observation output.
- A bad projection hash could suppress needed field-index updates. Mitigation: hash the exact stored projection shape and cover it with store tests.

## Readiness Gates

- Parser tests prove `query_indexed = false` is accepted and surfaced on state vars.
- Server tests prove excluded fields are absent from the query projection.
- Store tests prove unchanged projections still update `entity_catalog.sequence_nr` without rebuilding field rows.
- A live session E2E proves hot Session fields can be excluded from indexing while the session still advances normally.

## Non-Goals

- Arbitrary per-query dynamic indexing.
- Removing the durable query plane.
- Hiding state fields from entity reads; this only affects durable query indexing.

## Alternatives Considered

1. **Hardcode a Session-only exclusion list in OpenPaw** — Rejected. The capability belongs in Temper because other apps have the same pattern.
2. **Always rewrite the field index and accept the write amplification** — Rejected. The observed workload showed this cost is material.
3. **Move all hot fields out of entity state entirely** — Rejected. These fields are still valuable for entity inspection and state-machine reasoning.

## Rollback Policy

Remove the `query_indexed` filtering in projection generation and ignore `projection_hash` short-circuiting in the store. No entity-state migration is required; rollback only changes durable query-plane behavior.
