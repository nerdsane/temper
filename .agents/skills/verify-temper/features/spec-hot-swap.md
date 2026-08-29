# Spec hot-swap

## Sub-features
A running kernel takes a new version of a spec without a restart. Registry in `crates/temper-server/src/registry/mod.rs`; the swap primitive in `crates/temper-jit/src/swap.rs`; verification gate in `state/entity_ops.rs`.

## How to get to it (user POV)
An author pushes an edited spec to a live server; existing entities of that type immediately run under the new transition table once it re-verifies - no downtime, no redeploy.

## Driving it
Push over the management API (there is no CLI verb for it):
```bash
# replace mode (directory is truth; entity types absent from the dir are dropped)
curl -sS -X POST "http://localhost:3600/api/specs/load-dir" \
  -H "Authorization: Bearer $KEY" -d '{"tenant":"default","specs_dir":"/abs/dir","merge":false}'
# merge mode (agent submit_specs; preserves untouched types + relation graph)
curl -sS -X POST "http://localhost:3600/api/specs/load-inline" \
  -H "Authorization: Bearer $KEY" -d '{"tenant":"default","specs":{"Order.ioa.toml":"..."}}'
```
For an existing entity type, `SwapController::swap` writes the new `TransitionTable` into the same `RwLock` the live actors hold and bumps an atomic version - actors pick it up with no restart. New entity types get a fresh spec instead. `load-dir` verifies inline and streams NDJSON per entity; MCP `submit_specs` hits `load-inline`. Offline check first with `cargo run -p temper-cli -- verify --specs-dir <dir>`. The dedicated seeded suite is `cargo test -p temper-server --test dst_hotswap`.

## What proves it
The swap logs `hot-swapped transition table for existing entity` with `old_version`/`new_version`, and a new action/state defined only in the new spec becomes usable on a pre-existing entity id (same actor, no restart) once verification passes. `GET /observe/specs/{entity}` reflects the new spec. Verification is reset to Pending on register, so dispatch is gated until it completes.

## Gotchas
- **No state migration on swap.** The swap replaces the table + metadata only; persisted entities keep their stored status. If the new table drops a state that live entities occupy, those entities are stranded - nothing validates old status against the new state set at swap time.
- **Breaking specs are not rejected at swap; they are gated after.** On register, verification goes Pending -> Running; dispatch is blocked (`check_verification_gate`) until it reaches `Completed(all_passed=true)`. A failing spec swaps in, fails verification, and blocks all dispatches on that type with "Fix the spec and re-push" until a passing one lands.
- Merge vs replace is load-bearing: replace deletes types absent from the submission and resets all verification; merge preserves untouched types and the relation graph.
- `temper-evolution` is NOT this - it is the GEPA prompt-evolution engine for agents. Spec hot-swap is `registry` + `temper-jit/swap.rs`.
- No file-watcher: hot-swap is push-only. Genesis production installs add their own verify + rollback layer (ARN-421/422).
