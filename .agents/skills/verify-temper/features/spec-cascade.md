# Spec verification cascade (L0-L3, five levels)

## Sub-features
The cascade in `crates/temper-verify/src/cascade.rs` (`CascadeLevel`, `run()`). Building the model (IOA parse + `TransitionTable`) is a precondition, not a level.

- **L0 Symbolic** (Z3/SMT, `smt.rs`) - guards satisfiable (no dead guards), invariants inductive, unreachable states flagged.
- **L1 Model Check** (Stateright, exhaustive, `checker.rs` + `model/`) - full state-space exploration, all safety/liveness properties hold, no dead transitions.
- **L2 Simulation** (model-level DST, `simulation.rs`) - multi-seed run with light fault injection; invariants held, no liveness violation, dropped messages accounted.
- **L2b Actor Simulation** (`SimActorSystem`, defined at `cascade.rs` via `with_actor_sim`) - drives the REAL `TransitionTable::evaluate()` through the production dispatch path. **Defined but not wired into the CLI/platform cascade today** - no caller passes `with_actor_sim`, so the CLI runs L0/L1/L2/L3. The actor-level DST coverage comes from the standalone `dst_*` suites instead (see dst-proof.md).
- **L3 Property Tests** (`proptest_gen.rs`) - random action sequences with invariant checking and shrinking to a minimal counterexample.

Multi-entity dirs get a sixth, separate gate: **composite cross-entity verification** (ADR-0150, `temper-verify/src/composite/`, wired at `temper-cli/src/verify/mod.rs`) - joint-composes the entities' machines and BFS-checks `no_dropped_reaction`. It runs after the per-entity cascade in `temper verify` when the dir has >=2 entities; `verify-ioa` (stdin) stays per-entity.

## How to get to it (user POV)
An author changes a `.ioa.toml` and proves the spec is still sound before it can govern anything.

## Driving it
```bash
cargo run -p temper-cli -- verify --specs-dir <dir>   # a DIRECTORY (default "specs"); needs model.csdl.xml + *.ioa.toml
cargo run -p temper-cli -- verify-ioa < entity.ioa.toml   # one spec on stdin; JSON CascadeResult on stdout, exit 1 on any fail
scripts/verify-cascade.sh                                  # every spec dir; results in .cascade-results/
```

## What proves it
Each level prints `[PASS] L0 Symbolic … / L1 Model Check … / L2 Simulation … / L3 Property Tests …`; the run ends `IOA verification cascade: ALL PASSED` (and `Composite cross-entity verification: ALL PASSED` for multi-entity dirs). `CascadeResult.all_passed` is the machine gate. An edit that adds a state or action must show the new element in the pass output. A deliberately broken guard must FAIL a level - if it passes, that is a finding in the verifier, not a success.

## What the cascade does not prove
The cascade reasons about a model, and three things sit outside it. Do not cite a green cascade as evidence about any of them.

- **Fields.** An `[[invariant]]` may not read a field; the parser rejects it and points at `[[field_invariant]]`, which the runtime checks **on writes only**. A field invariant has therefore never been evaluated against any row nothing has written to since it was added. `temper audit` is what evaluates it.
- **Existing rows.** Entities persist across spec edits. Tightening an invariant proves the new spec self-consistent and says nothing about data written under the old one.
- **Dwelling.** `[[state_timeout]]` is a declaration, not a watchdog. An entity whose callback never arrives sits in a transient state forever and every level still passes.

`temper audit --specs-dir <dir> --url <base>` re-checks both invariant kinds against live entities using the runtime's own guard evaluator (`temper_jit::audit`), and adds two derived checks the cascade cannot express: `entity_stranded` (no action enabled in a non-terminal, non-indefinite state) and `transient_state_occupied` (a point-in-time warning; the signal is the *same* entity in the *same* transient state on two runs). Findings are observations - which of the spec or the data is wrong is a judgement.

## Lints
`lint_automaton` runs before verification (`temper-spec/src/automaton/lint.rs`). Beyond `action_missing_to` and the `field_invariant_*` checks, `absorbing_effect` catches a race no level can see: an action that assigns a **literal** to a variable and enters a state whose invariant reads that variable, while another **input** action that also writes it is legal in a state the first fires from and does not leave. The invariant then holds only because the action asserted it, and a concurrent observation is discarded silently. The message names the three remedies; it is a warning because which one applies is a design decision.

## Gotchas
- A failed level prints detail lines under its summary (`crates/temper-verify/src/diagnostics.rs`): the violated invariant, the action that broke it, the states either side, and the seed to replay. **Fix the root cause, never the invariant** - a count with no name is what makes deleting the invariant look like the cheap repair.
- L2 re-checks an action's precondition at delivery, not only when it is chosen. It must: the fault scheduler delays messages, and applying one to the state the actor has since reached fabricates a transition the spec forbids and reports a phantom violation. `test-fixtures/specs/late_delivery.ioa.toml` is the regression - a spec no legal execution can break, so any violation it reports is proof the check regressed.
- The old "L0-L3 = parse / table-build / model-check / DST" description is stale; the current levels are the five above, and parse + table-build are preconditions.
- L2b does not run in the CLI cascade - do not claim actor-level DST from `temper verify`; cite the `dst_*` suites for that.
- The `.claude` hook runs the cascade automatically on `.ioa.toml` edits and BLOCKS on failure; run it yourself first to keep the edit loop. `.cascade-results/` is local state, never committed.
