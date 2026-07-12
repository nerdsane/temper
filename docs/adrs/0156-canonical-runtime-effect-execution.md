# ADR-0156: Canonical Runtime Effect Execution

- Status: Proposed
- Date: 2026-07-12
- Deciders: Temper core maintainers
- Related:
  - ADR-0015: Agent OS Cross-Entity Primitives
  - ADR-0012: OAuth2 Enablement, Webhooks, Timers, and Secret Templates
  - ARN-179: PostgreSQL actor runtime drops supported effects
  - ARN-212: Parallel state-machine implementations
  - `crates/temper-jit/src/table/`
  - `crates/temper-server/src/entity_actor/effects.rs`
  - `crates/temper-actor-runtime/src/spec_actor.rs`

## Context

`temper-jit::table::Effect` is the runtime vocabulary generated from verified IOA
specifications. The entity actor in `temper-server` applies every variant through an
exhaustive `apply_effects` match, while `temper-actor-runtime::SpecDrivenActor` has a
second, partial match with a catch-all arm. That catch-all drops list mutation,
parameterized counters, timers, and child spawning while still reporting the transition
as successful. The serialized PostgreSQL state therefore records a transition whose
declared effects never happened, and later guards evaluate stale state after activation
or restart.

The PostgreSQL actor runtime also converts reaction rules into a single-target hash map.
That conversion discards fan-out, wildcard actions, destination-state filters, and
target resolvers already represented by `temper_runtime::reaction::ReactionRegistry`.
Adding match arms to the local interpreter would repair individual symptoms while
preserving both sources of semantic drift.

The broader verifier/JIT/codegen convergence belongs to ARN-212. ARN-179 needs a narrow
runtime decision that makes the two deployed effect paths share exhaustive semantics
without creating a dependency cycle between `temper-server` and
`temper-actor-runtime`.

## Decision

### One exhaustive executor in the shared JIT layer

Move the pure interpretation of `temper_jit::table::Effect` into the JIT table module,
the lowest existing crate that owns the effect type and is already consumed by both
runtime backends. The executor accepts an adapter over status, counters, booleans,
lists, fields, and the legacy `item_count`, then:

1. applies every in-memory mutation;
2. exhaustively matches every `Effect` variant, with no catch-all arm; and
3. returns typed commands for effects that require a runtime driver: emitted events,
   custom triggers, delayed actions, schedule-at requests, and child spawns.

`temper-server::entity_actor::effects` and
`temper-actor-runtime::SpecDrivenActor` must both call this executor. Backend-specific
code may execute returned commands, but it must not reinterpret the effect vocabulary.

**Why this approach**: the `Effect` owner is below both runtimes, so this removes the
duplicate semantic match without introducing a server/runtime cycle or a new crate.
Keeping I/O out of the executor preserves deterministic simulation and makes command
production directly comparable across adapters.

### Runtime commands must be durable or explicitly fail

The PostgreSQL actor runtime will execute returned commands as part of the same actor
activation transaction:

- immediate messages are buffered and inserted into the FIFO mailbox with the state
  update;
- delayed messages are buffered into a separate durable scheduled-message table with a
  delivery timestamp;
- child actor rows are created with the registered handler's initial state, and an
  optional initial action is enqueued atomically; and
- schedule-at reads the named actor field and returns a handler error if it is missing
  or malformed.

The scheduler promotes due rows from the scheduled-message table into the ordinary
mailbox before actor discovery. Promotion is one bounded PostgreSQL statement: a CTE
serializes promoters with a transaction-scoped advisory lock, selects due rows in
`(deliver_at, id)` order, deletes those rows, and inserts their payloads into
`actor_messages` in the same statement and transaction. Serializing promotion prevents
another scheduler worker from skipping an earlier locked timer and assigning a later
timer the lower mailbox ID. A failed insert rolls the deletion back. Only promoted rows
receive ordinary mailbox IDs, so
`last_msg_id` never advances past an ineligible delayed message and existing FIFO cursor
semantics remain unchanged. The promotion batch has an explicit budget and a later poll
continues any remaining due work.

The activation fails instead of committing a successful transition when a declared
runtime command cannot be represented or resolved. A supported effect is never reduced
to a debug log.

