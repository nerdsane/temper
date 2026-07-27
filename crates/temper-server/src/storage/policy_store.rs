use temper_runtime::persistence::PersistenceError;
use temper_store_postgres::{PostgresEventStore, PostgresPolicySnapshotEntry};
use temper_store_turso::{
    PolicyRow as TursoPolicyRow, PolicySnapshotEntry as TursoPolicySnapshotEntry,
    TenantStoreRouter, TursoEventStore,
};

/// Backend-neutral row for one granular Cedar policy entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PolicyStoreRow {
    pub tenant: String,
    pub policy_id: String,
    pub cedar_text: String,
    pub policy_hash: String,
    pub created_at: String,
    pub created_by: String,
    pub enabled: bool,
}

/// Complete durable policy set read from one publication version.
#[derive(Clone, Debug)]
pub struct PolicySnapshot {
    pub version: u64,
    pub rows: Vec<PolicyStoreRow>,
}

fn validate_policy_snapshot_tenant(tenant: &str, rows: &[PolicyStoreRow]) -> Result<(), String> {
    if let Some(row) = rows.iter().find(|row| row.tenant != tenant) {
        return Err(format!(
            "policy snapshot row {:?} belongs to tenant {:?}, expected {:?}",
            row.policy_id, row.tenant, tenant
        ));
    }
    Ok(())
}

impl From<TursoPolicyRow> for PolicyStoreRow {
    fn from(row: TursoPolicyRow) -> Self {
        Self {
            tenant: row.tenant,
            policy_id: row.policy_id,
            cedar_text: row.cedar_text,
            policy_hash: row.policy_hash,
            created_at: row.created_at,
            created_by: row.created_by,
            enabled: row.enabled,
        }
    }
}

impl From<temper_store_postgres::PostgresPolicyRow> for PolicyStoreRow {
    fn from(row: temper_store_postgres::PostgresPolicyRow) -> Self {
        Self {
            tenant: row.tenant,
            policy_id: row.policy_id,
            cedar_text: row.cedar_text,
            policy_hash: row.policy_hash,
            created_at: row.created_at,
            created_by: row.created_by,
            enabled: row.enabled,
        }
    }
}

/// Granular Cedar policy persistence capability.
#[async_trait::async_trait]
pub trait PolicyStore: Send + Sync {
    /// Load the complete tenant policy set and its publication head atomically.
    async fn load_policy_snapshot(&self, tenant: &str) -> Result<PolicySnapshot, PersistenceError>;

    /// Atomically replace the complete tenant policy set at an expected version.
    async fn replace_policy_snapshot(
        &self,
        tenant: &str,
        expected_version: u64,
        rows: Vec<PolicyStoreRow>,
    ) -> Result<u64, PersistenceError>;

    async fn load_policies_for_tenant(&self, tenant: &str) -> Result<Vec<PolicyStoreRow>, String>;

    async fn load_all_policies(&self) -> Result<Vec<PolicyStoreRow>, String>;
}

#[async_trait::async_trait]
impl PolicyStore for PostgresEventStore {
    async fn load_policy_snapshot(&self, tenant: &str) -> Result<PolicySnapshot, PersistenceError> {
        PostgresEventStore::load_policy_snapshot(self, tenant)
            .await
            .map(|snapshot| PolicySnapshot {
                version: snapshot.version,
                rows: snapshot
                    .rows
                    .into_iter()
                    .map(PolicyStoreRow::from)
                    .collect(),
            })
    }

    async fn replace_policy_snapshot(
        &self,
        tenant: &str,
        expected_version: u64,
        rows: Vec<PolicyStoreRow>,
    ) -> Result<u64, PersistenceError> {
        validate_policy_snapshot_tenant(tenant, &rows).map_err(PersistenceError::Serialization)?;
        let entries = rows
            .into_iter()
            .map(|row| PostgresPolicySnapshotEntry {
                policy_id: row.policy_id,
                cedar_text: row.cedar_text,
                created_at: row.created_at,
                created_by: row.created_by,
                enabled: row.enabled,
            })
            .collect();
        PostgresEventStore::replace_policy_snapshot(self, tenant, expected_version, entries).await
    }

