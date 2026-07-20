//! Tenant-aware store router for database-per-tenant isolation.
//!
//! [`TenantStoreRouter`] manages a platform database plus per-tenant databases.
//! The platform DB holds shared state (tenant registry, user mappings, system
//! packages). Each tenant gets an isolated database with the full entity schema.
//!
//! In local/dev mode, tenant databases are `file:`-based SQLite files.
//! In cloud mode (with Turso Cloud API credentials), new tenant databases
//! are provisioned on demand.

use std::collections::BTreeMap;
use std::sync::Arc;

use tokio::sync::RwLock;
use tracing::{info, instrument, warn};

use temper_runtime::persistence::{
    EventStore, JournalRead, PersistenceAppend, PersistenceAppendResult, PersistenceEnvelope,
    PersistenceError, storage_error,
};
use temper_runtime::tenant::parse_persistence_id_parts;

use crate::TursoEventStore;
use crate::schema;

/// Routes storage operations to per-tenant Turso databases.
///
/// Holds a platform database (for tenant registry, user access, and shared
/// system packages) plus a lazily-populated map of tenant → `TursoEventStore`.
#[derive(Clone)]
pub struct TenantStoreRouter {
    /// Platform database — tenant registry, user mappings, system integrations.
    platform: TursoEventStore,
    /// Per-tenant database connections, keyed by tenant ID.
    tenants: Arc<RwLock<BTreeMap<String, TursoEventStore>>>,
    /// Turso Cloud API token for dynamic provisioning (optional).
    #[cfg(feature = "cloud")]
    turso_api_token: Option<String>,
    /// Turso Cloud organization slug.
    #[cfg(feature = "cloud")]
    turso_org: Option<String>,
    /// Turso Cloud database group for new databases.
    #[cfg(feature = "cloud")]
    turso_group: Option<String>,
    /// Turso Cloud API base URL.
    #[cfg(feature = "cloud")]
    turso_api_base_url: String,
    /// Base directory for local file-based tenant databases (dev mode).
    local_base_dir: Option<String>,
}

impl std::fmt::Debug for TenantStoreRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TenantStoreRouter")
            .field("platform", &"<TursoEventStore>")
            .field("tenants", &"<RwLock<BTreeMap>>")
            .finish()
    }
}

/// A row from the `tenant_registry` table.
#[derive(Debug, Clone)]
pub struct TenantRegistryRow {
    pub tenant_id: String,
    pub turso_db_url: String,
    pub turso_auth_token: Option<String>,
    pub status: String,
}

/// A row from the `tenant_users` table.
#[derive(Debug, Clone)]
pub struct TenantUserRow {
    pub tenant_id: String,
    pub user_id: String,
    pub role: String,
}

impl TenantStoreRouter {
    /// Connect to the platform database and load existing tenant connections.
    ///
    /// # Arguments
    ///
    /// * `platform_url` — URL for the platform database (e.g., `libsql://...` or `file:platform.db`)
    /// * `platform_token` — Auth token for remote platform DB (None for local)
    /// * `local_base_dir` — Base directory for local tenant DBs (dev mode, e.g., `.temper/tenants/`)
    #[instrument(skip_all, fields(otel.name = "router.new"))]
    pub async fn new(
        platform_url: &str,
        platform_token: Option<&str>,
        local_base_dir: Option<String>,
    ) -> Result<Self, PersistenceError> {
        let platform = TursoEventStore::new(platform_url, platform_token).await?;

        // Run platform-specific migrations (tenant registry + user tables).
        Self::migrate_platform(&platform).await?;

        let router = Self {
            platform,
            tenants: Arc::new(RwLock::new(BTreeMap::new())),
            #[cfg(feature = "cloud")]
            turso_api_token: None,
            #[cfg(feature = "cloud")]
            turso_org: None,
            #[cfg(feature = "cloud")]
            turso_group: None,
            #[cfg(feature = "cloud")]
            turso_api_base_url: "https://api.turso.tech".to_string(),
            local_base_dir,
        };

        // Pre-connect to all registered tenants.
        router.connect_registered_tenants().await?;

        Ok(router)
    }

    /// Configure Turso Cloud API credentials for dynamic provisioning.
    #[cfg(feature = "cloud")]
    pub fn with_cloud_config(
        mut self,
        api_token: String,
        org: String,
        group: Option<String>,
    ) -> Self {
        self.turso_api_token = Some(api_token);
        self.turso_org = Some(org);
        self.turso_group = group;
        self
    }

