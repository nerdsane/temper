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
    EntityKeyLookup, EventStore, IndexReconciliation, PersistenceAppend, PersistenceAppendResult,
    PersistenceEnvelope, PersistenceError,
};
use temper_store_postgres::{PostgresEventStore, PostgresPolicyRow, PostgresTrajectoryInsert};
use temper_store_turso::{
    ActionStats, AgentSummary, DesignTimeEventRow, EvolutionRecordRow, FeatureRequestRow,
    OtsQueuedTrajectoryRow, OtsTrajectoryParams, OtsTrajectoryRow, PolicyDenialPatternRow,
    PolicyRow as TursoPolicyRow, TenantStoreRouter, TenantUserRow, TursoEventStore,
    TursoTrajectoryInsert, TursoTrajectoryRow, TursoWasmInvocationInsert, TursoWasmInvocationRow,
    TursoWasmModuleMetadataRow, UnmetIntentAggRow, store::TrajectoryStats,
};

use crate::platform_store::PlatformStore;
#[cfg(feature = "sim")]
use crate::platform_store::SimPlatformStore;
use crate::state::trajectory::{TrajectoryEntry, TrajectorySource};

mod dyn_event_store;
mod published_artifacts;
mod query_plane_impls;
mod query_plane_read;
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

pub type EventStoreFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Object-safe adapter for the runtime event journal.
pub trait DynEventStore: Send + Sync {
    /// Whether declared-key rows and coverage watermarks are authoritative here.
    fn supports_authoritative_key_index(&self) -> bool;

