# ADR-0076: Eliminate ServerEventStore Enum Dispatch

- Status: Accepted
- Date: 2026-04-29
- Deciders: Temper core maintainers
- Related:
  - ADR-0065: Postgres Platform Store and Canonical Schema
  - ADR-0066: StorageStack Backend Selection
  - `crates/temper-server/src/event_store.rs`
  - `crates/temper-server/src/storage/mod.rs`

## Context

`ServerEventStore` still contains long-tail enum dispatch after the Postgres
cutover work. The enum is safe enough for cutover only because Postgres arms
delegate to real implementations, but it is still the wrong long-term boundary:
new backend capabilities can be added with silent `Ok(())` or `Ok(None)` match
arms.

ADR-0066 introduced `StorageStack`, `DynEventStore`, `QueryPlaneStore`, and
`TrajectorySink`. That made the event journal object-safe and moved hot
query-plane / trajectory write paths off business-code enum matching. This ADR
defines the finite plan to remove the remaining compatibility enum entirely.

## Decision

`ServerEventStore` is a transitional compatibility handle only. New production
code must obtain storage through `StorageStack` capability traits, and
`StorageStack::from_server_event_store` must populate those slots from concrete
backend handles, not `Arc<ServerEventStore>` adapters.

Unsupported features are represented as:

- `None` for optional capabilities that are deliberately absent on a backend.
- `Err(...)` for configured capabilities that fail or are unsupported for the
  selected backend.

Silent successful no-ops are not an acceptable storage capability fallback.

## Current Inventory

| Current method | Concern | Target capability |
| --- | --- | --- |
| `backend_name` | diagnostics | `BackendLabel` |
| `postgres_pool` | compatibility | delete after typed stores exist |
| `turso_store` | compatibility | delete after typed stores exist |
| `platform_turso_store` | compatibility | `PlatformStore` / typed observe stores |
| `platform_store` | platform metadata | `PlatformStore` |
| `tenant_router` | compatibility | delete after `TenantQueryReader` |
| `turso_for_tenant` | tenant routing | `TenantQueryReader` / typed tenant stores |
| `redis_store` | compatibility | delete with enum |
| `save_policy` | policy management | `PolicyStore` |
| `load_policies_for_tenant` | policy management | `PolicyStore` |
| `load_all_policies` | policy management | `PolicyStore` |
| `toggle_policy_enabled` | policy management | `PolicyStore` |
| `update_policy_text` | policy management | `PolicyStore` |
| `delete_policy` | policy management | `PolicyStore` |
| `persist_trajectory_entry` | trajectory writes | `TrajectorySink` |
| `upsert_query_projection` | query plane | `QueryPlaneStore` |
| `remove_query_projection` | query plane | `QueryPlaneStore` |
| `query_field_index` | query plane | `QueryPlaneStore` |
| `load_query_projection_fields_many` | query plane | `QueryPlaneStore` |
| `projected_entity_counts_by_tenant` | query plane | `QueryPlaneStore` |
| `load_recent_trajectories` | observe reads | `ObserveReadStore` |
| `load_unmet_intent_rows` | observe reads | `ObserveReadStore` |
| `load_submit_spec_timestamps` | observe reads | `ObserveReadStore` |
| `count_trajectories_by_tenant` | observe reads | `ObserveReadStore` |
| `query_trajectory_stats` | observe reads | `ObserveReadStore` |
| `query_trajectories_by_agent` | observe reads | `ObserveReadStore` |
| `query_agent_summaries` | observe reads | `ObserveReadStore` |
| `upsert_feature_request` | evolution | `EvolutionStore` |
| `list_feature_requests` | evolution | `EvolutionStore` |
| `update_feature_request` | evolution | `EvolutionStore` |
| `insert_evolution_record` | evolution | `EvolutionStore` |
| `get_evolution_record` | evolution | `EvolutionStore` |
| `list_evolution_records` | evolution | `EvolutionStore` |
| `list_ranked_insights` | evolution | `EvolutionStore` |
| `insert_design_time_event` | design-time events | `DesignTimeEventStore` |
| `list_design_time_events` | design-time events | `DesignTimeEventStore` |
| `persist_ots_trajectory` | OTS | `OtsStore` |
| `list_ots_trajectories` | OTS | `OtsStore` |
| `get_ots_trajectory` | OTS | `OtsStore` |
| `put_blob` | blobs | `BlobStore` |
| `put_blob_with_ttl` | blobs | `BlobStore` |
| `sweep_expired_blobs` | blobs | `BlobStore` |
| `get_blob` | blobs | `BlobStore` |
| `upsert_policy_denial_pattern` | authz analytics | `AuthzAnalyticsStore` |
| `load_policy_denial_patterns` | authz analytics | `AuthzAnalyticsStore` |
| `query_decisions` | authz decisions | `DecisionStore` |
| `query_all_decisions` | authz decisions | `DecisionStore` |
| `get_pending_decision` | authz decisions | `DecisionStore` |
| `load_wasm_module_metadata_all_tenants` | WASM metadata | `WasmMetadataStore` |
| `persist_wasm_invocation` | WASM invocation log | `WasmInvocationStore` |
| `load_recent_wasm_invocations` | WASM invocation log | `WasmInvocationStore` |
| `delete_wasm_module` | WASM metadata | `WasmMetadataStore` |