    /// Access the platform store directly (for shared system packages, user lookups).
    pub fn platform_store(&self) -> &TursoEventStore {
        &self.platform
    }

    /// Get the store for a specific tenant.
    ///
    /// Returns the tenant-specific store if connected, or attempts to connect
    /// from the registry. For the special `temper-system` tenant, returns the
    /// platform store.
    #[instrument(skip_all, fields(tenant, otel.name = "router.store_for_tenant"))]
    pub async fn store_for_tenant(
        &self,
        tenant: &str,
    ) -> Result<TursoEventStore, PersistenceError> {
        // System tenant uses the platform DB.
        if tenant == "temper-system" {
            return Ok(self.platform.clone());
        }

        // Check cache first (read lock).
        {
            let tenants = self.tenants.read().await;
            if let Some(store) = tenants.get(tenant) {
                return Ok(store.clone());
            }
        }

        // Not cached — try to connect from registry.
        self.connect_tenant(tenant).await
    }

    /// List all registered tenant IDs.
    #[instrument(skip_all, fields(otel.name = "router.list_tenants"))]
    pub async fn list_tenants(&self) -> Result<Vec<String>, PersistenceError> {
        let rows = self.load_tenant_registry().await?;
        Ok(rows
            .into_iter()
            .filter(|r| r.status == "active")
            .map(|r| r.tenant_id)
            .collect())
    }

