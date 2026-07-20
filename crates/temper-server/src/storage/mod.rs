//! Runtime storage stack boundary.
//!
//! `temper_runtime::persistence::EventStore` uses `impl Future` return types,
//! which is good for concrete backends but not dyn-object-safe. This module
//! provides the boxed adapter used by the server-facing storage stack so
//! backend selection is a composition step rather than business-code branching.

// Object-safe trait return types unavoidably use Pin<Box<dyn Future<Output =
// nested-result>>> shapes. The `EventStoreFuture` alias is the explicit
// factoring of that pattern; clippy's type_complexity lint flags it anyway.
#![allow(clippy::type_complexity)]

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;
use temper_runtime::persistence::{
    EventStore, JournalRead, PersistenceAppend, PersistenceAppendResult, PersistenceEnvelope,
    PersistenceError,
};
use temper_store_postgres::{PostgresEventStore, PostgresTrajectoryInsert};
use temper_store_turso::{
    AgentSummary, DesignTimeEventRow, EvolutionRecordRow, FeatureRequestRow,
    OtsQueuedTrajectoryRow, OtsTrajectoryParams, OtsTrajectoryRow, PolicyDenialPatternRow,
    TenantStoreRouter, TenantUserRow, TursoEventStore, TursoTrajectoryInsert, TursoTrajectoryRow,
    TursoWasmInvocationInsert, TursoWasmInvocationRow, TursoWasmModuleMetadataRow,
    UnmetIntentAggRow, store::TrajectoryStats,
};

use crate::platform_store::PlatformStore;
#[cfg(feature = "sim")]
use crate::platform_store::SimPlatformStore;
use crate::state::trajectory::{TrajectoryEntry, TrajectorySource};

mod policy_row;
mod postgres_conversions;
mod published_artifacts;
mod query_plane_impls;
mod query_plane_read;
pub use policy_row::PolicyStoreRow;
use postgres_conversions::*;
pub use published_artifacts::{
    PublishedArtifactStore, PublishedArtifactStoreRow, PublishedArtifactStoreUpsert,
};
mod query_plane;
pub use query_plane::{
    EntityCatalogRow, QueryFieldIndexOrder, QueryFieldIndexOrderDirection, QueryFieldIndexPage,
    QueryPlaneStore, QueryProjectionFieldsRow, QueryProjectionUpsert,
};
pub(crate) use query_plane_read::{
    CatalogRowsLoad, load_catalog_rows_by_id, load_selected_catalog_rows_by_id,
};

mod backend_artifacts;
mod backend_observe_evolution;
mod backend_ops;
mod backend_policies;
mod boxed_event_store;
mod dyn_event_store;
mod stack;

pub use boxed_event_store::BoxedEventStore;
pub use dyn_event_store::{DynEventStore, EventStoreFuture};
pub use stack::StorageStack;

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

/// Inputs for a native brand-new data-only entity create.
///
/// This capability is only valid for entities whose first durable event and
/// first query projection row can be inserted atomically by a storage backend.
pub struct DataOnlyCreateRecord<'a> {
    /// Tenant that owns the entity.
    pub tenant: &'a str,
    /// Entity type being created.
    pub entity_type: &'a str,
    /// Entity id being created.
    pub entity_id: &'a str,
    /// Initial entity status.
    pub status: &'a str,
    /// Projection fields to store in the query catalog and scalar index.
    pub fields: &'a serde_json::Value,
    /// Full response projection to store in the query catalog.
    pub state: &'a serde_json::Value,
    /// First event envelope to append at sequence number 1.
    pub event: &'a PersistenceEnvelope,
}

/// Optional native storage capability for brand-new data-only creates.
#[async_trait::async_trait]
pub trait DataOnlyCreateStore: Send + Sync {
    /// Persist the first event and initial projection atomically.
    ///
    /// Returns the new sequence number on success. Duplicate first events or
    /// duplicate projection rows should return [`PersistenceError::ConcurrencyViolation`]
    /// so the caller can decline the fast path and use the generic path.
    async fn create_data_only_entity(
        &self,
        record: DataOnlyCreateRecord<'_>,
    ) -> Result<u64, PersistenceError>;
}

/// Durable observe trajectory sink.
#[async_trait::async_trait]
pub trait TrajectorySink: Send + Sync {
    async fn persist_trajectory_entry(&self, entry: &TrajectoryEntry) -> Result<(), String>;
}

/// Backend label for trait-object metadata stores.
pub trait BackendNamedStore: Send + Sync {
    fn backend_name(&self) -> &'static str;
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

/// Observe/trajectory read capability.
#[async_trait::async_trait]
pub trait ObserveReadStore: Send + Sync {
    async fn load_recent_trajectories(
        &self,
        limit: i64,
    ) -> Result<Vec<TursoTrajectoryRow>, PersistenceError>;