## Migration Plan

1. Keep the cutover-ready event/query/trajectory capabilities on
   `StorageStack`, but ensure those trait objects are backed by concrete
   `PostgresEventStore`, `TursoEventStore`, or `TenantStoreRouter` handles.
2. Add one capability trait per remaining concern cluster:
   `PolicyStore`, `ObserveReadStore`, `EvolutionStore`,
   `DesignTimeEventStore`, `OtsStore`, `BlobStore`,
   `AuthzAnalyticsStore`, `DecisionStore`, `WasmMetadataStore`, and
   `WasmInvocationStore`.
3. Migrate call sites one capability at a time. Each migration removes the
   corresponding methods from `ServerEventStore`.
4. Delete compatibility accessors (`postgres_pool`, `turso_store`,
   `turso_for_tenant`, etc.) once no call site needs backend-specific handles.
5. Delete `ServerEventStore` once it has no remaining business methods and
   construct `StorageStack` directly from boot-time backend configuration.

## Definition of Done

- `rg "match self" crates/temper-server/src/event_store.rs` returns no
  storage business dispatch.
- `ServerState` contains `storage_stack` as the only durable storage field.
- Backend selection is matched exactly once at boot, when constructing
  `StorageStack`.
- CI fails if a production module reintroduces backend enum dispatch outside
  storage construction and tests.
- Redis/simulation unsupported capabilities return explicit absence or errors,
  never successful persistence no-ops.

## Readiness Gates

- `cargo test -p temper-server --test storage_stack` includes a regression test
  proving stack capabilities are not backed by `ServerEventStore` trait
  adapters.
- Each new capability trait has Postgres and Turso implementations before its
  call sites move.
- Every migrated concern deletes at least one `ServerEventStore` method in the
  same change.

## Consequences

### Positive

- New backends implement declared capabilities instead of editing a large enum.
- Missing capability support becomes visible at construction or call time.
- Code review no longer has to police new storage code by taste alone.

### Negative

- The migration creates several small storage traits. Their boundaries must
  remain concern-based, not one trait per method.
- Until the final deletion, old compatibility methods remain available for
  unmigrated code and tests.

### DST Compliance

Simulation keeps using `SimEventStore` and `SimPlatformStore`, but simulated
platform capabilities must implement the same traits as production where they
claim support. Unsupported simulated capabilities are explicit.

## Rollback Policy

Capability extraction is internal refactoring. Roll back the latest capability
migration commit if a concern regresses. The cutover rollback remains the
storage environment flip documented in ADR-0074.
