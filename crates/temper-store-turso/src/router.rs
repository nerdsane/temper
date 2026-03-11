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
    EventStore, PersistenceEnvelope, PersistenceError, storage_error,
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
        if tenant == "temper-system" || tenant == "default" {
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

        // Delete associated users first.
        conn.execute(
            "DELETE FROM tenant_users WHERE tenant_id = ?1",
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

    /// Load all tenant registry rows from the platform DB.
    async fn load_tenant_registry(&self) -> Result<Vec<TenantRegistryRow>, PersistenceError> {
        let conn = self.platform.connection().map_err(storage_error)?;
        let mut rows = conn
            .query(
                "SELECT tenant_id, turso_db_url, turso_auth_token, status
                 FROM tenant_registry ORDER BY tenant_id",
                (),
            )
            .await
            .map_err(storage_error)?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            out.push(TenantRegistryRow {
                tenant_id: row.get::<String>(0).map_err(storage_error)?,
                turso_db_url: row.get::<String>(1).map_err(storage_error)?,
                turso_auth_token: row.get::<Option<String>>(2).ok().flatten(),
                status: row.get::<String>(3).map_err(storage_error)?,
            });
        }
        Ok(out)
    }

    /// Pre-connect to all active tenants in the registry.
    async fn connect_registered_tenants(&self) -> Result<(), PersistenceError> {
        let registry = self.load_tenant_registry().await?;
        let mut tenants = self.tenants.write().await;

        for entry in &registry {
            if entry.status != "active" {
                continue;
            }
            match TursoEventStore::new(&entry.turso_db_url, entry.turso_auth_token.as_deref()).await
            {
                Ok(store) => {
                    tenants.insert(entry.tenant_id.clone(), store);
                    info!(tenant = %entry.tenant_id, "Connected to tenant database");
                }
                Err(e) => {
                    warn!(
                        tenant = %entry.tenant_id,
                        error = %e,
                        "Failed to connect to tenant database, skipping"
                    );
                }
            }
        }
        Ok(())
    }

    /// Connect to a tenant from the registry (not cached yet).
    async fn connect_tenant(&self, tenant_id: &str) -> Result<TursoEventStore, PersistenceError> {
        let registry = self.load_tenant_registry().await?;
        let entry = registry
            .iter()
            .find(|r| r.tenant_id == tenant_id && r.status == "active")
            .ok_or_else(|| {
                PersistenceError::Storage(format!("tenant '{tenant_id}' not found in registry"))
            })?;

        let store =
            TursoEventStore::new(&entry.turso_db_url, entry.turso_auth_token.as_deref()).await?;
        self.tenants
            .write()
            .await
            .insert(tenant_id.to_string(), store.clone());

        info!(tenant = tenant_id, "Connected to tenant database on demand");
        Ok(store)
    }

    /// Provision a new database for a tenant.
    ///
    /// In local mode: creates a `file:{base_dir}/{tenant_id}.db` SQLite file.
    /// In cloud mode: calls the Turso Cloud API to create a database.
    async fn provision_database(
        &self,
        tenant_id: &str,
    ) -> Result<(String, Option<String>), PersistenceError> {
        #[cfg(feature = "cloud")]
        if let (Some(api_token), Some(org)) = (&self.turso_api_token, &self.turso_org) {
            return self
                .provision_cloud_database(tenant_id, api_token, org)
                .await;
        }

        // Local mode: create a file-based database.
        let base_dir = self.local_base_dir.as_deref().unwrap_or(".temper/tenants");

        std::fs::create_dir_all(base_dir).map_err(|e| {
            PersistenceError::Storage(format!("failed to create tenant directory {base_dir}: {e}"))
        })?;

        let db_url = format!("file:{base_dir}/{tenant_id}.db");
        info!(tenant_id, db_url, "Provisioned local tenant database");
        Ok((db_url, None))
    }

    /// Provision a database via the Turso Cloud Platform API.
    #[cfg(feature = "cloud")]
    async fn provision_cloud_database(
        &self,
        tenant_id: &str,
        api_token: &str,
        org: &str,
    ) -> Result<(String, Option<String>), PersistenceError> {
        let client = reqwest::Client::new();

        // Sanitize tenant ID for use as a database name (alphanumeric + hyphens).
        let db_name = format!("temper-{tenant_id}");

        let mut body = serde_json::json!({
            "name": db_name,
        });
        if let Some(group) = &self.turso_group {
            body["group"] = serde_json::Value::String(group.clone());
        }

        // Create the database.
        let resp = client
            .post(format!(
                "https://api.turso.tech/v1/organizations/{org}/databases"
            ))
            .bearer_auth(api_token)
            .json(&body)
            .send()
            .await
            .map_err(|e| PersistenceError::Storage(format!("Turso API request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body_text = resp
                .text()
                .await
                .unwrap_or_else(|_| "<no body>".to_string());
            return Err(PersistenceError::Storage(format!(
                "Turso API returned {status}: {body_text}"
            )));
        }

        let create_resp: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| PersistenceError::Storage(format!("Turso API response parse: {e}")))?;

        let hostname = create_resp["database"]["Hostname"]
            .as_str()
            .or_else(|| create_resp["database"]["hostname"].as_str())
            .ok_or_else(|| {
                PersistenceError::Storage(format!(
                    "Turso API missing hostname in response: {create_resp}"
                ))
            })?;

        let db_url = format!("libsql://{hostname}");

        // Create an auth token for the new database.
        let token_resp = client
            .post(format!(
                "https://api.turso.tech/v1/organizations/{org}/databases/{db_name}/auth/tokens"
            ))
            .bearer_auth(api_token)
            .json(&serde_json::json!({}))
            .send()
            .await
            .map_err(|e| {
                PersistenceError::Storage(format!("Turso API token request failed: {e}"))
            })?;

        if !token_resp.status().is_success() {
            let status = token_resp.status();
            let body_text = token_resp
                .text()
                .await
                .unwrap_or_else(|_| "<no body>".to_string());
            return Err(PersistenceError::Storage(format!(
                "Turso API token creation returned {status}: {body_text}"
            )));
        }

        let token_json: serde_json::Value = token_resp
            .json()
            .await
            .map_err(|e| PersistenceError::Storage(format!("Turso token parse: {e}")))?;

        let auth_token = token_json["jwt"]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| {
                PersistenceError::Storage(format!(
                    "Turso API missing jwt in token response: {token_json}"
                ))
            })?;

        info!(tenant_id, db_url, "Provisioned Turso Cloud tenant database");
        Ok((db_url, Some(auth_token)))
    }

    /// List all connected tenant IDs (cached connections only).
    pub async fn connected_tenants(&self) -> Vec<String> {
        self.tenants.read().await.keys().cloned().collect()
    }
}

