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

## Gotchas
- The old "L0-L3 = parse / table-build / model-check / DST" description is stale; the current levels are the five above, and parse + table-build are preconditions.
- L2b does not run in the CLI cascade - do not claim actor-level DST from `temper verify`; cite the `dst_*` suites for that.
- The `.claude` hook runs the cascade automatically on `.ioa.toml` edits and BLOCKS on failure; run it yourself first to keep the edit loop. `.cascade-results/` is local state, never committed.
