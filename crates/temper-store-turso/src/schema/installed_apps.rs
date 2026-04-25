/// Tracks which OS apps are installed per tenant (workspace).
///
/// On boot, `restore_registry_from_turso()` reads the `specs` table to reload
/// entity types. This table provides durable metadata for bounded reconcile.
pub const CREATE_TENANT_INSTALLED_APPS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS tenant_installed_apps (
    tenant_id TEXT NOT NULL, app_name TEXT NOT NULL, app_version TEXT NOT NULL DEFAULT '',
    bundle_digest TEXT NOT NULL DEFAULT '', spec_digest TEXT NOT NULL DEFAULT '',
    policy_digest TEXT NOT NULL DEFAULT '', wasm_digest TEXT NOT NULL DEFAULT '',
    content_digest TEXT NOT NULL DEFAULT '', seed_digest TEXT NOT NULL DEFAULT '',
    installed_at TEXT NOT NULL DEFAULT (datetime('now')),
    last_reconciled_at TEXT, status TEXT NOT NULL DEFAULT 'installed',
    PRIMARY KEY (tenant_id, app_name)
);";

pub const ALTER_INSTALLED_APPS_ADD_APP_VERSION: &str =
    "ALTER TABLE tenant_installed_apps ADD COLUMN app_version TEXT NOT NULL DEFAULT ''";
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
