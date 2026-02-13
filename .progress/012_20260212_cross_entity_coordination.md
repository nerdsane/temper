# Gap #10: Cross-Entity Coordination via Reaction Rules

## Status: COMPLETE

## Phases

- [x] Phase 1: Types + Registry (new reaction module)
- [x] Phase 2: Simulation Dispatcher (SimReactionSystem)
- [x] Phase 3: Production Dispatcher + ServerState integration
- [x] Phase 4: Registry Integration (TenantConfig reactions)
- [x] Phase 5: Tests (unit + integration) — 82 tests pass in temper-server, full workspace green
- [x] Phase 6: Gap Tracker update — #10 marked RESOLVED, P1 now 12/12

## Key Decisions

- Reaction rules are platform-level config, not part of IOA specs
- Choreography pattern: when entity X completes action A reaching state S, dispatch action B on entity Y
- `toml` crate added as workspace dependency for reaction rule parsing
- All types use BTreeMap (DST compliance)
- Cascade bounded by MAX_REACTION_DEPTH = 8 (TigerStyle budget)
- Split `dispatch_tenant_action` into public (with reactions) and `_core` (without) to avoid async recursion

## Files Created

- `crates/temper-server/src/reaction/mod.rs`
- `crates/temper-server/src/reaction/types.rs`
- `crates/temper-server/src/reaction/registry.rs`
- `crates/temper-server/src/reaction/sim_dispatcher.rs`
- `crates/temper-server/src/reaction/dispatcher.rs`
- `crates/temper-server/tests/reaction_cascade.rs`

## Files Modified

- `Cargo.toml` — added `toml` workspace dependency
- `crates/temper-server/Cargo.toml` — added `toml` dependency
- `crates/temper-server/src/lib.rs` — added `pub mod reaction;`
- `crates/temper-server/src/state.rs` — added `reaction_dispatcher` field + `with_reaction_dispatcher()` + split dispatch into core/public
- `crates/temper-server/src/registry.rs` — added `reactions` field to TenantConfig + integration methods
- `docs/GAP_TRACKER.md` — marked #10 resolved
