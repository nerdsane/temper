# Spec hot-swap

## Sub-features
A running kernel takes a new version of a spec without a restart. Registry in `crates/temper-server/src/registry/mod.rs`; the swap primitive in `crates/temper-jit/src/swap.rs`; verification gate in `state/entity_ops.rs`.

## How to get to it (user POV)
An author pushes an edited spec to a live server; existing entities of that type immediately run under the new transition table once it re-verifies - no downtime, no redeploy.

## Driving it
After SKILL Launch the operator key (`TEMPER_API_KEY=local-verify`) is seeded **only** `manage_policies` on `PolicySet` (`operator_manage_policies.rs`). The HTTP routes authorize different Cedar actions: `load_specs_from_directory` on `SpecDirectory` (`observe/specs/load_dir.rs`) and `submit_specs` on `SpecRegistry` (`observe/specs/load_inline.rs`). Replay: `POST /api/specs/load-dir` or `POST /api/specs/load-inline` with `Authorization: Bearer local-verify` → **403 AuthorizationDenied**. Do not invent a permit.

The path that actually permits after SKILL Launch is in-process, not those HTTP routes:

```bash
# swap primitive + live-entity proof (no Cedar HTTP gate)
cargo test -p temper-server --test dst_hotswap
# offline cascade before you change a spec
cargo run -p temper-cli -- verify --specs-dir <dir>
```

To get specs into a live serve, restart with `--app NAME=DIR` (or legacy `--specs-dir`); that path is `serve/loader.rs` at boot, not the Cedar-gated management API. There is no CLI verb for a live swap.

HTTP shape (403 for the SKILL Launch operator; body contract only — do not drive these expecting 200):
```bash
# replace mode (directory is truth; entity types absent from the dir are dropped)
# Cedar: load_specs_from_directory → 403 for the operator key
curl -sS -X POST "http://localhost:3600/api/specs/load-dir" \
  -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" -d '{"tenant":"default","specs_dir":"/abs/dir","merge":false}'
# merge mode (agent submit_specs; preserves untouched types + relation graph)
# Cedar: submit_specs → 403 for the operator key; MCP submit_specs hits this same route
curl -sS -X POST "http://localhost:3600/api/specs/load-inline" \
  -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" -d '{"tenant":"default","specs":{"model.csdl.xml":"<CSDL>","Order.ioa.toml":"..."}}'
```
The `load-inline` `specs` map MUST include `model.csdl.xml` alongside the `*.ioa.toml` files: load-inline stages the map to a temp dir and calls load-dir, which reads `model.csdl.xml` as required (`observe/specs/load_dir.rs:230`) and fails without it. For an existing entity type, `SwapController::swap` writes the new `TransitionTable` into the same `RwLock` the live actors hold and bumps an atomic version - actors pick it up with no restart. New entity types get a fresh spec instead. `load-dir` verifies inline and streams NDJSON per entity.

## What proves it
The swap logs `hot-swapped transition table for existing entity` with `old_version`/`new_version`, and a new action/state defined only in the new spec becomes usable on a pre-existing entity id (same actor, no restart) once verification passes. `GET /observe/specs/{entity}` reflects the new spec. Verification is reset to Pending on register, so dispatch is gated until it completes.

## Gotchas
- **No state migration on swap.** The swap replaces the table + metadata only; persisted entities keep their stored status. If the new table drops a state that live entities occupy, those entities are stranded - nothing validates old status against the new state set at swap time.
- **Breaking specs are not rejected at swap; they are gated after.** On register, verification goes Pending -> Running; dispatch is blocked (`check_verification_gate`) until it reaches `Completed(all_passed=true)`. A failing spec swaps in, fails verification, and blocks all dispatches on that type with "Fix the spec and re-push" until a passing one lands.
- Merge vs replace is load-bearing: replace deletes types absent from the submission and resets all verification; merge preserves untouched types and the relation graph.
- `temper-evolution` is NOT this - it is the GEPA prompt-evolution engine for agents. Spec hot-swap is `registry` + `temper-jit/swap.rs`.
- **HTTP load-dir / load-inline 403 for the SKILL Launch operator.** The seeded permit is `manage_policies` on `PolicySet` only. `load_specs_from_directory` / `submit_specs` are not on that seed. Drive `dst_hotswap` or restart serve with `--app NAME=DIR`; do not invent a Cedar permit to make the curls return 200.
- No file-watcher: hot-swap is push-only. Genesis production installs add their own verify + rollback layer (ARN-421/422).
