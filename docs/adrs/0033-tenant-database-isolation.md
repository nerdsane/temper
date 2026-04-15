# ADR-0033: Tenant Database Isolation + Turso Secrets

- Status: Accepted
- Date: 2026-03-17
- Deciders: Temper core maintainers
- Related:
  - ADR-0032: Granular Cedar policy storage (same hash-gating pattern)
  - `crates/temper-server/src/state/persistence/mod.rs`
  - `crates/temper-server/src/state/mod.rs`
  - `crates/temper-server/src/event_store.rs`
  - `crates/temper-store-turso/src/router.rs`

## Context

In TenantRouted mode (database-per-tenant via Turso), only events and snapshots are correctly routed to per-tenant databases via `TenantStoreRouter`. **All other metadata** — specs, trajectories, pending decisions, Cedar policies, WASM modules, design-time events — is incorrectly written to the **platform database**, defeating the purpose of tenant isolation.

Additionally, secrets are not supported on the Turso backend at all (returns "not supported yet") despite no technical limitation — Turso supports BLOB storage (used for WASM modules), and the Postgres schema already has a `tenant_secrets` table with RLS policies.

**Empirical proof** (verified locally with two tenants in TenantRouted mode):
- Platform DB: 39 spec rows across all tenants, all trajectories, all decisions, all policies
- Tenant DBs: only events (2 each), everything else empty

**Root cause**: Two methods on `ServerState` always delegate to the platform store:
1. `metadata_backend()` calls `store.platform_turso_store()` — routes all Turso metadata to platform
2. `persistent_store()` calls `store.platform_turso_store()` — routes all direct Turso reads to platform

The correct routing method `ServerEventStore::turso_for_tenant()` already exists but is never called from metadata persistence paths.

**Backend abstraction is sound**: `MetadataBackend` already has `Postgres` and `Turso` variants. Postgres uses RLS for isolation (single DB, `WHERE tenant=`). Turso TenantRouted uses separate DBs. Both go through the same persistence methods. The fix is routing — not restructuring.

## Decision

### Sub-Decision 1: New tenant-scoped routing methods

Add `metadata_backend_for_tenant(tenant)` (async) and `persistent_store_for_tenant(tenant)` (async) that route to per-tenant stores in TenantRouted mode.

Rename `metadata_backend()` → `platform_metadata_backend()` and `persistent_store()` → `platform_persistent_store()`. These are only used for system-wide analytics (evolution records, feature requests) that genuinely belong in the platform DB.

The existing `turso_for_tenant()` on `ServerEventStore` handles the routing logic:
- Single-DB Turso: returns the shared store (clone, Arc-cheap)
- TenantRouted: returns per-tenant store via `router.store_for_tenant(tenant)`
- `temper-system`/`default` automatically route to platform

A new `TenantMetadataBackend` enum owns its store (vs the existing `MetadataBackend<'a>` which borrows) because `turso_for_tenant()` returns an owned `TursoEventStore`.

**Why this approach**: Minimal API surface change. All tenant-scoped methods already receive `tenant` as a parameter — only the routing call changes. The backend abstraction (Postgres/Turso/Redis) is preserved.

### Sub-Decision 2: Fix all metadata write paths

Every persistence call that has a `tenant` parameter and currently uses `metadata_backend()` or `persistent_store()` switches to the tenant-scoped variant:
- Spec persistence (`upsert_spec_source`, `upsert_tenant_constraints`, `persist_spec_verification`)
- Trajectory persistence (`persist_trajectory_entry`)
- Design-time events (`emit_design_time_event`)
- Pending decisions (`persist_pending_decision`)
- WASM modules (`upsert_wasm_module`, `delete_wasm_module`, `persist_wasm_invocation`)
- Cedar policies (`persist_and_activate_policy`)

**Why this approach**: Every method already has the tenant available. No new parameters needed — just a different routing call.

### Sub-Decision 3: Fan-out for cross-tenant reads

Observe layer endpoints that aggregate across tenants need a fan-out pattern when data is in per-tenant DBs. A `collect_from_tenant_stores` helper iterates platform + all connected tenant stores.

Tenant-scoped reads (where tenant is in the URL/params) route directly to the tenant store. Cross-tenant reads (no tenant filter) fan out.

