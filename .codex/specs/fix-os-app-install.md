# Fix: OS App Install — Two Bugs

## Bug 1: provision_cloud_database fails on existing DB (409)

### Root Cause
tenant_api::install_os_app (HTTP handler) calls router.ensure_tenant() before installing.
ensure_tenant checks: (1) in-memory map, (2) tenant_registry table, (3) falls through to
register_tenant → provision_cloud_database.

provision_cloud_database calls POST /v1/organizations/{org}/databases to create temper-{tenant_id}.
If DB already exists (409 Conflict), it returns Err instead of recovering.

Affects any tenant whose DB was provisioned outside the registry flow (e.g. default bootstrapped via CLI).

### Fix
In crates/temper-store-turso/src/router.rs, provision_cloud_database:

1. After receiving 409, call GET /v1/organizations/{org}/databases/{db_name} to retrieve hostname
2. Create auth token via POST /v1/organizations/{org}/databases/{db_name}/auth/tokens
3. Return (db_url, Some(auth_token)) — same as create path
4. In register_tenant, after provision_database succeeds (including 409 recovery), insert into tenant_registry

This makes ensure_tenant truly idempotent.

### Tests
- ensure_tenant on already-provisioned tenant returns Ok (not 409)
- calling ensure_tenant twice for same tenant is idempotent

---

## Bug 2: OS app install replaces tenant specs instead of merging

### Root Cause
os_apps::install_os_app calls bootstrap::bootstrap_tenant_specs which calls
registry.register_tenant() → try_register_tenant_with_reactions_and_constraints with merge: false.

When merge: false and tenant already exists:
  existing_config.csdl = Arc::new(csdl);        // REPLACES all types
  existing_config.entity_set_map = entity_set_map; // REPLACES all sets

Installing agent-orchestration into rita-agents deleted Issue/Project/Cycle/Comment/Label.

### Fix
In crates/temper-platform/src/bootstrap.rs, bootstrap_tenant_specs:

1. Add a merge: bool parameter
2. When called from os_apps::install_os_app → pass merge: true
3. When called from system bootstrap or CLI --app → keep merge: false (directory is truth)
4. Pass merge through to try_register_tenant_with_reactions_and_constraints

The merge path already exists and works — it merges CSDLs and adds entity_set_map entries
without removing existing ones.

Additionally install_os_app must merge CSDLs: read existing tenant CSDL from registry,
merge new app types into it, pass merged CSDL to bootstrap_tenant_specs.

### Tests
- Install project-management into tenant X
- Install agent-orchestration into same tenant X
- Assert ALL entity types from BOTH apps are present
- Assert existing entity data still accessible
- Install project-management again — assert idempotent (no duplicate types)

---

## Files to Change
- crates/temper-store-turso/src/router.rs — provision_cloud_database 409 handling
- crates/temper-platform/src/os_apps.rs — pass merge=true to registry
- crates/temper-platform/src/bootstrap.rs — add merge parameter to bootstrap_tenant_specs
- crates/temper-platform/src/os_apps/tests.rs — multi-app install test
- crates/temper-store-turso/src/router.rs tests — idempotent ensure_tenant test
