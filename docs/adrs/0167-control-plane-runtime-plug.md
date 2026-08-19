# ADR-0167: Control-plane runtime plug trait

- Status: Accepted
- Date: 2026-08-18
- Deciders: Temper core maintainers
- Related:
  - ADR-0166: Shared `apply_effects` lives in `temper-jit`
  - ARN-351 (kernel hygiene / pluggability)
  - `crates/temper-runtime/src/plug.rs`
  - `crates/temper-server/src/entity_actor/in_process.rs`

## Context

The control plane (OData, Cedar, registry) talks to the default runtime by
constructing `EntityMsg` and holding `ActorRef<EntityMsg>`. A later Durable
Object or the existing Postgres path cannot sit behind that door without
depending on HTTP types or growing a second dispatch.

This is the *who do I call* seam. It is not the *what is the mailbox* seam.
`temper-actor-runtime` remaining a second actor system is a later job.

## Decision

### Sub-Decision 1: The plug lives in `temper-runtime`

`EntityRuntime` and `RuntimeRequest` are defined in `temper-runtime`. The
control plane calls `execute(request, timeout)`. It does not name `EntityMsg`.

**Why this approach**: A Postgres or Durable Object adapter can implement the
trait without depending on `temper-server`. Defining the trait in the server
would invert the seam.

### Sub-Decision 2: Default impl is a newtype, not a move of EntityActor

`InProcessEntityRuntime` wraps `ActorRef<EntityMsg>` and maps
`RuntimeRequest` to `EntityMsg`. EntityActor, persist, blob overflow, and
observe stay in `temper-server`.

**Why this approach**: The actor is a guest on the host. Moving it into
`temper-runtime` would mix the actor model with the entity machine — the
shape that made `temper-actor-runtime` a second, thinner interpreter.

### Sub-Decision 3: Retry stays on the control plane

ADR-0048 retry wraps `EntityRuntime::execute`. Each attempt is one plug call
with a timeout. Runtimes do not own backoff.

**Why this approach**: Retry policy is a dispatch concern. The plug is "do
this request in this budget."

## Rollout Plan

1. **This change** — trait + in-process impl + server dispatch/entity_ops
   call the plug. Retry wraps `EntityRuntime::execute`.
2. **Later** — Postgres path implements `EntityRuntime`. Host drift
   (second mailbox/scheduler) is a separate peel.

## Consequences

### Positive
- HTTP stops naming the mailbox message.
- A second runtime can implement the same door.

### Negative
- The Postgres path is not yet behind the door. Side entrance remains.

### Risks
- A thin impl can still sit behind the door and be incomplete. Fail closed
  on leftover work (schedule/spawn) stays required.

### DST Compliance
- `RuntimeRequest` uses `BTreeMap` for cross-entity booleans.
- Retry jitter is still derived from `sim_now()` (unchanged).
- No wall clock inside the trait.

## Non-Goals

- Moving EntityActor into `temper-runtime`.
- Making `temper-actor-runtime` implement `temper-runtime`'s `Actor` trait.
- A new crate name.

## Alternatives Considered

1. **Define the trait in `temper-server`** — Postgres would depend on HTTP.
   Rejected.
2. **Move EntityActor into `temper-runtime`** — mixes host and entity
   machine. Rejected.
3. **Move `EntityState` / `EntityMsg` into `temper-runtime`** — unnecessary
   for the door. The request vocabulary is enough. Rejected for this peel.
