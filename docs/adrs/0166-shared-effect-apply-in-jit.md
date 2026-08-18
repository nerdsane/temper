# ADR-0166: Shared `apply_effects` lives in `temper-jit`

- Status: Accepted
- Date: 2026-08-18
- Deciders: Temper core maintainers
- Related:
  - ARN-351 (kernel hygiene / pluggability)
  - `crates/temper-jit/src/apply.rs`
  - `crates/temper-server/src/entity_actor/effects.rs`
  - `crates/temper-actor-runtime/src/spec_actor.rs`
  - `crates/temper-verify/src/model/semantics.rs`

## Context

The portable definition of a Temper entity is the transition table plus what
each `Effect` means when applied. `Effect` already lived in `temper-jit`, but
the only complete interpreter was `temper-server`'s `apply_effects`. The
optional Postgres actor runtime had a second, incomplete match. Verification
had a third (`ModelEffect`). Runtime plugs could not share one meaning of
`ListAppend`.

`temper-jit` must not depend on `temper-server`. Dumping `EntityState`, blob
overflow, or HTTP types into jit would invert the seam.

## Decision

### Sub-Decision 1: One apply function in jit

`temper-jit::apply::apply_effects` is the definition of every `Effect` variant.
It mutates an [`EffectTarget`] and returns side-effect work (emit, custom,
schedule, spawn, schedule-at). Adapters run that work.

**Why this approach**: The table and the apply belong together. A Durable Object
or sim runtime can implement `EffectTarget` without importing the HTTP server.

### Sub-Decision 2: State is a trait, not `EntityState`

`EffectTarget` covers status, counters, booleans, lists, and string fields.
`EntityState` implements it (including legacy `item_count` sync).
`SpecActorState` and `TemperModelState` implement it. Blob overflow, event log,
and idempotency stay on the server.

**Why this approach**: Those concerns are persistence and actor bookkeeping, not
the meaning of an effect.

### Sub-Decision 3: Adapters keep routing

Emit and custom are collected names. The default EntityActor logs them and
hands custom names to post-transition hooks. The Postgres adapter `tell()`s
routed actors. Schedule and spawn stay returned work; a runtime that cannot
dispatch them must fail closed *before* apply, not after a half-written spawn.

## Rollout Plan

1. **This change** — jit apply + server wrapper + Postgres adapter uses the same
   function for portable mutations.
2. **Follow-up (done)** — verification calls shared apply. `ModelEffect` is an
   alias for jit `Effect`. List values stay symbolic (`AddItem#1`) as params
   the cascade supplies; mutation is not a second interpreter.
3. **Later** — split `temper-server` only after apply is one function.

## Consequences

### Positive
- Runtime plugs share one meaning of every effect.
- The server no longer owns the definition contract.

### Negative
- `EntityState`, `SpecActorState`, and `TemperModelState` must stay aligned
  with the trait.

### DST Compliance
- Spawn IDs use `sim_uuid()`, not `Uuid::new_v4()`.
- No `HashMap` in the apply path.

## Non-Goals

- Splitting `temper-server` into registry / HTTP / entity crates.
- Changing evolution.

## Alternatives Considered

1. **New `temper-apply` crate** — Extra crate for one function. Rejected; the
   table already lives in jit.
2. **Move `EntityState` into jit** — Pulls event log, snapshots, and blob
   overflow into the definition crate. Rejected.
3. **Keep apply in server and have plugs depend on server** — Inverts the
   control-plane / runtime seam. Rejected.