    async fn load_unmet_intent_rows(&self) -> Result<Vec<UnmetIntentAggRow>, PersistenceError>;

    async fn load_submit_spec_timestamps(
        &self,
    ) -> Result<BTreeMap<String, String>, PersistenceError>;

    async fn count_trajectories_by_tenant(&self)
    -> Result<BTreeMap<String, u64>, PersistenceError>;

    async fn query_trajectory_stats(
        &self,
        entity_type: Option<&str>,
        action: Option<&str>,
        success_filter: Option<bool>,
        failed_limit: i64,
    ) -> Result<TrajectoryStats, PersistenceError>;

    async fn query_trajectories_by_agent(
        &self,
        agent_id: &str,
        tenant: Option<&str>,
        entity_type: Option<&str>,
        limit: i64,
    ) -> Result<Vec<TursoTrajectoryRow>, PersistenceError>;

    async fn query_agent_summaries(
        &self,
        tenant: Option<&str>,
    ) -> Result<Vec<AgentSummary>, PersistenceError>;
}

/// Evolution engine durable metadata capability.
#[async_trait::async_trait]
pub trait EvolutionStore: Send + Sync {
    #[allow(clippy::too_many_arguments)]
    async fn upsert_feature_request(
        &self,
        id: &str,
        category: &str,
        description: &str,
        frequency: i64,
        trajectory_refs_json: &str,
        disposition: &str,
        developer_notes: Option<&str>,
    ) -> Result<(), PersistenceError>;

    async fn list_feature_requests(
        &self,
        disposition: Option<&str>,
    ) -> Result<Vec<FeatureRequestRow>, PersistenceError>;

    async fn update_feature_request(
        &self,
        id: &str,
        disposition: &str,
        developer_notes: Option<&str>,
    ) -> Result<bool, PersistenceError>;

    async fn insert_evolution_record(
        &self,
        id: &str,
        record_type: &str,
        status: &str,
        created_by: &str,
        derived_from: Option<&str>,
        data_json: &str,
    ) -> Result<(), PersistenceError>;

    async fn get_evolution_record(
        &self,
        id: &str,
    ) -> Result<Option<EvolutionRecordRow>, PersistenceError>;

    async fn list_evolution_records(
        &self,
        record_type: Option<&str>,
        status: Option<&str>,
    ) -> Result<Vec<EvolutionRecordRow>, PersistenceError>;

    async fn list_ranked_insights(&self) -> Result<Vec<EvolutionRecordRow>, PersistenceError>;
}

/// Design-time verification event capability.
#[async_trait::async_trait]
pub trait DesignTimeEventStore: Send + Sync {
    #[allow(clippy::too_many_arguments)]
    async fn insert_design_time_event(
        &self,
        kind: &str,
        entity_type: &str,
        tenant: &str,
        summary: &str,
        level: Option<&str>,
        passed: Option<bool>,
        step_number: Option<i64>,
        total_steps: Option<i64>,
    ) -> Result<(), PersistenceError>;

    async fn list_design_time_events(
        &self,
        tenant: Option<&str>,
        limit: i64,
    ) -> Result<Vec<DesignTimeEventRow>, PersistenceError>;
}

/// OTS trajectory capability.
#[async_trait::async_trait]
pub trait OtsStore: Send + Sync {
    async fn persist_ots_trajectory(
        &self,
        params: &OtsTrajectoryParams<'_>,
    ) -> Result<(), PersistenceError>;

    async fn enqueue_ots_trajectory(
        &self,
        params: &OtsTrajectoryParams<'_>,
    ) -> Result<(), PersistenceError>;

    async fn mark_ots_trajectory_persisted(
        &self,
        trajectory_id: &str,
    ) -> Result<(), PersistenceError>;

    async fn mark_ots_trajectory_failed(
        &self,
        trajectory_id: &str,
        error: &str,
    ) -> Result<(), PersistenceError>;

    async fn list_queued_ots_trajectories(
        &self,
        limit: i64,
    ) -> Result<Vec<OtsQueuedTrajectoryRow>, PersistenceError>;

    async fn list_ots_trajectories(
        &self,
        tenant: &str,
        agent_id: Option<&str>,
        outcome: Option<&str>,
        limit: i64,
    ) -> Result<Vec<OtsTrajectoryRow>, PersistenceError>;

    async fn get_ots_trajectory(
        &self,
        trajectory_id: &str,
    ) -> Result<Option<String>, PersistenceError>;
}

/// Legacy database-backed blob capability.
#[async_trait::async_trait]
pub trait BlobStore: Send + Sync {
    async fn put_blob(&self, key: &str, data: &[u8]) -> Result<(), String>;

    async fn put_blob_with_ttl(
        &self,
        key: &str,
        data: &[u8],
        ttl: Option<Duration>,
    ) -> Result<(), String>;

    async fn sweep_expired_blobs(&self, max_rows: u64) -> Result<u64, String>;

