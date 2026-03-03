# Cedar Authorization for Agent Backend

## Status: Complete
## Started: 2026-02-24
## Completed: 2026-02-24

## Prerequisite: Fix Compilation Error
- [x] Check loader.rs:364 — already fixed
- [x] Check webhooks.rs:222 — already fixed
- [x] Workspace compiles clean

## Phase 1: Wire Real Context Into Cedar (Level 1 — Entity Actions)
- [x] Add `SecurityContext::with_agent_context()` in temper-authz/context.rs
- [x] Add `authorize_with_context()` on ServerState in entity_ops.rs
- [x] Fix bindings.rs to extract X-Temper-* headers and use rich context
- [x] Move entity state fetch before authz check (resource attrs available)
- [x] Pass HeaderMap through from write.rs
- [x] Tests: 3 new tests in context.rs, all 16 temper-authz tests pass

## Phase 2: WASM Host Function Authorization (Level 2)
- [x] Add `WasmAuthzContext` to temper-wasm/types.rs
- [x] Create `WasmAuthzGate` trait + `AuthorizedWasmHost` decorator in temper-wasm/authorized_host.rs
- [x] Create `CedarWasmAuthzGate` + `PermissiveWasmAuthzGate` in temper-server/wasm_authz_gate.rs
- [x] Wire `AuthorizedWasmHost` in dispatch.rs (wraps ProductionWasmHost)
- [x] Wire `AuthorizedWasmHost` in dispatch_blocking.rs
- [x] Thread `agent_ctx` through `dispatch_wasm_integrations`
- [x] Domain extraction: pure string parsing (no url crate)
- [x] Tests: 11 new tests in authorized_host.rs, 5 new tests in wasm_authz_gate.rs

## Phase 3: Secret Pre-Filtering (Level 3)
- [x] Add `get_authorized_wasm_secrets()` method on ServerState (dispatch.rs)
- [x] Use filtered secrets in dispatch.rs (fire-and-forget path)
- [x] Use filtered secrets in dispatch_blocking.rs (inline path)

## Phase 4: Policy Lifecycle
- [x] Create test-fixtures/specs/policies/platform-presets.cedar (Tier 1)
- [x] Add authz_denied/denied_resource/denied_module fields to TrajectoryEntry
- [x] Add `AuthzDenied` variant to `ObservationClass` in temper-evolution/records.rs
- [x] Add `suggest_cedar_policies()` in temper-platform deploy pipeline (Tier 2)
- [x] Create docs/adrs/0004-cedar-authorization-for-agents.md
- [x] All 738 workspace tests pass

## Files Changed
### New Files
- `crates/temper-wasm/src/authorized_host.rs` — WasmAuthzGate trait + AuthorizedWasmHost decorator
- `crates/temper-server/src/wasm_authz_gate.rs` — CedarWasmAuthzGate + PermissiveWasmAuthzGate
- `test-fixtures/specs/policies/platform-presets.cedar` — Platform preset policies (Tier 1)
- `docs/adrs/0004-cedar-authorization-for-agents.md` — Architecture Decision Record

### Modified Files
- `crates/temper-authz/src/context.rs` — Added `with_agent_context()` method
- `crates/temper-wasm/src/types.rs` — Added `WasmAuthzContext` struct
- `crates/temper-wasm/src/lib.rs` — Export new authorized_host module
- `crates/temper-server/src/lib.rs` — Register wasm_authz_gate module
- `crates/temper-server/src/state/entity_ops.rs` — Added `authorize_with_context()` method
- `crates/temper-server/src/odata/bindings.rs` — Rich Cedar context with headers + entity state
- `crates/temper-server/src/odata/write.rs` — Pass HeaderMap to dispatch_bound_action
- `crates/temper-server/src/state/dispatch.rs` — AuthorizedWasmHost + agent_ctx threading + secret pre-filtering
- `crates/temper-server/src/state/dispatch_blocking.rs` — Same as dispatch.rs
- `crates/temper-server/src/state/trajectory.rs` — Added authz_denied fields
- `crates/temper-server/src/webhooks.rs` — Updated TrajectoryEntry constructor
- `crates/temper-server/src/observe/evolution.rs` — Updated TrajectoryEntry constructor
- `crates/temper-cli/src/serve/loader.rs` — Updated TrajectoryEntry constructor
- `crates/temper-evolution/src/records.rs` — Added AuthzDenied observation class
- `crates/temper-platform/src/deploy/pipeline.rs` — Cedar policy suggestion generation