/// `EventStore` implementation that routes by tenant extracted from `persistence_id`.
impl EventStore for TenantStoreRouter {
    #[instrument(skip_all, fields(persistence_id, otel.name = "router.append"))]
    async fn append(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
    ) -> Result<u64, PersistenceError> {
        let (tenant, _, _) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let store = self.store_for_tenant(tenant).await?;
        store
            .append(persistence_id, expected_sequence, events)
            .await
    }

    #[instrument(skip_all, fields(persistence_id, otel.name = "router.read_events"))]
    async fn read_events(
        &self,
        persistence_id: &str,
        from_sequence: u64,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        let (tenant, _, _) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let store = self.store_for_tenant(tenant).await?;
        store.read_events(persistence_id, from_sequence).await
    }

    #[instrument(skip_all, fields(persistence_id, otel.name = "router.save_snapshot"))]
    async fn save_snapshot(
        &self,
        persistence_id: &str,
        sequence_nr: u64,
        snapshot: &[u8],
    ) -> Result<(), PersistenceError> {
        let (tenant, _, _) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let store = self.store_for_tenant(tenant).await?;
        store
            .save_snapshot(persistence_id, sequence_nr, snapshot)
            .await
    }

    #[instrument(skip_all, fields(persistence_id, otel.name = "router.load_snapshot"))]
    async fn load_snapshot(
        &self,
        persistence_id: &str,
    ) -> Result<Option<(u64, Vec<u8>)>, PersistenceError> {
        let (tenant, _, _) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let store = self.store_for_tenant(tenant).await?;
        store.load_snapshot(persistence_id).await
    }

