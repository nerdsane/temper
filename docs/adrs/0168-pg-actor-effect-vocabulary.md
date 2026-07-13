# ADR-0168: Full Effect vocabulary for the PG actor-runtime backend (ARN-179)

- Status: Accepted
- Date: 2026-07-13
- Deciders: Temper core maintainers
- Related:
  - ARN-179: Postgres actor-runtime silently drops half the Effect vocabulary
  - `crates/temper-actor-runtime/src/spec_actor.rs`
  - `crates/temper-server/src/entity_actor/effects.rs` (reference implementation)
  - ARN-26 (PG-backed actor runtime lineage)

## Context

`SpecDrivenActor::apply_effect` only handled a subset of `temper_jit::table::Effect`
variants. Everything else fell through `_ => debug!("unhandled effect")` and was
silently dropped. Specs that use `list_append`, counter-from-param, or related
effects therefore left durable actor state stale; later guards (e.g. list length)
mis-gated transitions.

This backend is selected when `TEMPER_ACTOR_RUNTIME=postgres`.

## Decision

### Sub-Decision 1: Implement all pure state-mutation effects

Apply the same semantics as the entity-actor path for:

- `ListAppend` / `ListRemoveAt` (values from action params, stored in `fields`)
- `IncrementCounterByParam` / `DecrementCounterByParam`
- `SetCounterFromParam`

These only mutate `SpecActorState` and need no timer or cross-entity spawn plumbing.

### Sub-Decision 2: Reject schedule/spawn at construction (fail closed)

`ScheduleAction`, `ScheduleAtAction`, and `SpawnEntity` require timer / spawn
pipelines the PG actor-runtime does not yet wire the same way as entity dispatch.
Rather than silently drop them, `SpecDrivenActor::from_ioa` / `from_automaton`
**refuse to construct** when the compiled table contains those effects, with an
actionable error naming the action and effect type.

### Sub-Decision 3: Exhaustive match (no silent catch-all)

`apply_effect` matches every `Effect` variant. Schedule/spawn arms are
`unreachable!` after construction rejection (defense in depth if a table is
built without the constructor).

## Consequences

### Positive

- List/counter effects are durable for the PG actor path.
- Specs that need schedule/spawn fail at startup instead of corrupting behavior.
- New Effect variants fail compilation until a support decision is made.

### Negative

- Specs that schedule or spawn cannot run on the PG actor-runtime until that
  plumbing is added (follow-up).

## Non-Goals

- Full timer / spawn parity with entity-actor (separate effort).
- Changing the JIT `Effect` enum itself.