**Why this approach**: Follows the existing pattern in `load_wasm_modules()` (lines 254-299 of persistence/mod.rs) which already implements fan-out correctly for TenantRouted mode.

### Sub-Decision 4: Turso secrets implementation

Add a `tenant_secrets` table mirroring the Postgres schema, with `ciphertext BLOB` and `nonce BLOB` columns. Implement CRUD on `TursoEventStore` and wire into the persistence layer, replacing the "not supported yet" errors.

```sql
CREATE TABLE IF NOT EXISTS tenant_secrets (
    tenant TEXT NOT NULL,
    key_name TEXT NOT NULL,
    ciphertext BLOB NOT NULL,
    nonce BLOB NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY(tenant, key_name)
);
```

**Why this approach**: No technical limitation exists. Turso supports BLOB (used for WASM modules already). Secrets are tenant-scoped and must go to per-tenant DBs for proper isolation.

### Sub-Decision 5: Fix bootstrap paths

Boot-time recovery functions that use `platform_turso_store()` switch to per-tenant routing:
- `load_verified_cache(tenant)` → `turso_for_tenant(tenant)`
- `bootstrap_tenants()` → resolve per-tenant inside each loop
- `install_os_app()` → specs/policies to tenant store, `record_installed_app` stays on platform (cross-tenant boot index)

**Why this approach**: Boot functions already have tenant as a parameter. The router handles `temper-system`/`default` automatically.

## What Stays in Platform DB

| Table | Reason |
|---|---|
| `tenant_registry` | Maps tenant IDs to DB URLs |
| `tenant_users` | User-to-tenant access mappings |
| `tenant_installed_apps` | Cross-tenant boot index |
| `evolution_records` | System-wide analytics (O-P-A-D-I chain) |
| `feature_requests` | System-wide analytics |
| `temper-system` all data | System tenant by design |
| `default` all data | Default tenant by design |

## What Moves to Per-Tenant DBs

| Table | Before | After |
|---|---|---|
| `specs` | Platform | Per-tenant |
| `trajectories` | Platform | Per-tenant |
| `pending_decisions` | Platform | Per-tenant |
| `tenant_policies` / `policies` | Platform | Per-tenant |
| `wasm_modules` | Platform | Per-tenant |
| `wasm_invocation_logs` | Platform | Per-tenant |
| `design_time_events` | Platform | Per-tenant |
| `tenant_secrets` | Not in Turso | Per-tenant (new) |

## Consequences

### Positive
- Complete tenant data isolation in TenantRouted mode — no cross-tenant data leakage.
- Secrets fully functional on Turso backend with proper per-tenant isolation.
- Backend-swappable architecture preserved — Postgres with RLS achieves the same isolation.
- Fan-out reads maintain existing observe UI functionality across tenants.

### Negative
- Fan-out reads add latency proportional to number of connected tenants (mitigated by `connected_tenants()` returning only active stores).
- All persistence methods become async (were sync for borrowed store access).

### Risks
- Missing a write path that still routes to platform. Mitigated by renaming `persistent_store()` → `platform_persistent_store()` which forces compiler errors at every call site.
- Fan-out read performance with many tenants. Mitigated by SQL-level aggregation (existing pattern) and tenant-scoped query parameters.

### DST Compliance
- No new `HashMap`/`HashSet` usage; `BTreeMap` used throughout.
- No `tokio::spawn`, `std::thread::spawn`, or multi-threaded patterns introduced.
- `sim_now()` used for trajectory timestamps (existing pattern, unchanged).
- Turso SQL layer uses `datetime('now')` for DB-level timestamps (existing precedent in all tables).

## Non-Goals
- Migrating existing platform DB data to per-tenant DBs. This is a clean fix — data written after deployment goes to the right place; old data in the platform DB is superseded.
- Automatic data migration tooling. The two-pass boot recovery in `recover_cedar_policies` already handles coexistence.
- Per-tenant RBAC for secrets. All secrets for a tenant are managed by anyone with tenant access.

## Alternatives Considered

1. **Add RLS-like WHERE clauses to Turso queries** — Would keep single-DB mode but doesn't provide true isolation. Rejected: defeats the purpose of database-per-tenant.
2. **Abstract all metadata behind a `MetadataStore` trait** — Clean but requires major refactoring. Rejected: the existing enum dispatch (`MetadataBackend`) works; only the routing is broken.