    fn append<'a>(
        &'a self,
        persistence_id: &'a str,
        expected_sequence: u64,
        events: &'a [PersistenceEnvelope],
    ) -> EventStoreFuture<'a, Result<u64, PersistenceError>>;

    fn append_batch<'a>(
        &'a self,
        appends: &'a [PersistenceAppend],
    ) -> EventStoreFuture<'a, Result<Vec<PersistenceAppendResult>, PersistenceError>>;

    fn read_events<'a>(
        &'a self,
        persistence_id: &'a str,
        from_sequence: u64,
    ) -> EventStoreFuture<'a, Result<Vec<PersistenceEnvelope>, PersistenceError>>;

    fn append_with_keys<'a>(
        &'a self,
        persistence_id: &'a str,
        expected_sequence: u64,
        events: &'a [PersistenceEnvelope],
        key_rows: &'a [temper_runtime::persistence::EntityKeyRow],
    ) -> EventStoreFuture<'a, Result<u64, PersistenceError>>;

    fn append_with_index_rows<'a>(
        &'a self,
        persistence_id: &'a str,
        expected_sequence: u64,
        events: &'a [PersistenceEnvelope],
        key_rows: &'a [temper_runtime::persistence::EntityKeyRow],
        vector_rows: &'a [temper_runtime::persistence::EntityVectorRow],
        reconciliation: IndexReconciliation,
    ) -> EventStoreFuture<'a, Result<u64, PersistenceError>>;

    fn backfill_entity_vectors<'a>(
        &'a self,
        tenant: &'a str,
        entity_type: &'a str,
        entity_id: &'a str,
        vector_rows: &'a [temper_runtime::persistence::EntityVectorRow],
    ) -> EventStoreFuture<'a, Result<(), PersistenceError>>;

    fn vector_candidates<'a>(
        &'a self,
        tenant: &'a str,
        entity_type: &'a str,
        decl_name: &'a str,
        model_tag: &'a str,
        limit: usize,
    ) -> EventStoreFuture<
        'a,
        Result<Vec<temper_runtime::persistence::EntityVectorCandidate>, PersistenceError>,
    >;

    fn mark_vector_index_backfilled<'a>(
        &'a self,
        tenant: &'a str,
        entity_type: &'a str,
        vector_set: &'a str,
    ) -> EventStoreFuture<'a, Result<(), PersistenceError>>;

    fn vector_index_backfilled_types<'a>(
        &'a self,
        tenant: &'a str,
    ) -> EventStoreFuture<'a, Result<Vec<(String, String)>, PersistenceError>>;

    fn vectored_entity_ids_for_type<'a>(
        &'a self,
        tenant: &'a str,
        entity_type: &'a str,
    ) -> EventStoreFuture<'a, Result<Vec<String>, PersistenceError>>;

    fn lookup_by_key<'a>(
        &'a self,
        tenant: &'a str,
        entity_type: &'a str,
        key_name: &'a str,
        key_hash: &'a str,
    ) -> EventStoreFuture<'a, Result<Option<String>, PersistenceError>>;

    /// Look up the owner and journal generation committed with a declared key.
    fn lookup_by_key_with_sequence<'a>(
        &'a self,
        tenant: &'a str,
        entity_type: &'a str,
        key_name: &'a str,
        key_hash: &'a str,
    ) -> EventStoreFuture<'a, Result<Option<EntityKeyLookup>, PersistenceError>>;

    fn backfill_entity_keys<'a>(
        &'a self,
        tenant: &'a str,
        entity_type: &'a str,
        entity_id: &'a str,
        expected_sequence: u64,
        contract_fence: temper_runtime::persistence::KeyIndexBackfillFence<'a>,
        key_rows: &'a [temper_runtime::persistence::EntityKeyRow],
    ) -> EventStoreFuture<'a, Result<(), PersistenceError>>;

    fn mark_key_index_backfilled<'a>(
        &'a self,
        tenant: &'a str,
        entity_type: &'a str,
        key_set: &'a str,
    ) -> EventStoreFuture<'a, Result<(), PersistenceError>>;

    fn key_index_backfilled_types<'a>(
        &'a self,
        tenant: &'a str,
    ) -> EventStoreFuture<'a, Result<Vec<(String, String)>, PersistenceError>>;

    /// Return the revision fencing reconciliation for an entity type's key index.
    fn key_index_reconciliation_revision<'a>(
        &'a self,
        tenant: &'a str,
        entity_type: &'a str,
    ) -> EventStoreFuture<'a, Result<u64, PersistenceError>>;

    /// Begin a fenced key-index backfill and return its reconciliation revision.
    fn begin_key_index_backfill<'a>(
        &'a self,
        tenant: &'a str,
        entity_type: &'a str,
        key_set: &'a str,
    ) -> EventStoreFuture<'a, Result<u64, PersistenceError>>;

    /// Mark a key contract complete only if its reconciliation revision is unchanged.
    fn mark_key_index_backfilled_if_revision<'a>(
        &'a self,
        tenant: &'a str,
        entity_type: &'a str,
        key_set: &'a str,
        expected_revision: u64,
    ) -> EventStoreFuture<'a, Result<bool, PersistenceError>>;

    fn keyed_entity_ids_for_type<'a>(
        &'a self,
        tenant: &'a str,
        entity_type: &'a str,
    ) -> EventStoreFuture<'a, Result<Vec<String>, PersistenceError>>;

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

    fn list_entity_ids_for_key_reconciliation<'a>(
        &'a self,
        tenant: &'a str,
        entity_type: &'a str,
    ) -> EventStoreFuture<'a, Result<Vec<String>, PersistenceError>>;

    fn list_entity_ids_limited<'a>(
        &'a self,
        tenant: &'a str,
        entity_type: Option<&'a str>,
        limit: usize,
    ) -> EventStoreFuture<'a, Result<Vec<(String, String)>, PersistenceError>>;
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

    /// Whether this backend can serve as the exact declared-key ownership oracle.
    pub fn supports_authoritative_key_index(&self) -> bool {
        self.0.supports_authoritative_key_index()
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

    pub async fn append_batch(
        &self,
        appends: &[PersistenceAppend],
    ) -> Result<Vec<PersistenceAppendResult>, PersistenceError> {
        self.0.append_batch(appends).await
    }

    pub async fn read_events(
        &self,
        persistence_id: &str,
        from_sequence: u64,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        self.0.read_events(persistence_id, from_sequence).await
    }

    pub async fn append_with_keys(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
        key_rows: &[temper_runtime::persistence::EntityKeyRow],
    ) -> Result<u64, PersistenceError> {
        self.0
            .append_with_keys(persistence_id, expected_sequence, events, key_rows)
            .await
    }

    pub async fn append_with_index_rows(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
        key_rows: &[temper_runtime::persistence::EntityKeyRow],
        vector_rows: &[temper_runtime::persistence::EntityVectorRow],
        reconciliation: IndexReconciliation,
    ) -> Result<u64, PersistenceError> {
        self.0
            .append_with_index_rows(
                persistence_id,
                expected_sequence,
                events,
                key_rows,
                vector_rows,
                reconciliation,
            )
            .await
    }

    pub async fn backfill_entity_vectors(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        vector_rows: &[temper_runtime::persistence::EntityVectorRow],
    ) -> Result<(), PersistenceError> {
        self.0
            .backfill_entity_vectors(tenant, entity_type, entity_id, vector_rows)
            .await
    }

    pub async fn vector_candidates(
        &self,
        tenant: &str,
        entity_type: &str,
        decl_name: &str,
        model_tag: &str,
        limit: usize,
    ) -> Result<Vec<temper_runtime::persistence::EntityVectorCandidate>, PersistenceError> {
        self.0
            .vector_candidates(tenant, entity_type, decl_name, model_tag, limit)
            .await
    }

    pub async fn mark_vector_index_backfilled(
        &self,
        tenant: &str,
        entity_type: &str,
        vector_set: &str,
    ) -> Result<(), PersistenceError> {
        self.0
            .mark_vector_index_backfilled(tenant, entity_type, vector_set)
            .await
    }

    pub async fn vector_index_backfilled_types(
        &self,
        tenant: &str,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        self.0.vector_index_backfilled_types(tenant).await
    }

    pub async fn vectored_entity_ids_for_type(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Vec<String>, PersistenceError> {
        self.0
            .vectored_entity_ids_for_type(tenant, entity_type)
            .await
    }

    pub async fn lookup_by_key(
        &self,
        tenant: &str,
        entity_type: &str,
        key_name: &str,
        key_hash: &str,
    ) -> Result<Option<String>, PersistenceError> {
        self.0
            .lookup_by_key(tenant, entity_type, key_name, key_hash)
            .await
    }

    /// Look up the owner and journal generation committed with a declared key.
    pub async fn lookup_by_key_with_sequence(
        &self,
        tenant: &str,
        entity_type: &str,
        key_name: &str,
        key_hash: &str,
    ) -> Result<Option<EntityKeyLookup>, PersistenceError> {
        self.0
            .lookup_by_key_with_sequence(tenant, entity_type, key_name, key_hash)
            .await
    }

    pub async fn backfill_entity_keys(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        expected_sequence: u64,
        contract_fence: temper_runtime::persistence::KeyIndexBackfillFence<'_>,
        key_rows: &[temper_runtime::persistence::EntityKeyRow],
    ) -> Result<(), PersistenceError> {
        self.0
            .backfill_entity_keys(
                tenant,
                entity_type,
                entity_id,
                expected_sequence,
                contract_fence,
                key_rows,
            )
            .await
    }

    pub async fn mark_key_index_backfilled(
        &self,
        tenant: &str,
        entity_type: &str,
        key_set: &str,
    ) -> Result<(), PersistenceError> {
        self.0
            .mark_key_index_backfilled(tenant, entity_type, key_set)
            .await
    }

    pub async fn key_index_backfilled_types(
        &self,
        tenant: &str,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        self.0.key_index_backfilled_types(tenant).await
    }

    /// Return the revision fencing reconciliation for an entity type's key index.
    pub async fn key_index_reconciliation_revision(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<u64, PersistenceError> {
        self.0
            .key_index_reconciliation_revision(tenant, entity_type)
            .await
    }

    /// Begin a fenced key-index backfill and return its reconciliation revision.
    pub async fn begin_key_index_backfill(
        &self,
        tenant: &str,
        entity_type: &str,
        key_set: &str,
    ) -> Result<u64, PersistenceError> {
        self.0
            .begin_key_index_backfill(tenant, entity_type, key_set)
            .await
    }

    /// Mark a key contract complete only if its reconciliation revision is unchanged.
    pub async fn mark_key_index_backfilled_if_revision(
        &self,
        tenant: &str,
        entity_type: &str,
        key_set: &str,
        expected_revision: u64,
    ) -> Result<bool, PersistenceError> {
        self.0
            .mark_key_index_backfilled_if_revision(tenant, entity_type, key_set, expected_revision)
            .await
    }

    pub async fn keyed_entity_ids_for_type(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Vec<String>, PersistenceError> {
        self.0.keyed_entity_ids_for_type(tenant, entity_type).await
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

    /// Enumerate every journal stream or key-index owner that exact declared-key
    /// repair must reconcile, including deleted and key-only phantom owners.
    pub async fn list_entity_ids_for_key_reconciliation(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Vec<String>, PersistenceError> {
        self.0
            .list_entity_ids_for_key_reconciliation(tenant, entity_type)
            .await
    }

    pub async fn list_entity_ids_limited(
        &self,
        tenant: &str,
        entity_type: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        self.0
            .list_entity_ids_limited(tenant, entity_type, limit)
            .await
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
    /// Complete declared-key row set derived from the initial durable state.
    pub key_rows: &'a [temper_runtime::persistence::EntityKeyRow],
    /// Whether this entity type declares keys and requires exact ownership.
    pub reconcile_keys: bool,
    /// Versioned signature of the current declared-key contract, including the
    /// empty signature for a no-key type.
    pub key_set_signature: &'a str,
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

/// Composed storage capabilities selected at boot.
#[derive(Clone)]
pub struct StorageStack {
    pub backend: BackendLabel,
    pub events: BoxedEventStore,
    pub postgres_pool: Option<PgPool>,
    pub turso: Option<Arc<dyn TursoStoreProvider>>,
    pub platform: Option<Arc<dyn PlatformStore>>,
    pub policies: Option<Arc<dyn PolicyStore>>,
    pub query_plane: Option<Arc<dyn QueryPlaneStore>>,
    pub data_only_create: Option<Arc<dyn DataOnlyCreateStore>>,
    pub trajectory: Option<Arc<dyn TrajectorySink>>,
    pub metadata: Option<Arc<dyn MetadataStoreProvider>>,
}

impl StorageStack {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        backend: BackendLabel,
        events: BoxedEventStore,
        postgres_pool: Option<PgPool>,
        turso: Option<Arc<dyn TursoStoreProvider>>,
        platform: Option<Arc<dyn PlatformStore>>,
        policies: Option<Arc<dyn PolicyStore>>,
        query_plane: Option<Arc<dyn QueryPlaneStore>>,
        data_only_create: Option<Arc<dyn DataOnlyCreateStore>>,
        trajectory: Option<Arc<dyn TrajectorySink>>,
        metadata: Option<Arc<dyn MetadataStoreProvider>>,
    ) -> Self {
        Self {
            backend,
            events,
            postgres_pool,
            turso,
            platform,
            policies,
            query_plane,
            data_only_create,
            trajectory,
            metadata,
        }
    }

    pub fn from_postgres(store: PostgresEventStore) -> Self {
        let store = Arc::new(store);
        Self::new(
            BackendLabel::Postgres,
            BoxedEventStore::from_arc(store.clone()),
            Some(store.pool().clone()),
            None,
            Some(store.clone() as Arc<dyn PlatformStore>),
            Some(store.clone() as Arc<dyn PolicyStore>),
            Some(store.clone() as Arc<dyn QueryPlaneStore>),
            Some(store.clone() as Arc<dyn DataOnlyCreateStore>),
            Some(store.clone() as Arc<dyn TrajectorySink>),
            Some(Arc::new(SingleMetadataStoreProvider::new(store))),
        )
    }

    pub fn from_turso(store: TursoEventStore) -> Self {
        let store = Arc::new(store);
        Self::new(
            BackendLabel::Turso,
            BoxedEventStore::from_arc(store.clone()),
            None,
            Some(Arc::new(SingleTursoStoreProvider::new(store.clone()))),
            Some(store.clone() as Arc<dyn PlatformStore>),
            Some(store.clone() as Arc<dyn PolicyStore>),
            Some(store.clone() as Arc<dyn QueryPlaneStore>),
            None,
            Some(store.clone() as Arc<dyn TrajectorySink>),
            Some(Arc::new(SingleMetadataStoreProvider::new(store))),
        )
    }

    pub fn from_tenant_router(router: TenantStoreRouter) -> Self {
        let platform_store = Arc::new(router.platform_store().clone()) as Arc<dyn PlatformStore>;
        let router = Arc::new(router);
        Self::new(
            BackendLabel::TursoRouted,
            BoxedEventStore::from_arc(router.clone()),
            None,
            Some(Arc::new(TenantRoutedTursoStoreProvider::new(
                router.as_ref().clone(),
            ))),
            Some(platform_store),
            Some(router.clone() as Arc<dyn PolicyStore>),
            Some(router.clone() as Arc<dyn QueryPlaneStore>),
            None,
            Some(router.clone() as Arc<dyn TrajectorySink>),
            Some(Arc::new(TenantRoutedMetadataStoreProvider::new(
                router.as_ref().clone(),
            ))),
        )
    }

    pub fn from_redis(store: temper_store_redis::RedisEventStore) -> Self {
        let store = Arc::new(store);
        Self::new(
            BackendLabel::Redis,
            BoxedEventStore::from_arc(store),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
    }

    #[cfg(feature = "sim")]
    pub fn from_sim(
        store: temper_store_sim::SimEventStore,
        platform_store: Option<Arc<SimPlatformStore>>,
    ) -> Self {
        let store = Arc::new(store);
        let platform = platform_store.map(|store| store as Arc<dyn PlatformStore>);
        Self::new(
            BackendLabel::Sim,
            BoxedEventStore::from_arc(store),
            None,
            None,
            platform,
            None,
            None,
            None,
            None,
            None,
        )
    }
}

struct SingleMetadataStoreProvider {
    store: Arc<dyn MetadataStore>,
}

impl SingleMetadataStoreProvider {
    fn new<T>(store: Arc<T>) -> Self
    where
        T: MetadataStore + 'static,
    {
        Self { store }
    }
}

#[async_trait::async_trait]
impl MetadataStoreProvider for SingleMetadataStoreProvider {
    fn platform_store(&self) -> Option<Arc<dyn MetadataStore>> {
        Some(self.store.clone())
    }

    async fn store_for_tenant(&self, _tenant: &str) -> Option<Arc<dyn MetadataStore>> {
        Some(self.store.clone())
    }

    async fn all_stores(&self) -> Vec<Arc<dyn MetadataStore>> {
        vec![self.store.clone()]
    }
}

struct SingleTursoStoreProvider {
    store: Arc<TursoEventStore>,
}

impl SingleTursoStoreProvider {
    fn new(store: Arc<TursoEventStore>) -> Self {
        Self { store }
    }
}

fn tenant_admin_unsupported() -> PersistenceError {
    PersistenceError::Storage("tenant management requires routed Turso storage".to_string())
}

#[async_trait::async_trait]
impl TursoStoreProvider for SingleTursoStoreProvider {
    fn supports_tenant_admin(&self) -> bool {
        false
    }

    fn platform_store(&self) -> Option<TursoEventStore> {
        Some(self.store.as_ref().clone())
    }

    async fn store_for_tenant(&self, _tenant: &str) -> Option<TursoEventStore> {
        Some(self.store.as_ref().clone())
    }

    async fn all_stores(&self) -> Vec<TursoEventStore> {
        vec![self.store.as_ref().clone()]
    }

    async fn connected_tenants(&self) -> Vec<String> {
        Vec::new()
    }

    async fn tenants_for_user(
        &self,
        _user_id: &str,
    ) -> Result<Vec<TenantUserRow>, PersistenceError> {
        Err(tenant_admin_unsupported())
    }

    async fn register_tenant(&self, _tenant_id: &str) -> Result<TursoEventStore, PersistenceError> {
        Err(tenant_admin_unsupported())
    }

    async fn list_tenants(&self) -> Result<Vec<String>, PersistenceError> {
        Err(tenant_admin_unsupported())
    }

    async fn remove_tenant(&self, _tenant_id: &str) -> Result<bool, PersistenceError> {
        Err(tenant_admin_unsupported())
    }

    async fn add_tenant_user(
        &self,
        _tenant_id: &str,
        _user_id: &str,
        _role: &str,
    ) -> Result<(), PersistenceError> {
        Err(tenant_admin_unsupported())
    }

    async fn list_tenant_users(
        &self,
        _tenant_id: &str,
    ) -> Result<Vec<TenantUserRow>, PersistenceError> {
        Err(tenant_admin_unsupported())
    }

    async fn remove_tenant_user(
        &self,
        _tenant_id: &str,
        _user_id: &str,
    ) -> Result<(), PersistenceError> {
        Err(tenant_admin_unsupported())
    }

    async fn ensure_tenant(&self, _tenant_id: &str) -> Result<bool, PersistenceError> {
        Err(tenant_admin_unsupported())
    }
}

struct TenantRoutedMetadataStoreProvider {
    router: TenantStoreRouter,
}

impl TenantRoutedMetadataStoreProvider {
    fn new(router: TenantStoreRouter) -> Self {
        Self { router }
    }
}

#[async_trait::async_trait]
impl MetadataStoreProvider for TenantRoutedMetadataStoreProvider {
    fn platform_store(&self) -> Option<Arc<dyn MetadataStore>> {
        Some(Arc::new(self.router.platform_store().clone()) as Arc<dyn MetadataStore>)
    }

    async fn store_for_tenant(&self, tenant: &str) -> Option<Arc<dyn MetadataStore>> {
        self.router
            .store_for_tenant(tenant)
            .await
            .ok()
            .map(|store| Arc::new(store) as Arc<dyn MetadataStore>)
    }

    async fn all_stores(&self) -> Vec<Arc<dyn MetadataStore>> {
        let mut stores =
            vec![Arc::new(self.router.platform_store().clone()) as Arc<dyn MetadataStore>];
        for tenant_id in self.router.connected_tenants().await {
            if let Ok(store) = self.router.store_for_tenant(&tenant_id).await {
                stores.push(Arc::new(store) as Arc<dyn MetadataStore>);
            }
        }
        stores
    }
}

struct TenantRoutedTursoStoreProvider {
    router: TenantStoreRouter,
}

impl TenantRoutedTursoStoreProvider {
    fn new(router: TenantStoreRouter) -> Self {
        Self { router }
    }
}

#[async_trait::async_trait]
impl TursoStoreProvider for TenantRoutedTursoStoreProvider {
    fn supports_tenant_admin(&self) -> bool {
        true
    }

    fn platform_store(&self) -> Option<TursoEventStore> {
        Some(self.router.platform_store().clone())
    }

    async fn store_for_tenant(&self, tenant: &str) -> Option<TursoEventStore> {
        self.router.store_for_tenant(tenant).await.ok()
    }

    async fn all_stores(&self) -> Vec<TursoEventStore> {
        let mut stores = vec![self.router.platform_store().clone()];
        for tenant_id in self.router.connected_tenants().await {
            if let Ok(store) = self.router.store_for_tenant(&tenant_id).await {
                stores.push(store);
            }
        }
        stores
    }

    async fn connected_tenants(&self) -> Vec<String> {
        self.router.connected_tenants().await
    }

    async fn tenants_for_user(
        &self,
        user_id: &str,
    ) -> Result<Vec<TenantUserRow>, PersistenceError> {
        self.router.tenants_for_user(user_id).await
    }

    async fn register_tenant(&self, tenant_id: &str) -> Result<TursoEventStore, PersistenceError> {
        self.router.register_tenant(tenant_id).await
    }

    async fn list_tenants(&self) -> Result<Vec<String>, PersistenceError> {
        self.router.list_tenants().await
    }

    async fn remove_tenant(&self, tenant_id: &str) -> Result<bool, PersistenceError> {
        self.router.remove_tenant(tenant_id).await
    }

    async fn add_tenant_user(
        &self,
        tenant_id: &str,
        user_id: &str,
        role: &str,
    ) -> Result<(), PersistenceError> {
        self.router.add_tenant_user(tenant_id, user_id, role).await
    }

    async fn list_tenant_users(
        &self,
        tenant_id: &str,
    ) -> Result<Vec<TenantUserRow>, PersistenceError> {
        self.router.list_tenant_users(tenant_id).await
    }

    async fn remove_tenant_user(
        &self,
        tenant_id: &str,
        user_id: &str,
    ) -> Result<(), PersistenceError> {
        self.router.remove_tenant_user(tenant_id, user_id).await
    }

    async fn ensure_tenant(&self, tenant_id: &str) -> Result<bool, PersistenceError> {
        self.router.ensure_tenant(tenant_id).await
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

impl BackendNamedStore for PostgresEventStore {
    fn backend_name(&self) -> &'static str {
        "postgres"
    }
}

impl BackendNamedStore for TursoEventStore {
    fn backend_name(&self) -> &'static str {
        "turso"
    }
}

#[async_trait::async_trait]
impl ObserveReadStore for PostgresEventStore {
    async fn load_recent_trajectories(
        &self,
        limit: i64,
    ) -> Result<Vec<TursoTrajectoryRow>, PersistenceError> {
        self.load_recent_trajectories(limit)
            .await
            .map(|rows| rows.into_iter().map(pg_trajectory_to_turso).collect())
    }

    async fn load_unmet_intent_rows(&self) -> Result<Vec<UnmetIntentAggRow>, PersistenceError> {
        self.load_unmet_intent_rows()
            .await
            .map(|rows| rows.into_iter().map(pg_unmet_to_turso).collect())
    }

    async fn load_submit_spec_timestamps(
        &self,
    ) -> Result<BTreeMap<String, String>, PersistenceError> {
        self.load_submit_spec_timestamps().await
    }

    async fn count_trajectories_by_tenant(
        &self,
    ) -> Result<BTreeMap<String, u64>, PersistenceError> {
        self.count_trajectories_by_tenant().await
    }

    async fn query_trajectory_stats(
        &self,
        entity_type: Option<&str>,
        action: Option<&str>,
        success_filter: Option<bool>,
        failed_limit: i64,
    ) -> Result<TrajectoryStats, PersistenceError> {
        self.query_trajectory_stats(entity_type, action, success_filter, failed_limit)
            .await
            .map(pg_stats_to_turso)
    }

    async fn query_trajectories_by_agent(
        &self,
        agent_id: &str,
        tenant: Option<&str>,
        entity_type: Option<&str>,
        limit: i64,
    ) -> Result<Vec<TursoTrajectoryRow>, PersistenceError> {
        self.query_trajectories_by_agent(agent_id, tenant, entity_type, limit)
            .await
            .map(|rows| rows.into_iter().map(pg_trajectory_to_turso).collect())
    }

    async fn query_agent_summaries(
        &self,
        tenant: Option<&str>,
    ) -> Result<Vec<AgentSummary>, PersistenceError> {
        self.query_agent_summaries(tenant)
            .await
            .map(|rows| rows.into_iter().map(pg_agent_summary_to_turso).collect())
    }
}

#[async_trait::async_trait]
impl ObserveReadStore for TursoEventStore {
    async fn load_recent_trajectories(
        &self,
        limit: i64,
    ) -> Result<Vec<TursoTrajectoryRow>, PersistenceError> {
        self.load_recent_trajectories(limit).await
    }

    async fn load_unmet_intent_rows(&self) -> Result<Vec<UnmetIntentAggRow>, PersistenceError> {
        self.load_unmet_intent_rows().await
    }

    async fn load_submit_spec_timestamps(
        &self,
    ) -> Result<BTreeMap<String, String>, PersistenceError> {
        self.load_submit_spec_timestamps().await
    }

    async fn count_trajectories_by_tenant(
        &self,
    ) -> Result<BTreeMap<String, u64>, PersistenceError> {
        self.count_trajectories_by_tenant().await
    }

    async fn query_trajectory_stats(
        &self,
        entity_type: Option<&str>,
        action: Option<&str>,
        success_filter: Option<bool>,
        failed_limit: i64,
    ) -> Result<TrajectoryStats, PersistenceError> {
        self.query_trajectory_stats(entity_type, action, success_filter, failed_limit)
            .await
    }

    async fn query_trajectories_by_agent(
        &self,
        agent_id: &str,
        tenant: Option<&str>,
        entity_type: Option<&str>,
        limit: i64,
    ) -> Result<Vec<TursoTrajectoryRow>, PersistenceError> {
        self.query_trajectories_by_agent(agent_id, tenant, entity_type, limit)
            .await
    }

    async fn query_agent_summaries(
        &self,
        tenant: Option<&str>,
    ) -> Result<Vec<AgentSummary>, PersistenceError> {
        self.query_agent_summaries(tenant).await
    }
}

#[async_trait::async_trait]
impl EvolutionStore for PostgresEventStore {
    async fn upsert_feature_request(
        &self,
        id: &str,
        category: &str,
        description: &str,
        frequency: i64,
        trajectory_refs_json: &str,
        disposition: &str,
        developer_notes: Option<&str>,
    ) -> Result<(), PersistenceError> {
        self.upsert_feature_request(
            id,
            category,
            description,
            frequency,
            trajectory_refs_json,
            disposition,
            developer_notes,
        )
        .await
    }

    async fn list_feature_requests(
        &self,
        disposition: Option<&str>,
    ) -> Result<Vec<FeatureRequestRow>, PersistenceError> {
        self.list_feature_requests(disposition)
            .await
            .map(|rows| rows.into_iter().map(pg_feature_request_to_turso).collect())
    }

    async fn update_feature_request(
        &self,
        id: &str,
        disposition: &str,
        developer_notes: Option<&str>,
    ) -> Result<bool, PersistenceError> {
        self.update_feature_request(id, disposition, developer_notes)
            .await
    }

    async fn insert_evolution_record(
        &self,
        id: &str,
        record_type: &str,
        status: &str,
        created_by: &str,
        derived_from: Option<&str>,
        data_json: &str,
    ) -> Result<(), PersistenceError> {
        self.insert_evolution_record(id, record_type, status, created_by, derived_from, data_json)
            .await
    }

    async fn get_evolution_record(
        &self,
        id: &str,
    ) -> Result<Option<EvolutionRecordRow>, PersistenceError> {
        self.get_evolution_record(id)
            .await
            .map(|row| row.map(pg_evolution_record_to_turso))
    }

    async fn list_evolution_records(
        &self,
        record_type: Option<&str>,
        status: Option<&str>,
    ) -> Result<Vec<EvolutionRecordRow>, PersistenceError> {
        self.list_evolution_records(record_type, status)
            .await
            .map(|rows| rows.into_iter().map(pg_evolution_record_to_turso).collect())
    }

    async fn list_ranked_insights(&self) -> Result<Vec<EvolutionRecordRow>, PersistenceError> {
        self.list_ranked_insights()
            .await
            .map(|rows| rows.into_iter().map(pg_evolution_record_to_turso).collect())
    }
}

#[async_trait::async_trait]
impl EvolutionStore for TursoEventStore {
    async fn upsert_feature_request(
        &self,
        id: &str,
        category: &str,
        description: &str,
        frequency: i64,
        trajectory_refs_json: &str,
        disposition: &str,
        developer_notes: Option<&str>,
    ) -> Result<(), PersistenceError> {
        self.upsert_feature_request(
            id,
            category,
            description,
            frequency,
            trajectory_refs_json,
            disposition,
            developer_notes,
        )
        .await
    }

    async fn list_feature_requests(
        &self,
        disposition: Option<&str>,
    ) -> Result<Vec<FeatureRequestRow>, PersistenceError> {
        self.list_feature_requests(disposition).await
    }

    async fn update_feature_request(
        &self,
        id: &str,
        disposition: &str,
        developer_notes: Option<&str>,
    ) -> Result<bool, PersistenceError> {
        self.update_feature_request(id, disposition, developer_notes)
            .await
    }

    async fn insert_evolution_record(
        &self,
        id: &str,
        record_type: &str,
        status: &str,
        created_by: &str,
        derived_from: Option<&str>,
        data_json: &str,
    ) -> Result<(), PersistenceError> {
        self.insert_evolution_record(id, record_type, status, created_by, derived_from, data_json)
            .await
    }

    async fn get_evolution_record(
        &self,
        id: &str,
    ) -> Result<Option<EvolutionRecordRow>, PersistenceError> {
        self.get_evolution_record(id).await
    }

    async fn list_evolution_records(
        &self,
        record_type: Option<&str>,
        status: Option<&str>,
    ) -> Result<Vec<EvolutionRecordRow>, PersistenceError> {
        self.list_evolution_records(record_type, status).await
    }

    async fn list_ranked_insights(&self) -> Result<Vec<EvolutionRecordRow>, PersistenceError> {
        self.list_ranked_insights().await
    }
}

#[async_trait::async_trait]
impl DesignTimeEventStore for PostgresEventStore {
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
    ) -> Result<(), PersistenceError> {
        self.insert_design_time_event(
            kind,
            entity_type,
            tenant,
            summary,
            level,
            passed,
            step_number,
            total_steps,
        )
        .await
    }

    async fn list_design_time_events(
        &self,
        tenant: Option<&str>,
        limit: i64,
    ) -> Result<Vec<DesignTimeEventRow>, PersistenceError> {
        self.list_design_time_events(tenant, limit)
            .await
            .map(|rows| {
                rows.into_iter()
                    .map(pg_design_time_event_to_turso)
                    .collect()
            })
    }
}

#[async_trait::async_trait]
impl DesignTimeEventStore for TursoEventStore {
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
    ) -> Result<(), PersistenceError> {
        self.insert_design_time_event(
            kind,
            entity_type,
            tenant,
            summary,
            level,
            passed,
            step_number,
            total_steps,
        )
        .await
    }

    async fn list_design_time_events(
        &self,
        tenant: Option<&str>,
        limit: i64,
    ) -> Result<Vec<DesignTimeEventRow>, PersistenceError> {
        self.list_design_time_events(tenant, limit).await
    }
}

#[async_trait::async_trait]
impl OtsStore for PostgresEventStore {
    async fn persist_ots_trajectory(
        &self,
        params: &OtsTrajectoryParams<'_>,
    ) -> Result<(), PersistenceError> {
        self.persist_ots_trajectory(&temper_store_postgres::PostgresOtsTrajectoryParams {
            trajectory_id: params.trajectory_id,
            tenant: params.tenant,
            agent_id: params.agent_id,
            session_id: params.session_id,
            outcome: params.outcome,
            turn_count: params.turn_count,
            data: params.data,
        })
        .await
    }

    async fn enqueue_ots_trajectory(
        &self,
        params: &OtsTrajectoryParams<'_>,
    ) -> Result<(), PersistenceError> {
        self.enqueue_ots_trajectory(&temper_store_postgres::PostgresOtsTrajectoryParams {
            trajectory_id: params.trajectory_id,
            tenant: params.tenant,
            agent_id: params.agent_id,
            session_id: params.session_id,
            outcome: params.outcome,
            turn_count: params.turn_count,
            data: params.data,
        })
        .await
    }

    async fn mark_ots_trajectory_persisted(
        &self,
        trajectory_id: &str,
    ) -> Result<(), PersistenceError> {
        self.mark_ots_trajectory_persisted(trajectory_id).await
    }

    async fn mark_ots_trajectory_failed(
        &self,
        trajectory_id: &str,
        error: &str,
    ) -> Result<(), PersistenceError> {
        self.mark_ots_trajectory_failed(trajectory_id, error).await
    }

    async fn list_queued_ots_trajectories(
        &self,
        limit: i64,
    ) -> Result<Vec<OtsQueuedTrajectoryRow>, PersistenceError> {
        self.list_queued_ots_trajectories(limit)
            .await
            .map(|rows| rows.into_iter().map(pg_queued_ots_to_turso).collect())
    }

    async fn list_ots_trajectories(
        &self,
        tenant: &str,
        agent_id: Option<&str>,
        outcome: Option<&str>,
        limit: i64,
    ) -> Result<Vec<OtsTrajectoryRow>, PersistenceError> {
        self.list_ots_trajectories(tenant, agent_id, outcome, limit)
            .await
            .map(|rows| rows.into_iter().map(pg_ots_to_turso).collect())
    }

    async fn get_ots_trajectory(
        &self,
        trajectory_id: &str,
    ) -> Result<Option<String>, PersistenceError> {
        self.get_ots_trajectory(trajectory_id).await
    }
}

#[async_trait::async_trait]
impl OtsStore for TursoEventStore {
    async fn persist_ots_trajectory(
        &self,
        params: &OtsTrajectoryParams<'_>,
    ) -> Result<(), PersistenceError> {
        self.persist_ots_trajectory(params).await
    }

    async fn enqueue_ots_trajectory(
        &self,
        params: &OtsTrajectoryParams<'_>,
    ) -> Result<(), PersistenceError> {
        self.enqueue_ots_trajectory(params).await
    }

    async fn mark_ots_trajectory_persisted(
        &self,
        trajectory_id: &str,
    ) -> Result<(), PersistenceError> {
        self.mark_ots_trajectory_persisted(trajectory_id).await
    }

    async fn mark_ots_trajectory_failed(
        &self,
        trajectory_id: &str,
        error: &str,
    ) -> Result<(), PersistenceError> {
        self.mark_ots_trajectory_failed(trajectory_id, error).await
    }

    async fn list_queued_ots_trajectories(
        &self,
        limit: i64,
    ) -> Result<Vec<OtsQueuedTrajectoryRow>, PersistenceError> {
        self.list_queued_ots_trajectories(limit).await
    }

    async fn list_ots_trajectories(
        &self,
        tenant: &str,
        agent_id: Option<&str>,
        outcome: Option<&str>,
        limit: i64,
    ) -> Result<Vec<OtsTrajectoryRow>, PersistenceError> {
        self.list_ots_trajectories(tenant, agent_id, outcome, limit)
            .await
    }

    async fn get_ots_trajectory(
        &self,
        trajectory_id: &str,
    ) -> Result<Option<String>, PersistenceError> {
        self.get_ots_trajectory(trajectory_id).await
    }
}

#[async_trait::async_trait]
impl BlobStore for PostgresEventStore {
    async fn put_blob(&self, key: &str, data: &[u8]) -> Result<(), String> {
        self.put_blob(key, data).await
    }

    async fn put_blob_with_ttl(
        &self,
        key: &str,
        data: &[u8],
        ttl: Option<Duration>,
    ) -> Result<(), String> {
        self.put_blob_with_ttl(key, data, ttl).await
    }

    async fn sweep_expired_blobs(&self, max_rows: u64) -> Result<u64, String> {
        self.sweep_expired_blobs(max_rows).await
    }

    async fn get_blob(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        self.get_blob(key).await
    }
}

#[async_trait::async_trait]
impl BlobStore for TursoEventStore {
    async fn put_blob(&self, key: &str, data: &[u8]) -> Result<(), String> {
        self.put_blob(key, data).await
    }

    async fn put_blob_with_ttl(
        &self,
        key: &str,
        data: &[u8],
        ttl: Option<Duration>,
    ) -> Result<(), String> {
        self.put_blob_with_ttl(key, data, ttl).await
    }

    async fn sweep_expired_blobs(&self, max_rows: u64) -> Result<u64, String> {
        self.sweep_expired_blobs(max_rows).await
    }

    async fn get_blob(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        self.get_blob(key).await
    }
}

#[async_trait::async_trait]
impl AuthzAnalyticsStore for PostgresEventStore {
    async fn upsert_policy_denial_pattern(
        &self,
        tenant: &str,
        agent_type: Option<&str>,
        action: &str,
        resource_type: &str,
        resource_id: &str,
        timestamp: &str,
    ) -> Result<(), PersistenceError> {
        self.upsert_policy_denial_pattern(
            tenant,
            agent_type,
            action,
            resource_type,
            resource_id,
            timestamp,
        )
        .await
    }

    async fn load_policy_denial_patterns(
        &self,
        tenant: &str,
    ) -> Result<Vec<PolicyDenialPatternRow>, PersistenceError> {
        self.load_policy_denial_patterns(tenant)
            .await
            .map(|rows| rows.into_iter().map(pg_denial_pattern_to_turso).collect())
    }
}

#[async_trait::async_trait]
impl AuthzAnalyticsStore for TursoEventStore {
    async fn upsert_policy_denial_pattern(
        &self,
        tenant: &str,
        agent_type: Option<&str>,
        action: &str,
        resource_type: &str,
        resource_id: &str,
        timestamp: &str,
    ) -> Result<(), PersistenceError> {
        self.upsert_policy_denial_pattern(
            tenant,
            agent_type,
            action,
            resource_type,
            resource_id,
            timestamp,
        )
        .await
    }

    async fn load_policy_denial_patterns(
        &self,
        tenant: &str,
    ) -> Result<Vec<PolicyDenialPatternRow>, PersistenceError> {
        self.load_policy_denial_patterns(tenant).await
    }
}

#[async_trait::async_trait]
impl DecisionStore for PostgresEventStore {
    async fn query_decisions(
        &self,
        tenant: &str,
        status: Option<&str>,
    ) -> Result<Vec<String>, PersistenceError> {
        self.query_decisions(tenant, status).await
    }

    async fn query_all_decisions(
        &self,
        status: Option<&str>,
    ) -> Result<Vec<String>, PersistenceError> {
        self.query_all_decisions(status).await
    }

    async fn get_pending_decision(&self, id: &str) -> Result<Option<String>, PersistenceError> {
        self.get_pending_decision(id).await
    }
}

#[async_trait::async_trait]
impl DecisionStore for TursoEventStore {
    async fn query_decisions(
        &self,
        tenant: &str,
        status: Option<&str>,
    ) -> Result<Vec<String>, PersistenceError> {
        self.query_decisions(tenant, status).await
    }

    async fn query_all_decisions(
        &self,
        status: Option<&str>,
    ) -> Result<Vec<String>, PersistenceError> {
        self.query_all_decisions(status).await
    }

    async fn get_pending_decision(&self, id: &str) -> Result<Option<String>, PersistenceError> {
        self.get_pending_decision(id).await
    }
}

#[async_trait::async_trait]
impl WasmMetadataStore for PostgresEventStore {
    async fn load_wasm_module_metadata_all_tenants(
        &self,
    ) -> Result<Vec<TursoWasmModuleMetadataRow>, PersistenceError> {
        self.load_wasm_module_metadata_all_tenants()
            .await
            .map(|rows| rows.into_iter().map(pg_wasm_metadata_to_turso).collect())
    }

    async fn delete_wasm_module(
        &self,
        tenant: &str,
        module_name: &str,
    ) -> Result<bool, PersistenceError> {
        self.delete_wasm_module(tenant, module_name).await
    }
}

#[async_trait::async_trait]
impl WasmMetadataStore for TursoEventStore {
    async fn load_wasm_module_metadata_all_tenants(
        &self,
    ) -> Result<Vec<TursoWasmModuleMetadataRow>, PersistenceError> {
        self.load_wasm_module_metadata_all_tenants().await
    }

    async fn delete_wasm_module(
        &self,
        tenant: &str,
        module_name: &str,
    ) -> Result<bool, PersistenceError> {
        self.delete_wasm_module(tenant, module_name).await
    }
}

#[async_trait::async_trait]
impl WasmInvocationStore for PostgresEventStore {
    async fn persist_wasm_invocation(
        &self,
        entry: &TursoWasmInvocationInsert<'_>,
    ) -> Result<(), PersistenceError> {
        self.persist_wasm_invocation(&temper_store_postgres::PostgresWasmInvocationInsert {
            tenant: entry.tenant,
            entity_type: entry.entity_type,
            entity_id: entry.entity_id,
            module_name: entry.module_name,
            trigger_action: entry.trigger_action,
            callback_action: entry.callback_action,
            success: entry.success,
            error: entry.error,
            duration_ms: entry.duration_ms,
            created_at: entry.created_at,
        })
        .await
    }

    async fn load_recent_wasm_invocations(
        &self,
        limit: i64,
    ) -> Result<Vec<TursoWasmInvocationRow>, PersistenceError> {
        self.load_recent_wasm_invocations(limit)
            .await
            .map(|rows| rows.into_iter().map(pg_wasm_invocation_to_turso).collect())
    }
}

#[async_trait::async_trait]
impl WasmInvocationStore for TursoEventStore {
    async fn persist_wasm_invocation(
        &self,
        entry: &TursoWasmInvocationInsert<'_>,
    ) -> Result<(), PersistenceError> {
        self.persist_wasm_invocation(entry).await
    }

    async fn load_recent_wasm_invocations(
        &self,
        limit: i64,
    ) -> Result<Vec<TursoWasmInvocationRow>, PersistenceError> {
        self.load_recent_wasm_invocations(limit).await
    }
}

fn pg_trajectory_to_turso(row: temper_store_postgres::PostgresTrajectoryRow) -> TursoTrajectoryRow {
    TursoTrajectoryRow {
        tenant: row.tenant,
        entity_type: row.entity_type,
        entity_id: row.entity_id,
        action: row.action,
        success: row.success,
        from_status: row.from_status,
        to_status: row.to_status,
        error: row.error,
        agent_id: row.agent_id,
        session_id: row.session_id,
        authz_denied: row.authz_denied,
        denied_resource: row.denied_resource,
        denied_module: row.denied_module,
        source: row.source,
        spec_governed: row.spec_governed,
        created_at: row.created_at,
        request_body: row.request_body,
        intent: row.intent,
        matched_policy_ids: row.matched_policy_ids,
    }
}

fn pg_unmet_to_turso(row: temper_store_postgres::PostgresUnmetIntentAggRow) -> UnmetIntentAggRow {
    UnmetIntentAggRow {
        entity_type: row.entity_type,
        action: row.action,
        error: row.error,
        count: row.count,
        first_seen: row.first_seen,
        last_seen: row.last_seen,
    }
}

fn pg_stats_to_turso(stats: temper_store_postgres::PostgresTrajectoryStats) -> TrajectoryStats {
    TrajectoryStats {
        total: stats.total,
        success_count: stats.success_count,
        error_count: stats.error_count,
        success_rate: stats.success_rate,
        by_action: stats
            .by_action
            .into_iter()
            .map(|(name, action)| {
                (
                    name,
                    ActionStats {
                        total: action.total,
                        success: action.success,
                        error: action.error,
                    },
                )
            })
            .collect(),
        failed_intents: stats
            .failed_intents
            .into_iter()
            .map(pg_trajectory_to_turso)
            .collect(),
    }
}

fn pg_agent_summary_to_turso(row: temper_store_postgres::PostgresAgentSummary) -> AgentSummary {
    AgentSummary {
        agent_id: row.agent_id,
        total_actions: row.total_actions,
        success_count: row.success_count,
        error_count: row.error_count,
        denial_count: row.denial_count,
        success_rate: row.success_rate,
        last_active_at: row.last_active_at,
    }
}

fn pg_feature_request_to_turso(
    row: temper_store_postgres::PostgresFeatureRequestRow,
) -> FeatureRequestRow {
    FeatureRequestRow {
        id: row.id,
        category: row.category,
        description: row.description,
        frequency: row.frequency,
        trajectory_refs: row.trajectory_refs,
        disposition: row.disposition,
        developer_notes: row.developer_notes,
        created_at: row.created_at,
        updated_at: row.updated_at,
    }
}

fn pg_evolution_record_to_turso(
    row: temper_store_postgres::PostgresEvolutionRecordRow,
) -> EvolutionRecordRow {
    EvolutionRecordRow {
        id: row.id,
        record_type: row.record_type,
        status: row.status,
        created_by: row.created_by,
        derived_from: row.derived_from,
        data: row.data,
        timestamp: row.timestamp,
    }
}

fn pg_design_time_event_to_turso(
    row: temper_store_postgres::PostgresDesignTimeEventRow,
) -> DesignTimeEventRow {
    DesignTimeEventRow {
        id: row.id,
        kind: row.kind,
        entity_type: row.entity_type,
        tenant: row.tenant,
        summary: row.summary,
        level: row.level,
        passed: row.passed,
        step_number: row.step_number,
        total_steps: row.total_steps,
        created_at: row.created_at,
    }
}

fn pg_ots_to_turso(row: temper_store_postgres::PostgresOtsTrajectoryRow) -> OtsTrajectoryRow {
    OtsTrajectoryRow {
        trajectory_id: row.trajectory_id,
        tenant: row.tenant,
        agent_id: row.agent_id,
        session_id: row.session_id,
        outcome: row.outcome,
        turn_count: row.turn_count,
        persistence_status: row.persistence_status,
        persist_attempts: row.persist_attempts,
        last_error: row.last_error,
        created_at: row.created_at,
        updated_at: row.updated_at,
    }
}

fn pg_queued_ots_to_turso(
    row: temper_store_postgres::PostgresQueuedOtsTrajectoryRow,
) -> OtsQueuedTrajectoryRow {
    OtsQueuedTrajectoryRow {
        trajectory_id: row.trajectory_id,
        tenant: row.tenant,
        agent_id: row.agent_id,
        session_id: row.session_id,
        outcome: row.outcome,
        turn_count: row.turn_count,
        data: row.data,
        persist_attempts: row.persist_attempts,
    }
}

fn pg_denial_pattern_to_turso(
    row: temper_store_postgres::PostgresPolicyDenialPatternRow,
) -> PolicyDenialPatternRow {
    PolicyDenialPatternRow {
        tenant: row.tenant,
        agent_type: row.agent_type,
        action: row.action,
        resource_type: row.resource_type,
        count: row.count,
        first_seen: row.first_seen,
        last_seen: row.last_seen,
        distinct_resource_ids_json: row.distinct_resource_ids_json,
    }
}

fn pg_wasm_metadata_to_turso(
    row: temper_store_postgres::PostgresWasmModuleMetadataRow,
) -> TursoWasmModuleMetadataRow {
    TursoWasmModuleMetadataRow {
        tenant: row.tenant,
        module_name: row.module_name,
        sha256_hash: row.sha256_hash,
        size_bytes: row.size_bytes,
        updated_at: row.updated_at,
    }
}

fn pg_wasm_invocation_to_turso(
    row: temper_store_postgres::PostgresWasmInvocationRow,
) -> TursoWasmInvocationRow {
    TursoWasmInvocationRow {
        tenant: row.tenant,
        entity_type: row.entity_type,
        entity_id: row.entity_id,
        module_name: row.module_name,
        trigger_action: row.trigger_action,
        callback_action: row.callback_action,
        success: row.success,
        error: row.error,
        duration_ms: row.duration_ms,
        created_at: row.created_at,
    }
}

#[async_trait::async_trait]
impl DataOnlyCreateStore for PostgresEventStore {
    async fn create_data_only_entity(
        &self,
        record: DataOnlyCreateRecord<'_>,
    ) -> Result<u64, PersistenceError> {
        self.create_data_only_entity_native_with_state_and_keys(
            record.tenant,
            record.entity_type,
            record.entity_id,
            record.status,
            record.fields,
            record.state,
            record.event,
            record.key_rows,
            record.reconcile_keys,
            Some(record.key_set_signature),
        )
        .await
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
        .and_then(|ids| serde_json::to_string(ids).ok())
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
