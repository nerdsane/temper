//! Runtime storage stack boundary.
//!
//! `temper_runtime::persistence::EventStore` uses `impl Future` return types,
//! which is good for concrete backends but not dyn-object-safe. This module
//! provides the boxed adapter used by the server-facing storage stack so
//! backend selection is a composition step rather than business-code branching.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use temper_runtime::persistence::{EventStore, PersistenceEnvelope, PersistenceError};
use temper_store_postgres::{PostgresEventStore, PostgresPolicyRow, PostgresTrajectoryInsert};
use temper_store_turso::{
    PolicyRow as TursoPolicyRow, TenantStoreRouter, TursoEventStore, TursoTrajectoryInsert,
};

use crate::event_store::ServerEventStore;
use crate::platform_store::{
    InstalledAppRecord, PlatformStore, SpecRow, SpecVerificationUpdate, WasmModuleRow,
};
use crate::state::trajectory::{TrajectoryEntry, TrajectorySource};

pub type EventStoreFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Object-safe adapter for the runtime event journal.
pub trait DynEventStore: Send + Sync {
    fn append<'a>(
        &'a self,
        persistence_id: &'a str,
        expected_sequence: u64,
        events: &'a [PersistenceEnvelope],
    ) -> EventStoreFuture<'a, Result<u64, PersistenceError>>;

    fn read_events<'a>(
        &'a self,
        persistence_id: &'a str,
        from_sequence: u64,
    ) -> EventStoreFuture<'a, Result<Vec<PersistenceEnvelope>, PersistenceError>>;

    fn save_snapshot<'a>(
        &'a self,
        persistence_id: &'a str,
        sequence_nr: u64,
        snapshot: &'a [u8],
    ) -> EventStoreFuture<'a, Result<(), PersistenceError>>;

    fn load_snapshot<'a>(
        &'a self,
        persistence_id: &'a str,
    ) -> EventStoreFuture<'a, Result<Option<(u64, Vec<u8>)>, PersistenceError>>;

    fn list_entity_ids<'a>(
        &'a self,
        tenant: &'a str,
    ) -> EventStoreFuture<'a, Result<Vec<(String, String)>, PersistenceError>>;

    fn list_entity_ids_by_type<'a>(
        &'a self,
        tenant: &'a str,
        entity_type: &'a str,
    ) -> EventStoreFuture<'a, Result<Vec<String>, PersistenceError>>;
}

impl<T> DynEventStore for T
where
    T: EventStore,
{
    fn append<'a>(
        &'a self,
        persistence_id: &'a str,
        expected_sequence: u64,
        events: &'a [PersistenceEnvelope],
    ) -> EventStoreFuture<'a, Result<u64, PersistenceError>> {
        Box::pin(EventStore::append(
            self,
            persistence_id,
            expected_sequence,
            events,
        ))
    }

    fn read_events<'a>(
        &'a self,
        persistence_id: &'a str,
        from_sequence: u64,
    ) -> EventStoreFuture<'a, Result<Vec<PersistenceEnvelope>, PersistenceError>> {
        Box::pin(EventStore::read_events(self, persistence_id, from_sequence))
    }

    fn save_snapshot<'a>(
        &'a self,
        persistence_id: &'a str,
        sequence_nr: u64,
        snapshot: &'a [u8],
    ) -> EventStoreFuture<'a, Result<(), PersistenceError>> {
        Box::pin(EventStore::save_snapshot(
            self,
            persistence_id,
            sequence_nr,
            snapshot,
        ))
    }

    fn load_snapshot<'a>(
        &'a self,
        persistence_id: &'a str,
    ) -> EventStoreFuture<'a, Result<Option<(u64, Vec<u8>)>, PersistenceError>> {
        Box::pin(EventStore::load_snapshot(self, persistence_id))
    }

    fn list_entity_ids<'a>(
        &'a self,
        tenant: &'a str,
    ) -> EventStoreFuture<'a, Result<Vec<(String, String)>, PersistenceError>> {
        Box::pin(EventStore::list_entity_ids(self, tenant))
    }

    fn list_entity_ids_by_type<'a>(
        &'a self,
        tenant: &'a str,
        entity_type: &'a str,
    ) -> EventStoreFuture<'a, Result<Vec<String>, PersistenceError>> {
        Box::pin(EventStore::list_entity_ids_by_type(
            self,
            tenant,
            entity_type,
        ))
    }
}

/// Cloneable boxed event store handle.
#[derive(Clone)]
pub struct BoxedEventStore(Arc<dyn DynEventStore>);