**Why this approach**: committing state before a timer or spawn is durable would create
a new split-brain failure mode. Transactional buffering gives the PostgreSQL backend the
same all-or-nothing contract as its existing `tell` path.

### Reactions use the canonical registry without lossy maps

`SpecDrivenActor` stores and queries `ReactionRegistry` rather than a
`HashMap<emit, target>`. For each emitted event or custom trigger it dispatches every
matching exact and wildcard rule whose destination-state filter matches. It resolves
`SameId`, `Field`, `Static`, and `CreateIfMissing` targets according to the canonical
rule type. The map-building helpers and local single-target representation are removed.

**Why this approach**: effect execution cannot be called canonical if the command
driver silently changes routing cardinality or ignores rule conditions. Reusing the
existing registry deletes that second interpreter instead of maintaining it.

## Rollout Plan

This ships atomically in one change:

1. Add cross-adapter behavioral and deterministic regression coverage that fails on
   the current PostgreSQL actor path.
2. Introduce the canonical executor and migrate the server and PostgreSQL adapters in
   the same commit series.
3. Add durable delayed-message and spawn persistence, migrate existing local actor
   tables idempotently, and exercise the live PostgreSQL-backed OData flow.
4. Remove the partial effect match and lossy reaction-map helpers before the PR becomes
   shippable.

There is no compatibility path retaining the partial interpreter.

## Readiness Gates

- The regression demonstrates a list/counter effect surviving serialized state and
  enabling a subsequent guard.
- A conformance test produces the same state mutations and commands through the server
  and PostgreSQL adapters.
- Timer and spawn commands are committed atomically with actor state and are covered by
  PostgreSQL integration tests.
- Reaction fan-out, wildcard/state filtering, and target resolution are covered.
- Determinism guard, DST review, strict Clippy, readability, full workspace tests, live
  local E2E, dedicated PR review, Greptile, and CI all pass.

## Consequences

### Positive

- Adding a new `Effect` variant creates a compile error in the single executor until its
  semantics are defined.
- Production, replay/simulation, and PostgreSQL actor execution share state-mutation
  semantics.
- PostgreSQL transitions no longer acknowledge effects that were not durably recorded.
- Reaction routing retains the complete declared rule set.

### Negative

- The shared executor exposes typed command results even to callers that only consume a
  subset of them.
- PostgreSQL gains a scheduled-message staging table and activation must also flush
  buffered schedules and spawns.

### Risks

- Scheduled-message promotion can starve if its batch budget is too small relative to
  sustained timer volume. Scheduler polls promote oldest-due rows first, expose the
  remaining due count, and integration tests cross the batch boundary.
- Migrating server effect application can change established semantics accidentally.
  Cross-adapter conformance tests pin the current exhaustive server behavior before the
  implementation moves.

### DST Compliance

- The canonical executor is pure apart from deterministic `sim_uuid()` use already
  required by spawn effects.
- It performs no ambient I/O, wall-clock reads, thread creation, or unordered iteration.
- Existing server production, replay, and simulation callers continue to share one
  execution path; the PostgreSQL adapter joins that path.
- No `// determinism-ok` annotation is expected.

## Non-Goals

- Unifying the verifier, model checker, code generator, and JIT transition evaluator;
  ARN-212 owns that wider convergence.
- Changing IOA syntax or inventing new effect variants.
- Changing the non-PostgreSQL server dispatch contract beyond moving its existing
  semantics into the shared executor.

## Alternatives Considered

1. **Add the missing `SpecDrivenActor` match arms** — rejected because every future
   effect could drift again, and the lossy reaction map would remain.
2. **Make `temper-actor-runtime` depend on `temper-server`** — rejected because the
   server already depends on the actor runtime and the dependency cycle would invert
   kernel layering.
3. **Reject every non-scalar effect at PostgreSQL startup** — rejected because it would
   remove working spec capability rather than provide the backend behavior advertised
   by the runtime selector.
4. **Create a new effect-executor crate** — rejected because `temper-jit` already owns
   the typed vocabulary and is the common dependency; another crate adds structure
   without reducing coupling.

## Rollback Policy

Revert the complete change. The additive scheduled-message table may remain harmlessly
on existing databases, but scheduled writes and promotion must be reverted together.
Do not restore the partial interpreter as a fallback.
