# WS6: Fix P0 Gaps — OData Dispatch + Codegen

## Status: COMPLETE

## Results

All 7 P0 gaps fixed. 515 workspace tests pass (0 failures).

### Phase 1: Entity GET 404 (Gap #6) — DONE
- Fixed `dispatch.rs`: returns 404 with OData error body on entity not found
- Added entity_exists check before attempting to spawn actors for GET
- Updated test: `test_entity_by_key` split into `test_entity_by_key_not_found` + `test_entity_by_key_found`

### Phase 2: POST Body Usage (Gap #5) — DONE
- Body JSON `id` field used as entity ID (falls back to UUID)
- Body fields passed as `initial_fields` to EntityActor
- Added `get_or_create_tenant_entity()` to ServerState
- Added `get_or_spawn_tenant_actor_with_fields()` to accept initial data

### Phase 3: Entity Set Listing (Gap #3) — DONE
- Added `entity_index: Arc<RwLock<HashMap<String, HashSet<String>>>>` to ServerState
- Entity IDs tracked per (tenant, entity_type) on spawn
- GET /EntitySet now enumerates actual entities from index
- `list_entity_ids()`, `entity_exists()`, `remove_entity()` methods added

### Phase 4: PATCH/PUT/DELETE (Gap #4) — DONE
- Added `UpdateFields { fields, replace }` and `Delete` variants to EntityMsg
- Actor handles UpdateFields (PATCH: merge, PUT: replace) preserving Id/Status
- Three new dispatch handlers: `handle_odata_patch`, `handle_odata_put`, `handle_odata_delete`
- Router updated with `.patch().put().delete()` route bindings
- DELETE returns 204 No Content and removes from index+registry
- PATCH/PUT/DELETE on nonexistent entities return 404

### Phase 5: Query Options (Gap #1) — DONE
- Created `query_eval.rs` with `apply_query_options()` function
- $filter: evaluates FilterExpr AST (eq, ne, gt, ge, lt, le, and, or, not)
- $filter: supports contains(), startswith(), endswith() functions
- $select: prunes entity JSON to selected properties (preserves @odata annotations)
- $orderby: sorts by property with asc/desc direction
- $top/$skip: pagination after filter+sort
- $count: returns @odata.count after filter, before pagination
- Applied to both entity set GET and single entity GET ($select, $expand)

### Phase 6: $expand (Gap #2) — DONE
- Single-level $expand implemented via CSDL navigation properties
- Resolves target entity type from NavigationProperty type_name
- Matches related entities by parentId/{EntityType}Id convention
- Supports Collection vs single navigation property distinction
- Nested query options ($select, $filter, $orderby, $top, $skip) supported inside $expand
- Applied to both entity set and single entity GET

### Phase 7: IOA Codegen (Gap #7) — DONE
- Added `SpecSource` enum (Ioa/Tla) to temper-spec model
- Added `build_spec_model_mixed()` accepting HashMap<String, SpecSource>
- IOA path: parse_automaton() → to_state_machine() → StateMachine
- TLA+ path: extract_state_machine() → StateMachine
- `to_state_machine` re-exported from temper-spec crate root
- Codegen CLI reads both .ioa.toml and .tla files, IOA takes precedence
- All 3 model tests pass (TLA+, IOA, precedence)
- All 6 codegen CLI tests pass

## Files Modified

| File | Changes |
|------|---------|
| `crates/temper-server/src/dispatch.rs` | 404 on not-found, POST body usage, entity listing, PATCH/PUT/DELETE handlers, $expand/$select on single entity |
| `crates/temper-server/src/state.rs` | entity_index, get_or_spawn_tenant_actor_with_fields, remove_entity, list_entity_ids, entity_exists, get_or_create_tenant_entity, update_tenant_entity_fields, delete_tenant_entity |
| `crates/temper-server/src/router.rs` | PATCH/PUT/DELETE routes, 7 new tests |
| `crates/temper-server/src/entity_actor/types.rs` | UpdateFields and Delete message variants |
| `crates/temper-server/src/entity_actor/actor.rs` | Handler logic for UpdateFields and Delete |
| `crates/temper-server/src/lib.rs` | query_eval module registration |
| `crates/temper-server/src/query_eval.rs` | NEW: $filter/$select/$orderby/$top/$skip/$expand evaluation |
| `crates/temper-spec/src/model/mod.rs` | SpecSource enum, build_spec_model_mixed, 2 new tests |
| `crates/temper-spec/src/lib.rs` | Re-export to_state_machine, build_spec_model_mixed, SpecSource |
| `crates/temper-cli/src/codegen/mod.rs` | read_ioa_sources, mixed source merging |
