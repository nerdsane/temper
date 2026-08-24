/// Tracks which OS apps are installed per tenant (workspace).
///
/// On boot, `restore_registry_from_turso()` reads the `specs` table to reload
/// entity types. This table provides durable metadata for bounded reconcile.
pub const CREATE_TENANT_INSTALLED_APPS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS tenant_installed_apps (
    tenant_id TEXT NOT NULL, app_name TEXT NOT NULL, app_version TEXT NOT NULL DEFAULT '',
    source_kind TEXT NOT NULL DEFAULT 'local', app_ref TEXT NOT NULL DEFAULT '',
    version_hash TEXT NOT NULL DEFAULT '', pinned_version_hash TEXT NOT NULL DEFAULT '',
    current_version_hash TEXT NOT NULL DEFAULT '', follow_policy TEXT NOT NULL DEFAULT 'pinned',
    closure_id TEXT NOT NULL DEFAULT '',
    registry_url TEXT NOT NULL DEFAULT '', registry_tenant TEXT NOT NULL DEFAULT '',
    dependency_lock_digest TEXT NOT NULL DEFAULT '',
    bundle_digest TEXT NOT NULL DEFAULT '', spec_digest TEXT NOT NULL DEFAULT '',
    policy_digest TEXT NOT NULL DEFAULT '', wasm_digest TEXT NOT NULL DEFAULT '',
    content_digest TEXT NOT NULL DEFAULT '', seed_digest TEXT NOT NULL DEFAULT '',
    installed_at TEXT NOT NULL DEFAULT (datetime('now')),
    last_reconciled_at TEXT, status TEXT NOT NULL DEFAULT 'installed',
    PRIMARY KEY (tenant_id, app_name)
);";

pub const ALTER_INSTALLED_APPS_ADD_APP_VERSION: &str =
    "ALTER TABLE tenant_installed_apps ADD COLUMN app_version TEXT NOT NULL DEFAULT ''";
pub const ALTER_INSTALLED_APPS_ADD_SOURCE_KIND: &str =
    "ALTER TABLE tenant_installed_apps ADD COLUMN source_kind TEXT NOT NULL DEFAULT 'local'";
pub const ALTER_INSTALLED_APPS_ADD_APP_REF: &str =
    "ALTER TABLE tenant_installed_apps ADD COLUMN app_ref TEXT NOT NULL DEFAULT ''";
pub const ALTER_INSTALLED_APPS_ADD_VERSION_HASH: &str =
    "ALTER TABLE tenant_installed_apps ADD COLUMN version_hash TEXT NOT NULL DEFAULT ''";
pub const ALTER_INSTALLED_APPS_ADD_PINNED_VERSION_HASH: &str =
    "ALTER TABLE tenant_installed_apps ADD COLUMN pinned_version_hash TEXT NOT NULL DEFAULT ''";
pub const ALTER_INSTALLED_APPS_ADD_CURRENT_VERSION_HASH: &str =
    "ALTER TABLE tenant_installed_apps ADD COLUMN current_version_hash TEXT NOT NULL DEFAULT ''";
pub const ALTER_INSTALLED_APPS_ADD_FOLLOW_POLICY: &str =
    "ALTER TABLE tenant_installed_apps ADD COLUMN follow_policy TEXT NOT NULL DEFAULT 'pinned'";
pub const ALTER_INSTALLED_APPS_ADD_CLOSURE_ID: &str =
    "ALTER TABLE tenant_installed_apps ADD COLUMN closure_id TEXT NOT NULL DEFAULT ''";
pub const ALTER_INSTALLED_APPS_ADD_REGISTRY_URL: &str =
    "ALTER TABLE tenant_installed_apps ADD COLUMN registry_url TEXT NOT NULL DEFAULT ''";
pub const ALTER_INSTALLED_APPS_ADD_REGISTRY_TENANT: &str =
    "ALTER TABLE tenant_installed_apps ADD COLUMN registry_tenant TEXT NOT NULL DEFAULT ''";
pub const ALTER_INSTALLED_APPS_ADD_DEPENDENCY_LOCK_DIGEST: &str =
    "ALTER TABLE tenant_installed_apps ADD COLUMN dependency_lock_digest TEXT NOT NULL DEFAULT ''";
pub const ALTER_INSTALLED_APPS_ADD_BUNDLE_DIGEST: &str =
    "ALTER TABLE tenant_installed_apps ADD COLUMN bundle_digest TEXT NOT NULL DEFAULT ''";
pub const ALTER_INSTALLED_APPS_ADD_SPEC_DIGEST: &str =
    "ALTER TABLE tenant_installed_apps ADD COLUMN spec_digest TEXT NOT NULL DEFAULT ''";
pub const ALTER_INSTALLED_APPS_ADD_POLICY_DIGEST: &str =
    "ALTER TABLE tenant_installed_apps ADD COLUMN policy_digest TEXT NOT NULL DEFAULT ''";
pub const ALTER_INSTALLED_APPS_ADD_WASM_DIGEST: &str =
    "ALTER TABLE tenant_installed_apps ADD COLUMN wasm_digest TEXT NOT NULL DEFAULT ''";
pub const ALTER_INSTALLED_APPS_ADD_CONTENT_DIGEST: &str =
    "ALTER TABLE tenant_installed_apps ADD COLUMN content_digest TEXT NOT NULL DEFAULT ''";
pub const ALTER_INSTALLED_APPS_ADD_SEED_DIGEST: &str =
    "ALTER TABLE tenant_installed_apps ADD COLUMN seed_digest TEXT NOT NULL DEFAULT ''";
pub const ALTER_INSTALLED_APPS_ADD_LAST_RECONCILED_AT: &str =
    "ALTER TABLE tenant_installed_apps ADD COLUMN last_reconciled_at TEXT";
pub const ALTER_INSTALLED_APPS_ADD_STATUS: &str =
    "ALTER TABLE tenant_installed_apps ADD COLUMN status TEXT NOT NULL DEFAULT 'installed'";
