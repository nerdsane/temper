# ADR-0076: Eliminate ServerEventStore Enum Dispatch

- Status: Implemented
- Date: 2026-04-29
- Deciders: Temper core maintainers
- Related:
  - ADR-0065: Postgres Platform Store and Canonical Schema
  - ADR-0066: StorageStack Backend Selection
  - `crates/temper-server/src/storage/mod.rs`

## Context

Before this ADR was implemented, `ServerEventStore` still contained long-tail
enum dispatch after the Postgres cutover work. The enum was safe enough for
cutover only because Postgres arms delegated to real implementations, but it was
still the wrong long-term boundary: new backend capabilities could be added with
silent `Ok(())` or `Ok(None)` match arms.

ADR-0066 introduced `StorageStack`, `DynEventStore`, `QueryPlaneStore`, and
`TrajectorySink`. That made the event journal object-safe and moved hot
query-plane / trajectory write paths off business-code enum matching. This ADR
defined the finite plan to remove the remaining compatibility enum entirely.

## Decision

`ServerEventStore` is retired. Production code obtains storage through
`StorageStack` capability traits, and boot-time backend selection constructs
`StorageStack` directly from concrete backend handles.

Unsupported features are represented as:

- `None` for optional capabilities that are deliberately absent on a backend.
- `Err(...)` for configured capabilities that fail or are unsupported for the
  selected backend.

Silent successful no-ops are not an acceptable storage capability fallback.

## Retired Inventory

| Retired method | Concern | Target capability |
| --- | --- | --- |
| `backend_name` | diagnostics | `BackendLabel` |
| `postgres_pool` | backend handle | `StorageStack::postgres_pool` |
| `turso_store` | backend handle | `TursoStoreProvider` |
| `platform_turso_store` | backend handle | `TursoStoreProvider` |
| `platform_store` | platform metadata | `PlatformStore` |
| `tenant_router` | tenant routing | `TursoStoreProvider` |
| `turso_for_tenant` | tenant routing | `TursoStoreProvider` / typed tenant stores |
| `redis_store` | backend handle | no public production accessor |
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

## Implementation

Implemented in the StorageStack migration series:

1. Event/query/trajectory capabilities are backed by concrete
   `PostgresEventStore`, `TursoEventStore`, `TenantStoreRouter`,
   `RedisEventStore`, or `SimEventStore` handles.
2. Concern clusters moved to capability traits:
   `PolicyStore`, `ObserveReadStore`, `EvolutionStore`,
   `DesignTimeEventStore`, `OtsStore`, `BlobStore`,
   `AuthzAnalyticsStore`, `DecisionStore`, `WasmMetadataStore`,
   `WasmInvocationStore`, `MetadataStoreProvider`, and
   `TursoStoreProvider`.
3. Production call sites moved to `StorageStack` capabilities.
4. `ServerState` no longer has an `event_store` field.
5. `crates/temper-server/src/event_store.rs` was deleted.
6. `temper serve` constructs `StorageStack` directly from boot-time backend
   configuration.

## Definition of Done

- `crates/temper-server/src/event_store.rs` does not exist.
- `ServerState` contains `storage_stack` as the only durable storage field.
- Backend selection is matched exactly once at boot, when constructing
  `StorageStack`.
- CI fails if a production module reintroduces `ServerEventStore`,
  `from_server_event_store`, or direct `event_store` storage access.
- Redis/simulation unsupported capabilities return explicit absence or errors,
  never successful persistence no-ops.

## Readiness Gates

- `cargo test -p temper-server --test storage_stack` verifies the object-safe
  event adapter and concrete Turso stack capabilities.
- `bash scripts/check-storage-dispatch-boundary.sh` reports `0/0` legacy
  violations and fails if the retired compatibility surface returns.

## Consequences

### Positive

- New backends implement declared capabilities instead of editing a large enum.
- Missing capability support becomes visible at construction or call time.
- Code review no longer has to police new storage code by taste alone.

### Negative

- The implementation creates several small storage traits. Their boundaries must
  remain concern-based, not one trait per method.
- Backends that do not implement a capability must surface absence explicitly.

### DST Compliance

Simulation keeps using `SimEventStore` and `SimPlatformStore`, but simulated
platform capabilities must implement the same traits as production where they
claim support. Unsupported simulated capabilities are explicit.

## Rollback Policy

Capability extraction is internal refactoring. Roll back the latest capability
migration commit if a concern regresses. The cutover rollback remains the
storage environment flip documented in ADR-0074.