    #[instrument(skip_all, fields(tenant, otel.name = "router.list_entity_ids"))]
    async fn list_entity_ids(
        &self,
        tenant: &str,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        let store = self.store_for_tenant(tenant).await?;
        store.list_entity_ids(tenant).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_router_local_dev() {
        let dir = tempfile::tempdir().expect("tempdir");
        let platform_path = dir.path().join("platform.db");
        let platform_url = format!("file:{}", platform_path.display());

        let router = TenantStoreRouter::new(
            &platform_url,
            None,
            Some(dir.path().join("tenants").to_string_lossy().to_string()),
        )
        .await
        .expect("router creation");

        // No tenants initially.
        let tenants = router.list_tenants().await.expect("list");
        assert!(tenants.is_empty());

        // Register a tenant.
        let _store = router.register_tenant("alpha").await.expect("register");

        // Verify it's registered.
        let tenants = router.list_tenants().await.expect("list");
        assert_eq!(tenants, vec!["alpha"]);

        // Write and read back through the router.
        let persistence_id = "alpha:Order:order-1";
        let events = vec![PersistenceEnvelope {
            sequence_nr: 1,
            event_type: "OrderCreated".to_string(),
            payload: serde_json::json!({"status": "Draft"}),
            metadata: temper_runtime::persistence::EventMetadata {
                event_id: uuid::Uuid::nil(),
                causation_id: uuid::Uuid::nil(),
                correlation_id: uuid::Uuid::nil(),
                timestamp: chrono::Utc::now(),
                actor_id: "test".to_string(),
            },
        }];
        let seq = router
            .append(persistence_id, 0, &events)
            .await
            .expect("append");
        assert_eq!(seq, 1);

        let read_back = router.read_events(persistence_id, 0).await.expect("read");
        assert_eq!(read_back.len(), 1);
        assert_eq!(read_back[0].event_type, "OrderCreated");

        // System tenant routes to platform DB.
        let sys_store = router
            .store_for_tenant("temper-system")
            .await
            .expect("system");
        // The platform store should work (it has the entity schema too).
        let entity_ids = sys_store
            .list_entity_ids("temper-system")
            .await
            .expect("list");
        assert!(entity_ids.is_empty());
    }

    #[tokio::test]
    async fn test_tenant_user_management() {
        let dir = tempfile::tempdir().expect("tempdir");
        let platform_path = dir.path().join("platform.db");
        let platform_url = format!("file:{}", platform_path.display());

        let router = TenantStoreRouter::new(&platform_url, None, None)
            .await
            .expect("router");

        // Add users.
        router
            .add_tenant_user("alpha", "github:alice", "admin")
            .await
            .expect("add user");
        router
            .add_tenant_user("alpha", "github:bob", "member")
            .await
            .expect("add user");
        router
            .add_tenant_user("beta", "github:alice", "member")
            .await
            .expect("add user");

        // Query by user.
        let alice_tenants = router
            .tenants_for_user("github:alice")
            .await
            .expect("query");
        assert_eq!(alice_tenants.len(), 2);

        let bob_tenants = router.tenants_for_user("github:bob").await.expect("query");
        assert_eq!(bob_tenants.len(), 1);
        assert_eq!(bob_tenants[0].tenant_id, "alpha");

        // Remove user.
        router
            .remove_tenant_user("alpha", "github:bob")
            .await
            .expect("remove");
        let bob_tenants = router.tenants_for_user("github:bob").await.expect("query");
        assert!(bob_tenants.is_empty());
    }
}