    async fn get_blob(&self, key: &str) -> Result<Option<Vec<u8>>, String>;
}

/// Authorization analytics capability.
#[async_trait::async_trait]
pub trait AuthzAnalyticsStore: Send + Sync {
    async fn upsert_policy_denial_pattern(
        &self,
        tenant: &str,
        agent_type: Option<&str>,
        action: &str,
        resource_type: &str,
        resource_id: &str,
        timestamp: &str,
    ) -> Result<(), PersistenceError>;

    async fn load_policy_denial_patterns(
        &self,
        tenant: &str,
    ) -> Result<Vec<PolicyDenialPatternRow>, PersistenceError>;
}

/// Pending decision query capability.
#[async_trait::async_trait]
pub trait DecisionStore: Send + Sync {
    async fn query_decisions(
        &self,
        tenant: &str,
        status: Option<&str>,
    ) -> Result<Vec<String>, PersistenceError>;

    async fn query_all_decisions(
        &self,
        status: Option<&str>,
    ) -> Result<Vec<String>, PersistenceError>;

    async fn get_pending_decision(&self, id: &str) -> Result<Option<String>, PersistenceError>;
}

/// WASM module metadata capability.
#[async_trait::async_trait]
pub trait WasmMetadataStore: Send + Sync {
    async fn load_wasm_module_metadata_all_tenants(
        &self,
    ) -> Result<Vec<TursoWasmModuleMetadataRow>, PersistenceError>;

    async fn delete_wasm_module(
        &self,
        tenant: &str,
        module_name: &str,
    ) -> Result<bool, PersistenceError>;
}

/// WASM invocation log capability.
#[async_trait::async_trait]
pub trait WasmInvocationStore: Send + Sync {
    async fn persist_wasm_invocation(
        &self,
        entry: &TursoWasmInvocationInsert<'_>,
    ) -> Result<(), PersistenceError>;

    async fn load_recent_wasm_invocations(
        &self,
        limit: i64,
    ) -> Result<Vec<TursoWasmInvocationRow>, PersistenceError>;
}

/// Composite metadata capability used by legacy helper call sites while the
/// concern-specific migrations proceed.
pub trait MetadataStore:
    BackendNamedStore
    + PolicyStore
    + ObserveReadStore
    + EvolutionStore
    + DesignTimeEventStore
    + OtsStore
    + BlobStore
    + AuthzAnalyticsStore
    + DecisionStore
    + WasmMetadataStore
    + WasmInvocationStore
    + PublishedArtifactStore
{
}

impl<T> MetadataStore for T where
    T: BackendNamedStore
        + PolicyStore
        + ObserveReadStore
        + EvolutionStore
        + DesignTimeEventStore
        + OtsStore
        + BlobStore
        + AuthzAnalyticsStore
        + DecisionStore
        + WasmMetadataStore
        + WasmInvocationStore
        + PublishedArtifactStore
{
}

/// Provider for platform, tenant-scoped, and fan-out metadata stores.
#[async_trait::async_trait]
pub trait MetadataStoreProvider: Send + Sync {
    fn platform_store(&self) -> Option<Arc<dyn MetadataStore>>;

    async fn store_for_tenant(&self, tenant: &str) -> Option<Arc<dyn MetadataStore>>;

    async fn all_stores(&self) -> Vec<Arc<dyn MetadataStore>>;
}

/// Explicit Turso tenant-store access for transitional boot/recovery paths.
#[async_trait::async_trait]
pub trait TursoStoreProvider: Send + Sync {
    fn supports_tenant_admin(&self) -> bool;

    fn platform_store(&self) -> Option<TursoEventStore>;

    async fn store_for_tenant(&self, tenant: &str) -> Option<TursoEventStore>;

    async fn all_stores(&self) -> Vec<TursoEventStore>;

    async fn connected_tenants(&self) -> Vec<String>;

    async fn tenants_for_user(&self, user_id: &str)
    -> Result<Vec<TenantUserRow>, PersistenceError>;

    async fn register_tenant(&self, tenant_id: &str) -> Result<TursoEventStore, PersistenceError>;

    async fn list_tenants(&self) -> Result<Vec<String>, PersistenceError>;

    async fn remove_tenant(&self, tenant_id: &str) -> Result<bool, PersistenceError>;

    async fn add_tenant_user(
        &self,
        tenant_id: &str,
        user_id: &str,
        role: &str,
    ) -> Result<(), PersistenceError>;

    async fn list_tenant_users(
        &self,
        tenant_id: &str,
    ) -> Result<Vec<TenantUserRow>, PersistenceError>;

    async fn remove_tenant_user(
        &self,
        tenant_id: &str,
        user_id: &str,
    ) -> Result<(), PersistenceError>;

    async fn ensure_tenant(&self, tenant_id: &str) -> Result<bool, PersistenceError>;
}
