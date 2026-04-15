# WASM Integration for Agent-Generated API Calls

**Date**: 2026-02-23
**Status**: Complete (pending commit)

## Phases

- [x] Phase 0: ADR document (`docs/adrs/0002-wasm-integration-for-agent-generated-api-calls.md`)
- [x] Phase 1: `temper-wasm` crate (engine, host trait, types) — 4 tests passing
- [x] Phase 2: Spec format changes (Effect::Trigger, Integration fields, parser, builder) — 4 new tests
- [x] Phase 3: Storage (wasm_modules table in Turso + Postgres) — schema + CRUD methods
- [x] Phase 4: Dispatch wiring (WasmModuleRegistry, ServerState integration) — non-async fire-and-forget
- [x] Phase 5: DST support (SimIntegrationResponses, callback scheduling in SimActorSystem)
- [x] Phase 6: Platform & deploy pipeline (WASM module validation, DeployInput.wasm_modules)

## Key Decisions
- Async post-transition model: WASM runs AFTER transition succeeds, feeds back as input action
- Separate `wasm_modules` table for module storage
- Wasmtime v29 with fuel metering (TigerStyle budgets)
- DST: WASM not executed in simulation, callbacks injected by scheduler
- `dispatch_wasm_integrations()` is non-async — spawns tokio tasks for each callback (avoids recursive async)
- `Effect::Trigger { name }` at spec level maps to `Effect::Custom(name)` at JIT level

## Verification
- `cargo check --workspace` — clean
- `cargo test --workspace` — 622 tests, 0 failures
- DST review: running
- Code review: running

## Files Created
- `docs/adrs/0002-wasm-integration-for-agent-generated-api-calls.md`
- `crates/temper-wasm/Cargo.toml`
- `crates/temper-wasm/src/lib.rs`
- `crates/temper-wasm/src/types.rs`
- `crates/temper-wasm/src/host_trait.rs`
- `crates/temper-wasm/src/engine.rs`
- `crates/temper-server/src/wasm_registry.rs`

## Files Modified
- `Cargo.toml` (workspace) — added temper-wasm, wasmtime, sha2, async-trait
- `crates/temper-spec/src/automaton/types.rs` — Effect::Trigger variant, Integration fields
- `crates/temper-spec/src/automaton/parser.rs` — trigger parsing, WASM validation
- `crates/temper-jit/src/table/builder.rs` — Trigger → Custom conversion
- `crates/temper-store-turso/src/schema.rs` — wasm_modules table
- `crates/temper-store-turso/src/store.rs` — CRUD + migration
- `crates/temper-store-turso/src/lib.rs` — re-exports
- `crates/temper-store-postgres/src/schema.rs` — wasm_modules table
- `crates/temper-server/src/lib.rs` — wasm_registry module
- `crates/temper-server/src/state.rs` — dispatch wiring, WASM module CRUD
- `crates/temper-runtime/src/scheduler/sim_handler.rs` — pending_callbacks()
- `crates/temper-runtime/src/scheduler/sim_actor_system.rs` — SimIntegrationResponses
- `crates/temper-runtime/src/scheduler/mod.rs` — re-exports
- `crates/temper-server/src/entity_actor/sim_handler.rs` — custom effects capture
- `crates/temper-platform/src/deploy/pipeline.rs` — WASM validation step
- `crates/temper-verify/src/model/builder.rs` — Trigger → None mapping