    async fn load_policies_for_tenant(&self, tenant: &str) -> Result<Vec<PolicyStoreRow>, String> {
        self.load_policies_for_tenant(tenant)
            .await
            .map(|rows| rows.into_iter().map(PolicyStoreRow::from).collect())
            .map_err(|e| e.to_string())
    }

    async fn load_all_policies(&self) -> Result<Vec<PolicyStoreRow>, String> {
        self.load_all_policies()
            .await
            .map(|rows| rows.into_iter().map(PolicyStoreRow::from).collect())
            .map_err(|e| e.to_string())
    }
}

#[async_trait::async_trait]
impl PolicyStore for TursoEventStore {
    async fn load_policy_snapshot(&self, tenant: &str) -> Result<PolicySnapshot, PersistenceError> {
        TursoEventStore::load_policy_snapshot(self, tenant)
            .await
            .map(|snapshot| PolicySnapshot {
                version: snapshot.version,
                rows: snapshot
                    .rows
                    .into_iter()
                    .map(PolicyStoreRow::from)
                    .collect(),
            })
    }

    async fn replace_policy_snapshot(
        &self,
        tenant: &str,
        expected_version: u64,
        rows: Vec<PolicyStoreRow>,
    ) -> Result<u64, PersistenceError> {
        validate_policy_snapshot_tenant(tenant, &rows).map_err(PersistenceError::Serialization)?;
        let entries = rows
            .into_iter()
            .map(|row| TursoPolicySnapshotEntry {
                policy_id: row.policy_id,
                cedar_text: row.cedar_text,
                created_at: row.created_at,
                created_by: row.created_by,
                enabled: row.enabled,
            })
            .collect();
        TursoEventStore::replace_policy_snapshot(self, tenant, expected_version, entries).await
    }

    async fn load_policies_for_tenant(&self, tenant: &str) -> Result<Vec<PolicyStoreRow>, String> {
        self.load_policies_for_tenant(tenant)
            .await
            .map(|rows| rows.into_iter().map(PolicyStoreRow::from).collect())
            .map_err(|e| e.to_string())
    }

    async fn load_all_policies(&self) -> Result<Vec<PolicyStoreRow>, String> {
        self.load_all_policies()
            .await
            .map(|rows| rows.into_iter().map(PolicyStoreRow::from).collect())
            .map_err(|e| e.to_string())
    }
}

#[async_trait::async_trait]
impl PolicyStore for TenantStoreRouter {
    async fn load_policy_snapshot(&self, tenant: &str) -> Result<PolicySnapshot, PersistenceError> {
        let store = self.store_for_tenant(tenant).await?;
        PolicyStore::load_policy_snapshot(&store, tenant).await
    }

    async fn replace_policy_snapshot(
        &self,
        tenant: &str,
        expected_version: u64,
        rows: Vec<PolicyStoreRow>,
    ) -> Result<u64, PersistenceError> {
        let store = self.store_for_tenant(tenant).await?;
        PolicyStore::replace_policy_snapshot(&store, tenant, expected_version, rows).await
    }

    async fn load_policies_for_tenant(&self, tenant: &str) -> Result<Vec<PolicyStoreRow>, String> {
        let store = self
            .store_for_tenant(tenant)
            .await
            .map_err(|e| e.to_string())?;
        store
            .load_policies_for_tenant(tenant)
            .await
            .map(|rows| rows.into_iter().map(PolicyStoreRow::from).collect())
            .map_err(|e| e.to_string())
    }

    async fn load_all_policies(&self) -> Result<Vec<PolicyStoreRow>, String> {
        let mut rows: Vec<PolicyStoreRow> = self
            .platform_store()
            .load_all_policies()
            .await
            .map(|rows| rows.into_iter().map(PolicyStoreRow::from).collect())
            .map_err(|e| e.to_string())?;
        for tenant_id in self.connected_tenants().await {
            if let Ok(store) = self.store_for_tenant(&tenant_id).await {
                let mut tenant_rows: Vec<PolicyStoreRow> = store
                    .load_all_policies()
                    .await
                    .map(|rows| rows.into_iter().map(PolicyStoreRow::from).collect())
                    .map_err(|e| e.to_string())?;
                rows.append(&mut tenant_rows);
            }
        }
        Ok(rows)
    }
}
