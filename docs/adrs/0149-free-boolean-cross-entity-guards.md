# ADR-0149: Free-boolean cross-entity guards in the L1 model check

- Status: Accepted
- Date: 2026-06-22
- Deciders: Temper core maintainers
- Related:
  - `crates/temper-verify/src/model/semantics.rs` (guard evaluation)
  - `crates/temper-verify/src/model/stateright_impl.rs` (`actions`, safety property fns)
  - `crates/temper-verify/src/checker.rs` (dead-transition / reachability BFS)
  - `crates/temper-verify/src/smt.rs` (L0 — unchanged, abstract guard already excluded)

## Context

A cross-entity guard (`ModelGuard::CrossEntityState`) gates a transition on the
status of a *different* entity (e.g. "publish only when the related file entity
is Ready"). The single-entity L1 model only tracks the local entity's state, so
it cannot resolve the related entity's status from local state.

Today `evaluate_guard` lowers `CrossEntityState` to constant `false`
(`semantics.rs`). Because `actions()` calls `evaluate_guard`, a cross-entity-gated
transition is **never offered** during state-space exploration. Consequently the
transition's target state is never visited: it is treated as a dead/unreachable
edge. `find_dead_transitions` then has to special-case cross-entity guards to
avoid reporting them as dead, and any liveness property that needs to reach a
state behind a cross-entity gate (e.g. `Published` reachable) cannot be proven.

This is unsound in the *pessimistic* direction: the environment (the related
entity) *can* satisfy the guard at runtime, so the guarded edge is genuinely
fireable. Lowering it to `false` makes the model strictly smaller than reality
and silently drops every state and property obligation behind the gate.

The opposite fix — lowering to constant `true` — is also unsound, in the
*optimistic* direction: it would make the edge unconditionally enabled, which
(a) proves nothing about the gate, and (b) would break local safety proofs that
hold precisely because the gate can hold the entity back (e.g. a
`no_further_transitions` invariant on a state whose only outgoing edge is
cross-entity-gated, or a `never(Published)` local proof).

## Decision

Treat a cross-entity guard as a **free (uncontrolled) boolean input**: the
model explores *both* the guard-true and the guard-false branch. This is the
sound abstraction of "a value the local model does not control."

### Sub-Decision 1: Separate "may fire" from "is locally enabled"

The constant-`false` arm conflated two distinct questions that a single
`bool`-returning `evaluate_guard` cannot answer correctly at once:

- **Exploration / reachability** ("could this edge ever fire?") — for a
  cross-entity guard the answer is *yes, when the environment cooperates*. The
  edge must be offered so its target state and downstream properties are
  explored.
- **Local enablement** ("is an outgoing transition guaranteed enabled here from
  local state alone?") — used by `no_further_transitions` and `no_deadlock`
  safety checks. For a cross-entity guard the answer is *no, not from local
  state* — the local automaton may legitimately be waiting on the environment.

We keep `evaluate_guard` returning the **local-enablement** answer (`false` for
cross-entity), so the existing safety proofs stay sound, and add a separate
exploration predicate `transition_may_fire` that treats the cross-entity conjunct
as *possibly true*.

### Sub-Decision 2: Free boolean = a branch in `actions()`

Stateright has no first-class "input variable." Nondeterministic environment
inputs are encoded as **branching in the action set**: every action pushed in
`Model::actions()` becomes a distinct successor edge that BFS expands, and every
state where the action is simply *not* taken is also explored (BFS visits a
state's full action set, and visits successor states reached by *other* actions
or by taking nothing further). Therefore:

- **guard-true branch**: `actions()` offers the cross-entity-gated transition
  whenever its status precondition and bounds hold, regardless of the
  cross-entity conjunct. Taking it reaches the target state.
- **guard-false branch**: already covered — BFS also explores every state in
  which that transition is not taken (all other enabled actions, and the
  "stay / wait" state). No extra action is needed for the false branch.

This is exactly the standard demonic-environment encoding: offering the edge
models "the environment may enable it"; the rest of the exploration models "the
environment may not."

### Sub-Decision 3: Reachability BFS mirrors `actions()`

`checker::find_dead_transitions` runs its own BFS using
`is_transition_enabled`. It is updated to use the same "may fire" predicate so
that target states behind cross-entity gates are actually walked. With those
states now reachable, the special-case that *excluded* cross-entity transitions
from the dead-transition report is removed: a cross-entity transition is now
dead only if its status precondition is genuinely never met (a real bug), which
is the correct signal.

## Soundness and blast radius

- **L0 (SMT)** is unchanged: it already encodes a cross-entity guard as a fresh
  free `Bool` const and excludes such transitions from local induction /
  reachability. The free-boolean treatment in L1 now matches L0's intent.
- **Safety properties stay sound.** `evaluate_guard` still returns `false` for
  cross-entity in `no_further_transitions` / `no_deadlock`, so a state whose only
  exit is a cross-entity gate is still (correctly) treated as locally terminal /
  waiting. The `never(State)` family is a `StatusInSet`/`NeverState` check on the
  *visited* state set; making a gated state reachable means it is now actually
  checked, which is the intended strengthening, not a regression.
- **Optimism is bounded to reachability only.** We over-approximate which
  edges *can* fire (good for proving reachability/liveness and for not hiding
  states), and we never under-approximate enablement in a way that would let a
  safety violation slip through: any state newly reached still has *all* its
  invariants checked.

Existing specs with cross-entity guards that must keep passing:
`agent.ioa.toml` (`Complete: Working→Completed`), `Problem.ioa.toml`,
`Analysis.ioa.toml`, `EvolutionDecision.ioa.toml`. For the latter three the
gated edge is the `from=[]` creation edge into the already-initial `Open` state,
so reachability is unchanged. For `agent.ioa.toml`, `Completed` becomes reachable;
its only invariant (`goal != ''`) is `Unverifiable` (string compare) and
generates no property, so no new obligation is introduced. Verified by the
cascade tests below.

## Validation

- New unit tests in `semantics.rs`, `stateright_impl.rs`, `checker.rs` for the
  free-boolean exploration and the unchanged local-enablement semantics.
- `checker.rs` regression test proving a cross-entity-gated target state is now
  reachable-and-proven (not dead).
- `cargo test -p temper-verify` green; full cascade still green on all four
  in-repo cross-entity specs via `temper-platform`.
