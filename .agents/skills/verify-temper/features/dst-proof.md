# Deterministic simulation proof (the DST suites)

## Sub-features
Seeded runs, fault injection, invariant checking (P1-P18), crash/replay, reproduction. The actor-level DST coverage lives in standalone test binaries, not in the CLI cascade (see spec-cascade.md on L2b).

The 13 `dst_*` suites in `crates/temper-server/tests/` (each is its own `cargo test --test <name>` binary):

| Suite | Proves |
|---|---|
| `dst_concurrency_retry` | optimistic-concurrency retry under contention (ADR-0046) |
| `dst_entity_key_index` | key-index negative-existence invariant (ADR-0153) |
| `dst_entity_vector_index` | kNN vector-index reproducibility (ADR-0155) |
| `dst_genesis_install_rollback` | Genesis install-verify + rollback, invariant P18 (ARN-421) |
| `dst_hotswap` | spec hot-swap safety |
| `dst_lifecycle` | create -> dispatch -> persist -> crash -> respawn -> replay -> continue |
| `dst_multi_tenant` | tenant isolation |
| `dst_persistence` | real EntityActor + SimEventStore event persistence/replay |
| `dst_platform_boot` | platform boot-cycle correctness |
| `dst_platform_cedar` | Cedar policy lifecycle |
| `dst_platform_index` | index consistency |
| `dst_platform_random` | randomized workload against P1-P17 invariants |
| `dst_platform_rollback` | rollback / fault injection |

Two more seeded suites sit outside that prefix: `gmail_oauth_dst` (temper-server) and `system_entity_dst` (temper-platform). `platform_e2e_dst` is named `_dst` but is an E2E shared-registry test, not a seeded sim.

## How to get to it (user POV)
An engineer changing sim-visible kernel code proves the change holds across seeds and reproduces any failure exactly.

## Driving it
```bash
cargo test -p temper-server --test dst_lifecycle          # one suite (fast to iterate)
cargo test -p temper-server --test dst_platform_random    # randomized; TEMPER_DST_RANDOM_MODE=full|smoke
cargo test -p temper-platform --test system_entity_dst
cargo test -p temper-server dst_                           # all dst_* in temper-server
```

## What proves it
The suite passes across its seed range, and a failure reproduces under the same seed. Seeds are the loop index (`for seed in 0..NUM_SEEDS`); every assertion interpolates the seed (`panic!("seed {seed}: …")`), so the failing seed is named in the panic and re-running the named test replays it identically - there is no separate "replay seed N" flag. `dst_platform_random` scales its budget via `TEMPER_DST_RANDOM_MODE` (`full` = 100 seeds default, `smoke` = 10; any other value panics).

## Gotchas
- Code that passes tests can still break determinism (wall clock, HashMap order, `Uuid::new_v4`, `thread_rng`). The static guard `scripts/check-determinism.sh` (mirrored as a `.claude` hook) scans `temper-runtime`/`temper-jit`/`temper-server` and is suppressible only with `// determinism-ok`. The guard plus the DST reviewer ruleset (`.agents/agents/dst-reviewer.md`) are the check, not the suites alone.
- Determinism is installed from the seed via `install_deterministic_context(seed)` (seeds a logical clock + deterministic id gen). A test that reaches for real time/ids escapes it and will flake.
