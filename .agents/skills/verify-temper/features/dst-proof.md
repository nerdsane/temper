# Deterministic simulation proof

## Sub-features
Seeded runs, fault injection, invariant checking, reproduction.

## Driving it
```bash
cargo test -p temper-platform --test platform_e2e_dst           # E2E shared-registry proof
cargo test -p temper-runtime                                    # sim runtime suite
```

## What proves it
The DST suite passes, and a failure reproduces under the same seed (the failing output names the seed; rerunning with it must fail identically). For changed sim-visible code, the determinism guard (`scripts/check-determinism.sh`) reports no new violations.

## Gotchas
Code that passes tests can still break determinism (wall clock, HashMap order) - the guard and the DST reviewer ruleset in `.agents/agents/dst-reviewer.md` are the check, not the test suite alone.
