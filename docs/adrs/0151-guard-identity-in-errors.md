# ADR-0151: Guard identity carried in transition-rejection errors

- Status: Proposed
- Date: 2026-06-22
- Deciders: Temper core maintainers
- Related:
  - `crates/temper-jit/src/table/guard.rs` (`Guard`, `GuardFailure`, `check_detailed`)
  - `crates/temper-jit/src/table/types.rs` (`TransitionResult.guard_failure`)
  - `crates/temper-jit/src/table/evaluate.rs` (rejection arm populates `guard_failure`)
  - `crates/temper-server/src/entity_actor/effects.rs` (renders the agent-facing error string)

## Context

When an action is rejected because its guard fails, the server returns a generic
string: `Action 'X' not valid from state 'Y'`. That string is identical whether
the rejection came from a from-state miss (the action does not transition from
the current status at all) or from a guard that did not hold (the action *could*
fire from this status, but a precondition — a counter floor, a required boolean,
a cross-entity status — was not met).

An in-session agent that receives this string cannot tell *what to fix*. It
cannot distinguish "you called the wrong action for this state" from "you need
to set `landing_file_id` to a Ready file first". The self-heal loop that the rest
of the kernel floor rests on needs the rejection to name the specific sub-guard
that failed, the field/ref it read, and the required-vs-found values where the
guard makes them available.

## Decision

### Sub-Decision 1: `Guard::check_detailed` — additive sibling of `check`

`Guard` gains `check_detailed(current_state, ctx) -> Option<GuardFailure>`. It
returns `None` when the guard passes and `Some(GuardFailure)` naming the first
failing sub-guard otherwise. For `And`, it recurses in source order and returns
the first failing conjunct — the same short-circuit order `check` evaluates in —
so the named failure is the one a reader would hit first.

`check` (the bare-bool fast path) is kept and is **not** reimplemented in terms
of `check_detailed`. The hot evaluation path stays a single boolean walk with no
`String`/`Option` allocation; `check_detailed` only runs on the cold rejection
path. Both walk the same guard tree with the same per-variant predicates, so
they cannot disagree on whether a guard holds.

**Why this approach**: the rejection path is rare relative to successful
dispatch; paying for failure identity only when a guard actually fails keeps the
common case allocation-free while still giving the agent a precise contract.

### Sub-Decision 2: `GuardFailure` shape

```rust
pub struct GuardFailure {
    pub kind: GuardFailureKind, // e.g. CounterMin, BoolTrue, CrossEntityState
    pub var: Option<String>,    // the field/counter/bool/ref the guard read
    pub required: Option<String>,   // required-vs-found, where the guard has it
    pub found: Option<String>,
}
```

`GuardFailure` derives `Debug, Clone, PartialEq, Eq, Serialize, Deserialize`.
`PartialEq`/`Eq` are required because `TransitionResult` derives `PartialEq` and
the shadow-execution check (`shadow.rs`) compares two `TransitionResult`s for
full-struct equality. `Serialize`/`Deserialize` use only `serde`, which
`temper-jit` already depends on — **no `temper-verify` dependency is
introduced** (the dependency-discipline rule that the kernel floor rests on).

### Sub-Decision 3: `TransitionResult.guard_failure`

`TransitionResult` gains `guard_failure: Option<GuardFailure>`, populated only
on guard rejection (a rule matched by name *and* state, but its guard did not
hold). A from-state miss and a success both leave it `None`. This is purely
additive: existing readers that look at `.success` / `.new_state` are unchanged.

### Sub-Decision 4: server renders the specific error string

`entity_actor/effects.rs` distinguishes the two rejection cases by reading
`guard_failure`:

- `Some(failure)` → a specific string, e.g.
  `Action 'SubmitForReview' blocked from state 'Draft': guard cross_entity_state on 'landing_file_id' requires status in [Ready,Locked], found <missing>`.
- `None` → the existing generic `Action 'X' not valid from state 'Y'` (a genuine
  from-state miss carries no sub-guard to name).

The rendered string is the agent-facing self-heal contract.

## Consequences

- `crates/temper-jit/src/table/types.rs` exceeded 500 lines once `check_detailed`
  and `GuardFailure` were added, so `Guard`, `GuardFailure`, `EvalContext` and
  the guard `impl` blocks move to a new `crates/temper-jit/src/table/guard.rs`
  (the 500-line file-split rule). `types.rs` keeps `TransitionTable`,
  `TransitionResult`, the rule/effect/metadata types.
- The change is behavior-preserving for every existing caller: `check` is
  untouched, `evaluate_ctx` returns the same `success`/`new_state`, and the new
  field is `None` on all paths except guard rejection.