    /// List tenants accessible by a given user ID (e.g., `github:username`).
    #[instrument(skip_all, fields(user_id, otel.name = "router.tenants_for_user"))]
    pub async fn tenants_for_user(
        &self,
        user_id: &str,
    ) -> Result<Vec<TenantUserRow>, PersistenceError> {
        let conn = self.platform.connection().map_err(storage_error)?;
        let mut rows = conn
            .query(
                "SELECT tenant_id, user_id, role FROM tenant_users WHERE user_id = ?1",
                libsql::params![user_id],
            )
            .await
            .map_err(storage_error)?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            out.push(TenantUserRow {
                tenant_id: row.get::<String>(0).map_err(storage_error)?,
                user_id: row.get::<String>(1).map_err(storage_error)?,
                role: row.get::<String>(2).map_err(storage_error)?,
            });
        }
        Ok(out)
    }

    /// Add a user to a tenant.
    #[instrument(skip_all, fields(tenant_id, user_id, otel.name = "router.add_tenant_user"))]
    pub async fn add_tenant_user(
        &self,
        tenant_id: &str,
        user_id: &str,
        role: &str,
    ) -> Result<(), PersistenceError> {
        let conn = self.platform.connection().map_err(storage_error)?;
        conn.execute(
            "INSERT OR REPLACE INTO tenant_users (tenant_id, user_id, role) VALUES (?1, ?2, ?3)",
            libsql::params![tenant_id, user_id, role],
        )
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    /// List all users for a specific tenant.
    #[instrument(skip_all, fields(tenant_id, otel.name = "router.list_tenant_users"))]
    pub async fn list_tenant_users(
        &self,
        tenant_id: &str,
    ) -> Result<Vec<TenantUserRow>, PersistenceError> {
        let conn = self.platform.connection().map_err(storage_error)?;
        let mut rows = conn
            .query(
                "SELECT tenant_id, user_id, role FROM tenant_users WHERE tenant_id = ?1",
                libsql::params![tenant_id],
            )
            .await
            .map_err(storage_error)?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            out.push(TenantUserRow {
                tenant_id: row.get::<String>(0).map_err(storage_error)?,
                user_id: row.get::<String>(1).map_err(storage_error)?,
                role: row.get::<String>(2).map_err(storage_error)?,
            });
        }
        Ok(out)
    }

    /// Remove a tenant entirely.
    ///
    /// Deletes the tenant from `tenant_registry`, removes associated users,
    /// and evicts the in-memory store connection.
    #[instrument(skip_all, fields(tenant_id, otel.name = "router.remove_tenant"))]
    pub async fn remove_tenant(&self, tenant_id: &str) -> Result<bool, PersistenceError> {
        let conn = self.platform.connection().map_err(storage_error)?;

        // Delete associated users and installed apps first.
        conn.execute(
            "DELETE FROM tenant_users WHERE tenant_id = ?1",
            libsql::params![tenant_id],
        )
        .await
        .map_err(storage_error)?;
        conn.execute(
            "DELETE FROM tenant_installed_apps WHERE tenant_id = ?1",
            libsql::params![tenant_id],
        )
        .await
        .map_err(storage_error)?;

        // Delete from registry.
        let result = conn
            .execute(
                "DELETE FROM tenant_registry WHERE tenant_id = ?1",
                libsql::params![tenant_id],
            )
            .await
            .map_err(storage_error)?;

        // Evict from in-memory cache.
        self.tenants.write().await.remove(tenant_id);

        let removed = result > 0;
        if removed {
            info!(tenant_id, "Tenant removed from registry");
        }
        Ok(removed)
    }

    /// Remove a user from a tenant.
    #[instrument(skip_all, fields(tenant_id, user_id, otel.name = "router.remove_tenant_user"))]
    pub async fn remove_tenant_user(
        &self,
        tenant_id: &str,
        user_id: &str,
    ) -> Result<(), PersistenceError> {
        let conn = self.platform.connection().map_err(storage_error)?;
        conn.execute(
            "DELETE FROM tenant_users WHERE tenant_id = ?1 AND user_id = ?2",
            libsql::params![tenant_id, user_id],
        )
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    /// Ensure a tenant exists in the persistence layer.
    ///
    /// If the tenant is already registered, returns `Ok(true)` (already existed).
    /// If not, provisions a new database and registers it, returning `Ok(false)`.
    #[instrument(skip_all, fields(tenant_id, otel.name = "router.ensure_tenant"))]
    pub async fn ensure_tenant(&self, tenant_id: &str) -> Result<bool, PersistenceError> {
        {
            let tenants = self.tenants.read().await;
            if tenants.contains_key(tenant_id) {
                return Ok(true);
            }
        }
        // Not in memory — check if in DB but not yet connected.
        let conn = self.platform.connection().map_err(storage_error)?;
        let mut rows = conn
            .query(
                "SELECT turso_db_url, turso_auth_token FROM tenant_registry WHERE tenant_id = ?1",
                libsql::params![tenant_id],
            )
            .await
            .map_err(storage_error)?;
        if let Some(row) = rows.next().await.map_err(storage_error)? {
            // Exists in DB but not connected — reconnect.
            let db_url: String = row.get::<String>(0).map_err(storage_error)?;
            let auth_token: Option<String> = row.get::<Option<String>>(1).ok().flatten();
            let store = TursoEventStore::new(&db_url, auth_token.as_deref()).await?;
            self.tenants
                .write()
                .await
                .insert(tenant_id.to_string(), store);
            return Ok(true);
        }
        // Not in DB either — provision new.
        self.register_tenant(tenant_id).await?;
        Ok(false)
    }

    /// Register and connect a new tenant.
    ///
    /// In local mode, creates a new SQLite file in `local_base_dir`.
    /// In cloud mode (with `cloud` feature), provisions via Turso Cloud API.
    #[instrument(skip_all, fields(tenant_id, otel.name = "router.register_tenant"))]
    pub async fn register_tenant(
        &self,
        tenant_id: &str,
    ) -> Result<TursoEventStore, PersistenceError> {
        // Check if already registered.
        {
            let tenants = self.tenants.read().await;
            if tenants.contains_key(tenant_id) {
                return Err(PersistenceError::Storage(format!(
                    "tenant '{tenant_id}' already exists"
                )));
            }
        }

        let (db_url, auth_token) = self.provision_database(tenant_id).await?;

        // Connect first (before moving values into params).
        let store = TursoEventStore::new(&db_url, auth_token.as_deref()).await?;

        // Register in platform DB.
        let conn = self.platform.connection().map_err(storage_error)?;
        conn.execute(
            "INSERT INTO tenant_registry (tenant_id, turso_db_url, turso_auth_token)
             VALUES (?1, ?2, ?3)",
            libsql::params![tenant_id, db_url, auth_token],
        )
        .await
        .map_err(storage_error)?;
        self.tenants
            .write()
            .await
            .insert(tenant_id.to_string(), store.clone());

        info!(tenant_id, "Registered and connected new tenant");
        Ok(store)
    }

    // ── Private helpers ──────────────────────────────────────────────────

    /// Run platform-specific schema migrations.
    async fn migrate_platform(store: &TursoEventStore) -> Result<(), PersistenceError> {
        let conn = store.connection().map_err(storage_error)?;
        conn.execute(schema::CREATE_TENANT_REGISTRY_TABLE, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_TENANT_USERS_TABLE, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_TENANT_USERS_USER_INDEX, ())
            .await
            .map_err(storage_error)?;
        Ok(())
    }
}

mod event_store;
mod management;

#[cfg(test)]
mod tests;
