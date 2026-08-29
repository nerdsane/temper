# ADR-0156: Postgres Actor Runtime Effect Vocabulary

## Status

Accepted (2026-07-12)

## Context

`SpecDrivenActor::apply_effect` in `temper-actor-runtime` matched eight of the
sixteen `temper_jit::table::Effect` variants and dropped the rest through a
`_ => debug!` catch-all (ARN-179). A spec whose transition appended to a list
or set a counter from an action parameter had that effect silently discarded,
so any later guard reading that variable (`list_length_min`, `counter_min`)
evaluated stale state and mis-gated transitions.

A second copy of the vocabulary decision lived in
`temper-cli/src/serve/actor_runtime.rs`, which rejected specs by matching
spec-level effect types before the actors were built. The two lists disagreed:
the CLI rejected `trigger` effects that the actor executes, and both lists
could drift from the `Effect` enum without any compile-time signal — the same
parallel-interpreter drift mechanism tracked in ARN-212.

## Decision

1. **The crate is the single source of truth for its effect vocabulary.**
   `SpecDrivenActor::from_automaton` (and `from_ioa`) validate the compiled
   `TransitionTable` at construction via `validate_effect_support` and return
   an error for any effect the runtime cannot execute. The serve wiring in
   `temper-cli` no longer duplicates the effect check; it keeps only its
   integration/action-trigger checks, which concern serve-time actor wiring
   rather than effect execution.
2. **Exhaustive matches, no catch-alls.** Both `validate_effect_support` and
   `apply_effect` match every `Effect` variant explicitly, so adding a variant
   to `temper-jit` fails compilation in this crate and forces a support
   decision. Runtime arms for construction-rejected variants fail the
   activation loudly instead of dropping the effect.
3. **Implemented:** `ListAppend`, `ListRemoveAt`, `IncrementCounterByParam`,
   `DecrementCounterByParam`, `SetCounterFromParam` — pure state mutations,
   with semantics mirrored from the canonical executor
   (`temper-server::entity_actor::effects::apply_effects`): list values are
   read from the param keyed by the list variable name, removal indices from
   `{var}_index`, counter deltas accept numbers or numeric strings and default
   to 0, and `set_counter_from_param` requires a non-negative integer.
4. **Rejected at construction:** `ScheduleAction`, `ScheduleAtAction`,
   `SpawnEntity` — the runtime addresses actors by `(namespace, actor_type)`
   with no per-entity spawning, and its mailbox has no delayed delivery, so
   these cannot be executed faithfully. `Custom` trigger effects are rejected
   when no reaction routing entry exists for them, since an unrouted trigger
   would otherwise be a silent no-op.

## Consequences

- Specs using param-driven list/counter effects now work under
  `--actor-runtime postgres`; previously the CLI rejected them and the actor
  would have dropped them.
- Specs using schedule/spawn effects fail at startup with an actionable error
  instead of mis-executing at runtime. Supporting them later means adding
  delayed-visibility delivery and per-entity addressing to the runtime, then
  moving the variants to the implemented set.
- `from_automaton` changed from infallible to `Result`; its only external
  callers already handled `Result` from `from_ioa`.
- The full unification of effect interpreters across backends remains ARN-212;
  this ADR fixes the Postgres backend's vocabulary and removes one of the two
  drifting copies.

## Alternatives Considered

- **Add the missing match arms only.** Leaves the catch-all drift mechanism
  intact; the next `Effect` variant would silently drop again.
- **Reuse `temper-server`'s executor directly.** The dependency points the
  other way (`temper-server` depends on `temper-actor-runtime`), and the
  executors operate on different state shapes. Extracting a shared executor
  crate is the ARN-212 track, deliberately not folded into this fix.