impl BoxedEventStore {
    pub fn new<T>(store: T) -> Self
    where
        T: EventStore,
    {
        Self(Arc::new(store))
    }

    pub fn from_arc<T>(store: Arc<T>) -> Self
    where
        T: EventStore,
    {
        Self(store)
    }

    pub fn inner(&self) -> Arc<dyn DynEventStore> {
        self.0.clone()
    }

    pub async fn append(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
    ) -> Result<u64, PersistenceError> {
        self.0
            .append(persistence_id, expected_sequence, events)
            .await
    }

    pub async fn read_events(
        &self,
        persistence_id: &str,
        from_sequence: u64,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        self.0.read_events(persistence_id, from_sequence).await
    }

    pub async fn save_snapshot(
        &self,
        persistence_id: &str,
        sequence_nr: u64,
        snapshot: &[u8],
    ) -> Result<(), PersistenceError> {
        self.0
            .save_snapshot(persistence_id, sequence_nr, snapshot)
            .await
    }

    pub async fn load_snapshot(
        &self,
        persistence_id: &str,
    ) -> Result<Option<(u64, Vec<u8>)>, PersistenceError> {
        self.0.load_snapshot(persistence_id).await
    }

    pub async fn list_entity_ids(
        &self,
        tenant: &str,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        self.0.list_entity_ids(tenant).await
    }

    pub async fn list_entity_ids_by_type(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Vec<String>, PersistenceError> {
        self.0.list_entity_ids_by_type(tenant, entity_type).await
    }
}

/// Backend label used for metrics and operator-facing diagnostics only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendLabel {
    Postgres,
    Turso,
    Redis,
    TursoRouted,
    Sim,
}

impl BackendLabel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Postgres => "postgres",
            Self::Turso => "turso",
            Self::Redis => "redis",
            Self::TursoRouted => "turso-routed",
            Self::Sim => "sim",
        }
    }
}

impl From<&ServerEventStore> for BackendLabel {
    fn from(store: &ServerEventStore) -> Self {
        match store {
            ServerEventStore::Postgres(_) => Self::Postgres,
            ServerEventStore::Turso(_) => Self::Turso,
            ServerEventStore::Redis(_) => Self::Redis,
            ServerEventStore::TenantRouted(_) => Self::TursoRouted,
            #[cfg(feature = "sim")]
            ServerEventStore::Sim(_, _) => Self::Sim,
        }
    }
}

/// Backend-neutral projection row returned by [`QueryPlaneStore`].
#[derive(Clone, Debug, PartialEq)]
pub struct QueryProjectionFieldsRow {
    pub entity_id: String,
    pub status: String,
    pub fields: BTreeMap<String, Option<String>>,
}

/// Backend-neutral row for one granular Cedar policy entry.
#[derive(Clone, Debug)]
pub struct PolicyStoreRow {
    pub tenant: String,
    pub policy_id: String,
    pub cedar_text: String,
    pub policy_hash: String,
    pub created_at: String,
    pub created_by: String,
    pub enabled: bool,
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

impl From<PostgresPolicyRow> for PolicyStoreRow {
    fn from(row: PostgresPolicyRow) -> Self {
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

/// Durable query-plane capability.
#[async_trait::async_trait]
pub trait QueryPlaneStore: Send + Sync {
    async fn upsert_projection(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        status: &str,
        fields: &serde_json::Value,
        sequence_nr: u64,
    ) -> Result<(), PersistenceError>;

    async fn remove_projection(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<(), PersistenceError>;

    async fn query_field_index(
        &self,
        tenant: &str,
        entity_type: &str,
        where_clause: &str,
        params: Vec<String>,
    ) -> Result<Option<Vec<String>>, PersistenceError>;

    async fn load_projection_fields_many(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_ids: &[String],
        field_names: &[&str],
    ) -> Result<Option<Vec<QueryProjectionFieldsRow>>, PersistenceError>;

    async fn projected_entity_counts_by_tenant(
        &self,
    ) -> Result<Option<Vec<(String, u64)>>, PersistenceError>;
}

/// Durable observe trajectory sink.
#[async_trait::async_trait]
pub trait TrajectorySink: Send + Sync {
    async fn persist_trajectory_entry(&self, entry: &TrajectoryEntry) -> Result<(), String>;
}

/// Granular Cedar policy persistence capability.
#[async_trait::async_trait]
pub trait PolicyStore: Send + Sync {
    async fn save_policy(
        &self,
        tenant: &str,
        policy_id: &str,
        cedar_text: &str,
        created_by: &str,
    ) -> Result<bool, String>;

    async fn load_policies_for_tenant(&self, tenant: &str) -> Result<Vec<PolicyStoreRow>, String>;

    async fn load_all_policies(&self) -> Result<Vec<PolicyStoreRow>, String>;

    async fn toggle_policy_enabled(
        &self,
        tenant: &str,
        policy_id: &str,
        enabled: bool,
    ) -> Result<bool, String>;

    async fn update_policy_text(
        &self,
        tenant: &str,
        policy_id: &str,
        cedar_text: &str,
        created_by: &str,
    ) -> Result<bool, String>;

    async fn delete_policy(&self, tenant: &str, policy_id: &str) -> Result<(), String>;
}

/// Composed storage capabilities selected at boot.
#[derive(Clone)]
pub struct StorageStack {
    pub backend: BackendLabel,
    pub events: BoxedEventStore,
    pub platform: Option<Arc<dyn PlatformStore>>,
    pub policies: Option<Arc<dyn PolicyStore>>,
    pub query_plane: Option<Arc<dyn QueryPlaneStore>>,
    pub trajectory: Option<Arc<dyn TrajectorySink>>,
}

impl StorageStack {
    pub fn new(
        backend: BackendLabel,
        events: BoxedEventStore,
        platform: Option<Arc<dyn PlatformStore>>,
        policies: Option<Arc<dyn PolicyStore>>,
        query_plane: Option<Arc<dyn QueryPlaneStore>>,
        trajectory: Option<Arc<dyn TrajectorySink>>,
    ) -> Self {
        Self {
            backend,
            events,
            platform,
            policies,
            query_plane,
            trajectory,
        }
    }

    pub fn from_server_event_store(store: ServerEventStore) -> Self {
        match store {
            ServerEventStore::Postgres(store) => {
                let store = Arc::new(store);
                Self::new(
                    BackendLabel::Postgres,
                    BoxedEventStore::from_arc(store.clone()),
                    Some(store.clone() as Arc<dyn PlatformStore>),
                    Some(store.clone() as Arc<dyn PolicyStore>),
                    Some(store.clone() as Arc<dyn QueryPlaneStore>),
                    Some(store.clone() as Arc<dyn TrajectorySink>),
                )
            }
            ServerEventStore::Turso(store) => {
                let store = Arc::new(store);
                Self::new(
                    BackendLabel::Turso,
                    BoxedEventStore::from_arc(store.clone()),
                    Some(store.clone() as Arc<dyn PlatformStore>),
                    Some(store.clone() as Arc<dyn PolicyStore>),
                    Some(store.clone() as Arc<dyn QueryPlaneStore>),
                    Some(store.clone() as Arc<dyn TrajectorySink>),
                )
            }
            ServerEventStore::TenantRouted(router) => {
                let platform_store =
                    Arc::new(router.platform_store().clone()) as Arc<dyn PlatformStore>;
                let router = Arc::new(router);
                Self::new(
                    BackendLabel::TursoRouted,
                    BoxedEventStore::from_arc(router.clone()),
                    Some(platform_store),
                    Some(router.clone() as Arc<dyn PolicyStore>),
                    Some(router.clone() as Arc<dyn QueryPlaneStore>),
                    Some(router.clone() as Arc<dyn TrajectorySink>),
                )
            }
            ServerEventStore::Redis(store) => {
                let store = Arc::new(store);
                Self::new(
                    BackendLabel::Redis,
                    BoxedEventStore::from_arc(store),
                    None,
                    None,
                    None,
                    None,
                )
            }
            #[cfg(feature = "sim")]
            ServerEventStore::Sim(store, platform_store) => {
                let store = Arc::new(store);
                let platform = platform_store.map(|store| store as Arc<dyn PlatformStore>);
                Self::new(
                    BackendLabel::Sim,
                    BoxedEventStore::from_arc(store),
                    platform,
                    None,
                    None,
                    None,
                )
            }
        }
    }
}

#[async_trait::async_trait]
impl PolicyStore for PostgresEventStore {
    async fn save_policy(
        &self,
        tenant: &str,
        policy_id: &str,
        cedar_text: &str,
        created_by: &str,
    ) -> Result<bool, String> {
        self.save_policy(tenant, policy_id, cedar_text, created_by)
            .await
            .map_err(|e| e.to_string())
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

    async fn toggle_policy_enabled(
        &self,
        tenant: &str,
        policy_id: &str,
        enabled: bool,
    ) -> Result<bool, String> {
        self.toggle_policy_enabled(tenant, policy_id, enabled)
            .await
            .map_err(|e| e.to_string())
    }

    async fn update_policy_text(
        &self,
        tenant: &str,
        policy_id: &str,
        cedar_text: &str,
        created_by: &str,
    ) -> Result<bool, String> {
        self.update_policy_text(tenant, policy_id, cedar_text, created_by)
            .await
            .map_err(|e| e.to_string())
    }

    async fn delete_policy(&self, tenant: &str, policy_id: &str) -> Result<(), String> {
        self.delete_policy(tenant, policy_id)
            .await
            .map_err(|e| e.to_string())
    }
}

#[async_trait::async_trait]
impl PolicyStore for TursoEventStore {
    async fn save_policy(
        &self,
        tenant: &str,
        policy_id: &str,
        cedar_text: &str,
        created_by: &str,
    ) -> Result<bool, String> {
        self.save_policy(tenant, policy_id, cedar_text, created_by)
            .await
            .map_err(|e| e.to_string())
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

    async fn toggle_policy_enabled(
        &self,
        tenant: &str,
        policy_id: &str,
        enabled: bool,
    ) -> Result<bool, String> {
        self.toggle_policy_enabled(tenant, policy_id, enabled)
            .await
            .map_err(|e| e.to_string())
    }

    async fn update_policy_text(
        &self,
        tenant: &str,
        policy_id: &str,
        cedar_text: &str,
        created_by: &str,
    ) -> Result<bool, String> {
        self.update_policy_text(tenant, policy_id, cedar_text, created_by)
            .await
            .map_err(|e| e.to_string())
    }

    async fn delete_policy(&self, tenant: &str, policy_id: &str) -> Result<(), String> {
        self.delete_policy(tenant, policy_id)
            .await
            .map_err(|e| e.to_string())
    }
}

#[async_trait::async_trait]
impl PolicyStore for TenantStoreRouter {
    async fn save_policy(
        &self,
        tenant: &str,
        policy_id: &str,
        cedar_text: &str,
        created_by: &str,
    ) -> Result<bool, String> {
        let store = self
            .store_for_tenant(tenant)
            .await
            .map_err(|e| e.to_string())?;
        store
            .save_policy(tenant, policy_id, cedar_text, created_by)
            .await
            .map_err(|e| e.to_string())
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

    async fn toggle_policy_enabled(
        &self,
        tenant: &str,
        policy_id: &str,
        enabled: bool,
    ) -> Result<bool, String> {
        let store = self
            .store_for_tenant(tenant)
            .await
            .map_err(|e| e.to_string())?;
        store
            .toggle_policy_enabled(tenant, policy_id, enabled)
            .await
            .map_err(|e| e.to_string())
    }

    async fn update_policy_text(
        &self,
        tenant: &str,
        policy_id: &str,
        cedar_text: &str,
        created_by: &str,
    ) -> Result<bool, String> {
        let store = self
            .store_for_tenant(tenant)
            .await
            .map_err(|e| e.to_string())?;
        store
            .update_policy_text(tenant, policy_id, cedar_text, created_by)
            .await
            .map_err(|e| e.to_string())
    }

    async fn delete_policy(&self, tenant: &str, policy_id: &str) -> Result<(), String> {
        let store = self
            .store_for_tenant(tenant)
            .await
            .map_err(|e| e.to_string())?;
        store
            .delete_policy(tenant, policy_id)
            .await
            .map_err(|e| e.to_string())
    }
}

#[async_trait::async_trait]
impl QueryPlaneStore for PostgresEventStore {
    async fn upsert_projection(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        status: &str,
        fields: &serde_json::Value,
        sequence_nr: u64,
    ) -> Result<(), PersistenceError> {
        self.upsert_query_projection(tenant, entity_type, entity_id, status, fields, sequence_nr)
            .await
    }

    async fn remove_projection(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<(), PersistenceError> {
        self.remove_query_projection(tenant, entity_type, entity_id)
            .await
    }

    async fn query_field_index(
        &self,
        tenant: &str,
        entity_type: &str,
        where_clause: &str,
        params: Vec<String>,
    ) -> Result<Option<Vec<String>>, PersistenceError> {
        PostgresEventStore::query_field_index(self, tenant, entity_type, where_clause, params)
            .await
            .map(Some)
    }

    async fn load_projection_fields_many(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_ids: &[String],
        field_names: &[&str],
    ) -> Result<Option<Vec<QueryProjectionFieldsRow>>, PersistenceError> {
        self.load_query_projection_fields_many(tenant, entity_type, entity_ids, field_names)
            .await
            .map(|rows| {
                Some(
                    rows.into_iter()
                        .map(|row| QueryProjectionFieldsRow {
                            entity_id: row.entity_id,
                            status: row.status,
                            fields: row.fields,
                        })
                        .collect(),
                )
            })
    }

    async fn projected_entity_counts_by_tenant(
        &self,
    ) -> Result<Option<Vec<(String, u64)>>, PersistenceError> {
        PostgresEventStore::projected_entity_counts_by_tenant(self)
            .await
            .map(Some)
    }
}

#[async_trait::async_trait]
impl QueryPlaneStore for TursoEventStore {
    async fn upsert_projection(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        status: &str,
        fields: &serde_json::Value,
        sequence_nr: u64,
    ) -> Result<(), PersistenceError> {
        self.upsert_query_projection(tenant, entity_type, entity_id, status, fields, sequence_nr)
            .await
    }

    async fn remove_projection(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<(), PersistenceError> {
        self.remove_query_projection(tenant, entity_type, entity_id)
            .await
    }

    async fn query_field_index(
        &self,
        tenant: &str,
        entity_type: &str,
        where_clause: &str,
        params: Vec<String>,
    ) -> Result<Option<Vec<String>>, PersistenceError> {
        TursoEventStore::query_field_index(self, tenant, entity_type, where_clause, params)
            .await
            .map(Some)
    }

    async fn load_projection_fields_many(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_ids: &[String],
        field_names: &[&str],
    ) -> Result<Option<Vec<QueryProjectionFieldsRow>>, PersistenceError> {
        self.load_query_projection_fields_many(tenant, entity_type, entity_ids, field_names)
            .await
            .map(|rows| {
                Some(
                    rows.into_iter()
                        .map(|row| QueryProjectionFieldsRow {
                            entity_id: row.entity_id,
                            status: row.status,
                            fields: row.fields,
                        })
                        .collect(),
                )
            })
    }

    async fn projected_entity_counts_by_tenant(
        &self,
    ) -> Result<Option<Vec<(String, u64)>>, PersistenceError> {
        TursoEventStore::projected_entity_counts_by_tenant(self)
            .await
            .map(Some)
    }
}

#[async_trait::async_trait]
impl QueryPlaneStore for TenantStoreRouter {
    async fn upsert_projection(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        status: &str,
        fields: &serde_json::Value,
        sequence_nr: u64,
    ) -> Result<(), PersistenceError> {
        let store = self.store_for_tenant(tenant).await?;
        store
            .upsert_query_projection(tenant, entity_type, entity_id, status, fields, sequence_nr)
            .await
    }

    async fn remove_projection(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<(), PersistenceError> {
        let store = self.store_for_tenant(tenant).await?;
        store
            .remove_query_projection(tenant, entity_type, entity_id)
            .await
    }

    async fn query_field_index(
        &self,
        tenant: &str,
        entity_type: &str,
        where_clause: &str,
        params: Vec<String>,
    ) -> Result<Option<Vec<String>>, PersistenceError> {
        let store = self.store_for_tenant(tenant).await?;
        TursoEventStore::query_field_index(&store, tenant, entity_type, where_clause, params)
            .await
            .map(Some)
    }

    async fn load_projection_fields_many(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_ids: &[String],
        field_names: &[&str],
    ) -> Result<Option<Vec<QueryProjectionFieldsRow>>, PersistenceError> {
        let store = self.store_for_tenant(tenant).await?;
        store
            .load_query_projection_fields_many(tenant, entity_type, entity_ids, field_names)
            .await
            .map(|rows| {
                Some(
                    rows.into_iter()
                        .map(|row| QueryProjectionFieldsRow {
                            entity_id: row.entity_id,
                            status: row.status,
                            fields: row.fields,
                        })
                        .collect(),
                )
            })
    }

    async fn projected_entity_counts_by_tenant(
        &self,
    ) -> Result<Option<Vec<(String, u64)>>, PersistenceError> {
        let mut counts = Vec::new();
        for tenant_id in self.connected_tenants().await {
            let store = self.store_for_tenant(&tenant_id).await?;
            if let Some((_, count)) = TursoEventStore::projected_entity_counts_by_tenant(&store)
                .await?
                .into_iter()
                .find(|(tenant, _)| tenant == &tenant_id)
            {
                counts.push((tenant_id, count));
            }
        }
        Ok(Some(counts))
    }
}

#[async_trait::async_trait]
impl QueryPlaneStore for ServerEventStore {
    async fn upsert_projection(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        status: &str,
        fields: &serde_json::Value,
        sequence_nr: u64,
    ) -> Result<(), PersistenceError> {
        self.upsert_query_projection(tenant, entity_type, entity_id, status, fields, sequence_nr)
            .await
    }

    async fn remove_projection(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<(), PersistenceError> {
        self.remove_query_projection(tenant, entity_type, entity_id)
            .await
    }

    async fn query_field_index(
        &self,
        tenant: &str,
        entity_type: &str,
        where_clause: &str,
        params: Vec<String>,
    ) -> Result<Option<Vec<String>>, PersistenceError> {
        self.query_field_index(tenant, entity_type, where_clause, params)
            .await
    }

    async fn load_projection_fields_many(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_ids: &[String],
        field_names: &[&str],
    ) -> Result<Option<Vec<QueryProjectionFieldsRow>>, PersistenceError> {
        self.load_query_projection_fields_many(tenant, entity_type, entity_ids, field_names)
            .await
            .map(|rows| {
                rows.map(|rows| {
                    rows.into_iter()
                        .map(|row| QueryProjectionFieldsRow {
                            entity_id: row.entity_id,
                            status: row.status,
                            fields: row.fields,
                        })
                        .collect()
                })
            })
    }

    async fn projected_entity_counts_by_tenant(
        &self,
    ) -> Result<Option<Vec<(String, u64)>>, PersistenceError> {
        self.projected_entity_counts_by_tenant().await
    }
}

fn trajectory_source_label(source: &TrajectorySource) -> &'static str {
    match source {
        TrajectorySource::Entity => "Entity",
        TrajectorySource::Platform => "Platform",
        TrajectorySource::Authz => "Authz",
    }
}

fn trajectory_request_body_json(entry: &TrajectoryEntry) -> Option<String> {
    entry.request_body.as_ref().and_then(|value| {
        let serialized = serde_json::to_string(value).ok()?;
        Some(if serialized.len() > 4096 {
            let mut end = 4096;
            while !serialized.is_char_boundary(end) {
                end -= 1;
            }
            serialized[..end].to_string()
        } else {
            serialized
        })
    })
}

fn trajectory_matched_policy_ids_json(entry: &TrajectoryEntry) -> Option<String> {
    entry
        .matched_policy_ids
        .as_ref()
        .map(|ids| serde_json::to_string(ids).unwrap_or_default())
}

#[async_trait::async_trait]
impl TrajectorySink for PostgresEventStore {
    async fn persist_trajectory_entry(&self, entry: &TrajectoryEntry) -> Result<(), String> {
        let matched_policy_ids_json = trajectory_matched_policy_ids_json(entry);
        let request_body_json = trajectory_request_body_json(entry);
        let source = entry.source.as_ref().map(trajectory_source_label);

        self.persist_trajectory(PostgresTrajectoryInsert {
            tenant: &entry.tenant,
            entity_type: &entry.entity_type,
            entity_id: &entry.entity_id,
            action: &entry.action,
            success: entry.success,
            from_status: entry.from_status.as_deref(),
            to_status: entry.to_status.as_deref(),
            error: entry.error.as_deref(),
            agent_id: entry.agent_id.as_deref(),
            session_id: entry.session_id.as_deref(),
            authz_denied: entry.authz_denied,
            denied_resource: entry.denied_resource.as_deref(),
            denied_module: entry.denied_module.as_deref(),
            source,
            spec_governed: entry.spec_governed,
            created_at: &entry.timestamp,
            request_body: request_body_json.as_deref(),
            intent: entry.intent.as_deref(),
            matched_policy_ids: matched_policy_ids_json.as_deref(),
        })
        .await
        .map_err(|e| {
            format!(
                "failed to persist trajectory entry for {}/{}/{} action {} in postgres: {e}",
                entry.tenant, entry.entity_type, entry.entity_id, entry.action
            )
        })
    }
}

#[async_trait::async_trait]
impl TrajectorySink for TursoEventStore {
    async fn persist_trajectory_entry(&self, entry: &TrajectoryEntry) -> Result<(), String> {
        let matched_policy_ids_json = trajectory_matched_policy_ids_json(entry);
        let request_body_json = trajectory_request_body_json(entry);
        let source = entry.source.as_ref().map(trajectory_source_label);

        self.persist_trajectory(TursoTrajectoryInsert {
            tenant: &entry.tenant,
            entity_type: &entry.entity_type,
            entity_id: &entry.entity_id,
            action: &entry.action,
            success: entry.success,
            from_status: entry.from_status.as_deref(),
            to_status: entry.to_status.as_deref(),
            error: entry.error.as_deref(),
            agent_id: entry.agent_id.as_deref(),
            session_id: entry.session_id.as_deref(),
            authz_denied: entry.authz_denied,
            denied_resource: entry.denied_resource.as_deref(),
            denied_module: entry.denied_module.as_deref(),
            source,
            spec_governed: entry.spec_governed,
            created_at: &entry.timestamp,
            request_body: request_body_json.as_deref(),
            intent: entry.intent.as_deref(),
            matched_policy_ids: matched_policy_ids_json.as_deref(),
        })
        .await
        .map_err(|e| {
            format!(
                "failed to persist trajectory entry for {}/{}/{} action {} in turso: {e}",
                entry.tenant, entry.entity_type, entry.entity_id, entry.action
            )
        })
    }
}

#[async_trait::async_trait]
impl TrajectorySink for TenantStoreRouter {
    async fn persist_trajectory_entry(&self, entry: &TrajectoryEntry) -> Result<(), String> {
        let store = self.store_for_tenant(&entry.tenant).await.map_err(|e| {
            format!(
                "failed to resolve tenant store for trajectory entry {}/{}/{} action {}: {e}",
                entry.tenant, entry.entity_type, entry.entity_id, entry.action
            )
        })?;
        let matched_policy_ids_json = trajectory_matched_policy_ids_json(entry);
        let request_body_json = trajectory_request_body_json(entry);
        let source = entry.source.as_ref().map(trajectory_source_label);

        store
            .persist_trajectory(TursoTrajectoryInsert {
                tenant: &entry.tenant,
                entity_type: &entry.entity_type,
                entity_id: &entry.entity_id,
                action: &entry.action,
                success: entry.success,
                from_status: entry.from_status.as_deref(),
                to_status: entry.to_status.as_deref(),
                error: entry.error.as_deref(),
                agent_id: entry.agent_id.as_deref(),
                session_id: entry.session_id.as_deref(),
                authz_denied: entry.authz_denied,
                denied_resource: entry.denied_resource.as_deref(),
                denied_module: entry.denied_module.as_deref(),
                source,
                spec_governed: entry.spec_governed,
                created_at: &entry.timestamp,
                request_body: request_body_json.as_deref(),
                intent: entry.intent.as_deref(),
                matched_policy_ids: matched_policy_ids_json.as_deref(),
            })
            .await
            .map_err(|e| {
                format!(
                    "failed to persist trajectory entry for {}/{}/{} action {} in turso-routed: {e}",
                    entry.tenant, entry.entity_type, entry.entity_id, entry.action
                )
            })
    }
}

#[async_trait::async_trait]
impl TrajectorySink for ServerEventStore {
    async fn persist_trajectory_entry(&self, entry: &TrajectoryEntry) -> Result<(), String> {
        self.persist_trajectory_entry(entry).await
    }
}

fn unsupported_platform_backend(store: &ServerEventStore) -> String {
    format!(
        "platform storage is not supported on {} backend",
        store.backend_name()
    )
}

#[async_trait::async_trait]
impl PlatformStore for ServerEventStore {
    async fn upsert_spec(
        &self,
        tenant: &str,
        entity_type: &str,
        ioa_source: &str,
        csdl_xml: &str,
        content_hash: &str,
    ) -> Result<(), String> {
        let store = self
            .platform_store()
            .ok_or_else(|| unsupported_platform_backend(self))?;
        store
            .upsert_spec(tenant, entity_type, ioa_source, csdl_xml, content_hash)
            .await
    }

    async fn load_specs(&self) -> Result<Vec<SpecRow>, String> {
        let store = self
            .platform_store()
            .ok_or_else(|| unsupported_platform_backend(self))?;
        store.load_specs().await
    }

    async fn delete_spec(&self, tenant: &str, entity_type: &str) -> Result<(), String> {
        let store = self
            .platform_store()
            .ok_or_else(|| unsupported_platform_backend(self))?;
        store.delete_spec(tenant, entity_type).await
    }

    async fn commit_specs(&self, tenant: &str) -> Result<(), String> {
        let store = self
            .platform_store()
            .ok_or_else(|| unsupported_platform_backend(self))?;
        store.commit_specs(tenant).await
    }

    async fn delete_uncommitted_specs(&self) -> Result<usize, String> {
        let store = self
            .platform_store()
            .ok_or_else(|| unsupported_platform_backend(self))?;
        store.delete_uncommitted_specs().await
    }

    async fn load_verification_cache(
        &self,
        tenant: &str,
    ) -> Result<std::collections::BTreeMap<String, (String, bool)>, String> {
        let store = self
            .platform_store()
            .ok_or_else(|| unsupported_platform_backend(self))?;
        store.load_verification_cache(tenant).await
    }

    async fn persist_spec_verification(
        &self,
        tenant: &str,
        entity_type: &str,
        update: SpecVerificationUpdate<'_>,
    ) -> Result<(), String> {
        let store = self
            .platform_store()
            .ok_or_else(|| unsupported_platform_backend(self))?;
        store
            .persist_spec_verification(tenant, entity_type, update)
            .await
    }

    async fn upsert_tenant_policy(&self, tenant: &str, policy_text: &str) -> Result<(), String> {
        let store = self
            .platform_store()
            .ok_or_else(|| unsupported_platform_backend(self))?;
        store.upsert_tenant_policy(tenant, policy_text).await
    }

    async fn upsert_tenant_constraints(
        &self,
        tenant: &str,
        cross_invariants_toml: &str,
    ) -> Result<(), String> {
        let store = self
            .platform_store()
            .ok_or_else(|| unsupported_platform_backend(self))?;
        store
            .upsert_tenant_constraints(tenant, cross_invariants_toml)
            .await
    }

    async fn load_tenant_policies(&self) -> Result<Vec<(String, String)>, String> {
        let store = self
            .platform_store()
            .ok_or_else(|| unsupported_platform_backend(self))?;
        store.load_tenant_policies().await
    }

    async fn is_app_installed(&self, tenant: &str, app_name: &str) -> Result<bool, String> {
        let store = self
            .platform_store()
            .ok_or_else(|| unsupported_platform_backend(self))?;
        store.is_app_installed(tenant, app_name).await
    }

    async fn record_installed_app(&self, tenant: &str, app_name: &str) -> Result<(), String> {
        let store = self
            .platform_store()
            .ok_or_else(|| unsupported_platform_backend(self))?;
        store.record_installed_app(tenant, app_name).await
    }

    async fn record_installed_app_metadata(
        &self,
        record: &InstalledAppRecord,
    ) -> Result<(), String> {
        let store = self
            .platform_store()
            .ok_or_else(|| unsupported_platform_backend(self))?;
        store.record_installed_app_metadata(record).await
    }

    async fn get_installed_app(
        &self,
        tenant: &str,
        app_name: &str,
    ) -> Result<Option<InstalledAppRecord>, String> {
        let store = self
            .platform_store()
            .ok_or_else(|| unsupported_platform_backend(self))?;
        store.get_installed_app(tenant, app_name).await
    }

    async fn list_all_installed_apps(&self) -> Result<Vec<(String, String)>, String> {
        let store = self
            .platform_store()
            .ok_or_else(|| unsupported_platform_backend(self))?;
        store.list_all_installed_apps().await
    }

    async fn upsert_pending_decision(
        &self,
        id: &str,
        tenant: &str,
        status: &str,
        data: &str,
    ) -> Result<(), String> {
        let store = self
            .platform_store()
            .ok_or_else(|| unsupported_platform_backend(self))?;
        store
            .upsert_pending_decision(id, tenant, status, data)
            .await
    }

    async fn load_pending_decisions(&self, limit: usize) -> Result<Vec<String>, String> {
        let store = self
            .platform_store()
            .ok_or_else(|| unsupported_platform_backend(self))?;
        store.load_pending_decisions(limit).await
    }

    async fn load_all_wasm_modules(&self, tenant: &str) -> Result<Vec<WasmModuleRow>, String> {
        let store = self
            .platform_store()
            .ok_or_else(|| unsupported_platform_backend(self))?;
        store.load_all_wasm_modules(tenant).await
    }

    async fn load_wasm_modules_all_tenants(&self) -> Result<Vec<WasmModuleRow>, String> {
        let store = self
            .platform_store()
            .ok_or_else(|| unsupported_platform_backend(self))?;
        store.load_wasm_modules_all_tenants().await
    }

    async fn upsert_wasm_module(
        &self,
        tenant: &str,
        name: &str,
        bytes: &[u8],
        hash: &str,
    ) -> Result<(), String> {
        let store = self
            .platform_store()
            .ok_or_else(|| unsupported_platform_backend(self))?;
        store.upsert_wasm_module(tenant, name, bytes, hash).await
    }
}
