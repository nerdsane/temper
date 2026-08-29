//! Runtime storage stack and boxed event-store boundary for selectable backends.

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
    EventStore, PersistenceAppend, PersistenceAppendResult, PersistenceEnvelope, PersistenceError,
};
use temper_store_postgres::{
    PostgresEventStore, PostgresEvolutionRecordInsert, PostgresPolicyApprovalCommit,
    PostgresTrajectoryInsert,
};
use temper_store_turso::{
    ActionStats, AgentSummary, DesignTimeEventRow, EvolutionRecordRow, FeatureRequestRow,
    OtsQueuedTrajectoryRow, OtsTrajectoryDocument, OtsTrajectoryParams, OtsTrajectoryRow,
    PolicyDenialPatternRow, TenantStoreRouter, TenantUserRow, TursoEventStore,
    TursoEvolutionRecordInsert, TursoPolicyApprovalCommit, TursoTrajectoryInsert,
    TursoTrajectoryRow, TursoWasmInvocationInsert, TursoWasmInvocationRow,
    TursoWasmModuleMetadataRow, UnmetIntentAggRow, store::TrajectoryStats,
};

use crate::platform_store::PlatformStore;
#[cfg(feature = "sim")]
use crate::platform_store::SimPlatformStore;
use crate::state::trajectory::TrajectoryEntry;

mod backend_label;
mod metadata_impls;
mod observe_read;
mod policy_store;
mod published_artifacts;
#[macro_use]
mod stream_publication_methods;
pub use backend_label::{BackendLabel, DataOnlyCreateRecord, DataOnlyCreateStore, PolicyStoreRow};
pub use policy_store::{BackendNamedStore, PolicyStore, TrajectorySink};
mod query_plane_impls;
mod query_plane_read;
mod redaction;
mod schema_deployment;
mod trajectory_row;
pub use published_artifacts::{
    PublishedArtifactStore, PublishedArtifactStoreRow, PublishedArtifactStoreUpsert,
};
pub use schema_deployment::SchemaDeploymentStoreDyn;
mod turso_store_provider;
pub use turso_store_provider::TursoStoreProvider;
mod query_plane;
pub use query_plane::{
    EntityCatalogRow, QueryFieldIndexOrder, QueryFieldIndexOrderDirection,
    QueryFieldIndexOrderTarget, QueryFieldIndexPage, QueryPlaneStore, QueryProjectionFieldsRow,
    QueryProjectionUpsert,
};
pub(crate) use query_plane_read::{
    CatalogRowsLoad, load_catalog_rows_by_id, load_selected_catalog_rows_by_id,
};

pub type EventStoreFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Object-safe adapter for the runtime event journal.
pub trait DynEventStore: Send + Sync {
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

    fn read_events_limited<'a>(
        &'a self,
        persistence_id: &'a str,
        from_sequence: u64,
        limit: usize,
    ) -> EventStoreFuture<'a, Result<Vec<PersistenceEnvelope>, PersistenceError>>;

    fn read_latest_events<'a>(
        &'a self,
        persistence_id: &'a str,
        limit: usize,
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
        reconcile_vectors: bool,
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

    fn backfill_entity_keys<'a>(
        &'a self,
        tenant: &'a str,
        entity_type: &'a str,
        entity_id: &'a str,
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

    fn list_entity_ids_limited<'a>(
        &'a self,
        tenant: &'a str,
        entity_type: Option<&'a str>,
        limit: usize,
    ) -> EventStoreFuture<'a, Result<Vec<(String, String)>, PersistenceError>>;

    fn list_journal_ids_page<'a>(
        &'a self,
        tenant: &'a str,
        entity_type: Option<&'a str>,
        after: Option<(&'a str, &'a str)>,
        limit: usize,
    ) -> EventStoreFuture<'a, Result<Vec<(String, String)>, PersistenceError>>;

    dyn_stream_publication_declarations!();

    fn list_scoped_entity_ids_page<'a>(
        &'a self,
        tenant: &'a str,
        entity_type: &'a str,
        scope: &'a temper_runtime::persistence::schema_deployment::SchemaScope,
        bundle_digest: &'a str,
        after_entity_id: Option<&'a str>,
        limit: usize,
    ) -> EventStoreFuture<'a, Result<Vec<String>, PersistenceError>>;

    fn scoped_entity_bundle_digests<'a>(
        &'a self,
        tenant: &'a str,
        entity_type: &'a str,
        entity_id: &'a str,
        scope: &'a temper_runtime::persistence::schema_deployment::SchemaScope,
        limit: usize,
    ) -> EventStoreFuture<'a, Result<Vec<String>, PersistenceError>>;

    fn scoped_bundle_write_version<'a>(
        &'a self,
        tenant: &'a str,
        scope: &'a temper_runtime::persistence::schema_deployment::SchemaScope,
        bundle_digest: &'a str,
    ) -> EventStoreFuture<'a, Result<u64, PersistenceError>>;
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

    fn append_batch<'a>(
        &'a self,
        appends: &'a [PersistenceAppend],
    ) -> EventStoreFuture<'a, Result<Vec<PersistenceAppendResult>, PersistenceError>> {
        Box::pin(EventStore::append_batch(self, appends))
    }

    fn read_events<'a>(
        &'a self,
        persistence_id: &'a str,
        from_sequence: u64,
    ) -> EventStoreFuture<'a, Result<Vec<PersistenceEnvelope>, PersistenceError>> {
        Box::pin(EventStore::read_events(self, persistence_id, from_sequence))
    }

    fn read_events_limited<'a>(
        &'a self,
        persistence_id: &'a str,
        from_sequence: u64,
        limit: usize,
    ) -> EventStoreFuture<'a, Result<Vec<PersistenceEnvelope>, PersistenceError>> {
        Box::pin(EventStore::read_events_limited(
            self,
            persistence_id,
            from_sequence,
            limit,
        ))
    }

    fn read_latest_events<'a>(
        &'a self,
        persistence_id: &'a str,
        limit: usize,
    ) -> EventStoreFuture<'a, Result<Vec<PersistenceEnvelope>, PersistenceError>> {
        Box::pin(EventStore::read_latest_events(self, persistence_id, limit))
    }

    fn append_with_keys<'a>(
        &'a self,
        persistence_id: &'a str,
        expected_sequence: u64,
        events: &'a [PersistenceEnvelope],
        key_rows: &'a [temper_runtime::persistence::EntityKeyRow],
    ) -> EventStoreFuture<'a, Result<u64, PersistenceError>> {
        Box::pin(EventStore::append_with_keys(
            self,
            persistence_id,
            expected_sequence,
            events,
            key_rows,
        ))
    }

    fn append_with_index_rows<'a>(
        &'a self,
        persistence_id: &'a str,
        expected_sequence: u64,
        events: &'a [PersistenceEnvelope],
        key_rows: &'a [temper_runtime::persistence::EntityKeyRow],
        vector_rows: &'a [temper_runtime::persistence::EntityVectorRow],
        reconcile_vectors: bool,
    ) -> EventStoreFuture<'a, Result<u64, PersistenceError>> {
        Box::pin(EventStore::append_with_index_rows(
            self,
            persistence_id,
            expected_sequence,
            events,
            key_rows,
            vector_rows,
            reconcile_vectors,
        ))
    }

    fn backfill_entity_vectors<'a>(
        &'a self,
        tenant: &'a str,
        entity_type: &'a str,
        entity_id: &'a str,
        vector_rows: &'a [temper_runtime::persistence::EntityVectorRow],
    ) -> EventStoreFuture<'a, Result<(), PersistenceError>> {
        Box::pin(EventStore::backfill_entity_vectors(
            self,
            tenant,
            entity_type,
            entity_id,
            vector_rows,
        ))
    }

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
    > {
        Box::pin(EventStore::vector_candidates(
            self,
            tenant,
            entity_type,
            decl_name,
            model_tag,
            limit,
        ))
    }

    fn mark_vector_index_backfilled<'a>(
        &'a self,
        tenant: &'a str,
        entity_type: &'a str,
        vector_set: &'a str,
    ) -> EventStoreFuture<'a, Result<(), PersistenceError>> {
        Box::pin(EventStore::mark_vector_index_backfilled(
            self,
            tenant,
            entity_type,
            vector_set,
        ))
    }

    fn vector_index_backfilled_types<'a>(
        &'a self,
        tenant: &'a str,
    ) -> EventStoreFuture<'a, Result<Vec<(String, String)>, PersistenceError>> {
        Box::pin(EventStore::vector_index_backfilled_types(self, tenant))
    }

    fn vectored_entity_ids_for_type<'a>(
        &'a self,
        tenant: &'a str,
        entity_type: &'a str,
    ) -> EventStoreFuture<'a, Result<Vec<String>, PersistenceError>> {
        Box::pin(EventStore::vectored_entity_ids_for_type(
            self,
            tenant,
            entity_type,
        ))
    }

    fn lookup_by_key<'a>(
        &'a self,
        tenant: &'a str,
        entity_type: &'a str,
        key_name: &'a str,
        key_hash: &'a str,
    ) -> EventStoreFuture<'a, Result<Option<String>, PersistenceError>> {
        Box::pin(EventStore::lookup_by_key(
            self,
            tenant,
            entity_type,
            key_name,
            key_hash,
        ))
    }

    fn backfill_entity_keys<'a>(
        &'a self,
        tenant: &'a str,
        entity_type: &'a str,
        entity_id: &'a str,
        key_rows: &'a [temper_runtime::persistence::EntityKeyRow],
    ) -> EventStoreFuture<'a, Result<(), PersistenceError>> {
        Box::pin(EventStore::backfill_entity_keys(
            self,
            tenant,
            entity_type,
            entity_id,
            key_rows,
        ))
    }

    fn mark_key_index_backfilled<'a>(
        &'a self,
        tenant: &'a str,
        entity_type: &'a str,
        key_set: &'a str,
    ) -> EventStoreFuture<'a, Result<(), PersistenceError>> {
        Box::pin(EventStore::mark_key_index_backfilled(
            self,
            tenant,
            entity_type,
            key_set,
        ))
    }

    fn key_index_backfilled_types<'a>(
        &'a self,
        tenant: &'a str,
    ) -> EventStoreFuture<'a, Result<Vec<(String, String)>, PersistenceError>> {
        Box::pin(EventStore::key_index_backfilled_types(self, tenant))
    }

    fn keyed_entity_ids_for_type<'a>(
        &'a self,
        tenant: &'a str,
        entity_type: &'a str,
    ) -> EventStoreFuture<'a, Result<Vec<String>, PersistenceError>> {
        Box::pin(EventStore::keyed_entity_ids_for_type(
            self,
            tenant,
            entity_type,
        ))
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

    fn list_entity_ids_limited<'a>(
        &'a self,
        tenant: &'a str,
        entity_type: Option<&'a str>,
        limit: usize,
    ) -> EventStoreFuture<'a, Result<Vec<(String, String)>, PersistenceError>> {
        Box::pin(EventStore::list_entity_ids_limited(
            self,
            tenant,
            entity_type,
            limit,
        ))
    }

    fn list_journal_ids_page<'a>(
        &'a self,
        tenant: &'a str,
        entity_type: Option<&'a str>,
        after: Option<(&'a str, &'a str)>,
        limit: usize,
    ) -> EventStoreFuture<'a, Result<Vec<(String, String)>, PersistenceError>> {
        Box::pin(EventStore::list_journal_ids_page(
            self,
            tenant,
            entity_type,
            after,
            limit,
        ))
    }

    dyn_stream_publication_impl!();

    fn list_scoped_entity_ids_page<'a>(
        &'a self,
        tenant: &'a str,
        entity_type: &'a str,
        scope: &'a temper_runtime::persistence::schema_deployment::SchemaScope,
        bundle_digest: &'a str,
        after_entity_id: Option<&'a str>,
        limit: usize,
    ) -> EventStoreFuture<'a, Result<Vec<String>, PersistenceError>> {
        Box::pin(EventStore::list_scoped_entity_ids_page(
            self,
            tenant,
            entity_type,
            scope,
            bundle_digest,
            after_entity_id,
            limit,
        ))
    }

    fn scoped_entity_bundle_digests<'a>(
        &'a self,
        tenant: &'a str,
        entity_type: &'a str,
        entity_id: &'a str,
        scope: &'a temper_runtime::persistence::schema_deployment::SchemaScope,
        limit: usize,
    ) -> EventStoreFuture<'a, Result<Vec<String>, PersistenceError>> {
        Box::pin(EventStore::scoped_entity_bundle_digests(
            self,
            tenant,
            entity_type,
            entity_id,
            scope,
            limit,
        ))
    }

    fn scoped_bundle_write_version<'a>(
        &'a self,
        tenant: &'a str,
        scope: &'a temper_runtime::persistence::schema_deployment::SchemaScope,
        bundle_digest: &'a str,
    ) -> EventStoreFuture<'a, Result<u64, PersistenceError>> {
        Box::pin(EventStore::scoped_bundle_write_version(
            self,
            tenant,
            scope,
            bundle_digest,
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

    pub async fn read_events_limited(
        &self,
        persistence_id: &str,
        from_sequence: u64,
        limit: usize,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        self.0
            .read_events_limited(persistence_id, from_sequence, limit)
            .await
    }

    pub async fn read_latest_events(
        &self,
        persistence_id: &str,
        limit: usize,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        self.0.read_latest_events(persistence_id, limit).await
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
        reconcile_vectors: bool,
    ) -> Result<u64, PersistenceError> {
        self.0
            .append_with_index_rows(
                persistence_id,
                expected_sequence,
                events,
                key_rows,
                vector_rows,
                reconcile_vectors,
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

    pub async fn backfill_entity_keys(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        key_rows: &[temper_runtime::persistence::EntityKeyRow],
    ) -> Result<(), PersistenceError> {
        self.0
            .backfill_entity_keys(tenant, entity_type, entity_id, key_rows)
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

    pub async fn list_journal_ids_page(
        &self,
        tenant: &str,
        entity_type: Option<&str>,
        after: Option<(&str, &str)>,
        limit: usize,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        self.0
            .list_journal_ids_page(tenant, entity_type, after, limit)
            .await
    }

    pub async fn list_scoped_entity_ids_page(
        &self,
        tenant: &str,
        entity_type: &str,
        scope: &temper_runtime::persistence::schema_deployment::SchemaScope,
        bundle_digest: &str,
        after_entity_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<String>, PersistenceError> {
        self.0
            .list_scoped_entity_ids_page(
                tenant,
                entity_type,
                scope,
                bundle_digest,
                after_entity_id,
                limit,
            )
            .await
    }

    boxed_stream_publication_methods!();

    /// Return the bounded durable bundle identities for one scoped entity.
    pub async fn scoped_entity_bundle_digests(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        scope: &temper_runtime::persistence::schema_deployment::SchemaScope,
        limit: usize,
    ) -> Result<Vec<String>, PersistenceError> {
        self.0
            .scoped_entity_bundle_digests(tenant, entity_type, entity_id, scope, limit)
            .await
    }

    pub async fn scoped_bundle_write_version(
        &self,
        tenant: &str,
        scope: &temper_runtime::persistence::schema_deployment::SchemaScope,
        bundle_digest: &str,
    ) -> Result<u64, PersistenceError> {
        self.0
            .scoped_bundle_write_version(tenant, scope, bundle_digest)
            .await
    }
}

/// Observe/trajectory read capability.
#[async_trait::async_trait]
pub trait ObserveReadStore: Send + Sync {
    async fn load_recent_trajectories(
        &self,
        tenant: &str,
        limit: i64,
    ) -> Result<Vec<TursoTrajectoryRow>, PersistenceError>;

    async fn load_unmet_intent_rows(
        &self,
        tenant: &str,
    ) -> Result<Vec<UnmetIntentAggRow>, PersistenceError>;

    async fn load_submit_spec_timestamps(
        &self,
        tenant: &str,
    ) -> Result<BTreeMap<String, String>, PersistenceError>;

    async fn count_trajectories_by_tenant(&self)
    -> Result<BTreeMap<String, u64>, PersistenceError>;

    async fn query_trajectory_stats(
        &self,
        tenant: &str,
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

    /// One session's rows, oldest first, in the order the kernel wrote them.
    ///
    /// Conformance replays a session as a state-machine run, so the ordering
    /// is part of the contract, not an implementation detail.
    async fn query_trajectories_by_session(
        &self,
        session_id: &str,
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
#[derive(Clone, Copy, Debug)]
pub struct EvolutionRecordWrite<'a> {
    /// Tenant that owns the record.
    pub tenant: &'a str,
    /// Stable evolution record identifier.
    pub id: &'a str,
    /// Evolution record kind.
    pub record_type: &'a str,
    /// Current record status.
    pub status: &'a str,
    /// Principal that created the record.
    pub created_by: &'a str,
    /// Optional predecessor record identifier.
    pub derived_from: Option<&'a str>,
    /// Serialized record payload.
    pub data_json: &'a str,
}

#[async_trait::async_trait]
pub trait EvolutionStore: Send + Sync {
    #[allow(clippy::too_many_arguments)]
    async fn upsert_feature_request(
        &self,
        tenant: &str,
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
        tenant: &str,
        disposition: Option<&str>,
    ) -> Result<Vec<FeatureRequestRow>, PersistenceError>;

    async fn update_feature_request(
        &self,
        tenant: &str,
        id: &str,
        disposition: &str,
        developer_notes: Option<&str>,
    ) -> Result<bool, PersistenceError>;

    async fn insert_evolution_record(
        &self,
        record: EvolutionRecordWrite<'_>,
    ) -> Result<(), PersistenceError>;

    async fn get_evolution_record(
        &self,
        tenant: &str,
        id: &str,
    ) -> Result<Option<EvolutionRecordRow>, PersistenceError>;

    async fn list_evolution_records(
        &self,
        tenant: &str,
        record_type: Option<&str>,
        status: Option<&str>,
    ) -> Result<Vec<EvolutionRecordRow>, PersistenceError>;

    async fn list_ranked_insights(
        &self,
        tenant: &str,
    ) -> Result<Vec<EvolutionRecordRow>, PersistenceError>;
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

    /// Mark a queued trajectory as persisted.
    ///
    /// Addressed by `(tenant, trajectory_id)`, the identity the row is keyed
    /// by: the id comes from the uploading harness, so two tenants can hold
    /// the same one.
    async fn mark_ots_trajectory_persisted(
        &self,
        tenant: &str,
        trajectory_id: &str,
    ) -> Result<(), PersistenceError>;

    /// Mark a queued trajectory as failed, addressed the same way.
    async fn mark_ots_trajectory_failed(
        &self,
        tenant: &str,
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

    /// Load a full OTS trajectory by tenant and ID.
    ///
    /// Tenant is part of the lookup so a trajectory id taken from a request
    /// path cannot read another tenant's trace out of a shared store.
    async fn get_ots_trajectory(
        &self,
        tenant: &str,
        trajectory_id: &str,
    ) -> Result<Option<OtsTrajectoryDocument>, PersistenceError>;
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

    /// Read a legacy database blob only when its stored size is within the
    /// caller's allocation budget. `None` means missing or over budget.
    async fn get_blob_if_size_at_most(
        &self,
        key: &str,
        max_bytes: usize,
    ) -> Result<Option<Vec<u8>>, String>;
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
#[derive(Clone, Copy, Debug)]
pub struct PolicyApprovalCommit<'a> {
    /// Tenant that owns both rows.
    pub tenant: &'a str,
    /// Pending decision to transition.
    pub decision_id: &'a str,
    /// Serialized approved decision.
    pub approved_decision_json: &'a str,
    /// Policy row created by the decision.
    pub policy_id: &'a str,
    /// Approved Cedar source.
    pub cedar_text: &'a str,
    /// Principal that approved the decision.
    pub created_by: &'a str,
}

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

    async fn get_pending_decision(
        &self,
        tenant: &str,
        id: &str,
    ) -> Result<Option<String>, PersistenceError>;

    async fn commit_policy_approval(
        &self,
        commit: PolicyApprovalCommit<'_>,
    ) -> Result<(), PersistenceError>;

    async fn rollback_policy_approval(
        &self,
        tenant: &str,
        decision_id: &str,
        pending_decision_json: &str,
        policy_id: &str,
    ) -> Result<(), PersistenceError>;
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
    pub schema_deployments: Option<Arc<dyn SchemaDeploymentStoreDyn>>,
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
        schema_deployments: Option<Arc<dyn SchemaDeploymentStoreDyn>>,
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
            schema_deployments,
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
            Some(Arc::new(SingleMetadataStoreProvider::new(store.clone()))),
            Some(store.clone() as Arc<dyn SchemaDeploymentStoreDyn>),
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
            Some(Arc::new(SingleMetadataStoreProvider::new(store.clone()))),
            Some(store.clone() as Arc<dyn SchemaDeploymentStoreDyn>),
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
            Some(router.clone() as Arc<dyn SchemaDeploymentStoreDyn>),
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
            BoxedEventStore::from_arc(store.clone()),
            None,
            None,
            platform,
            None,
            None,
            None,
            None,
            None,
            Some(store as Arc<dyn SchemaDeploymentStoreDyn>),
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
        tenant: &str,
        trajectory_id: &str,
    ) -> Result<(), PersistenceError> {
        self.mark_ots_trajectory_persisted(tenant, trajectory_id)
            .await
    }

    async fn mark_ots_trajectory_failed(
        &self,
        tenant: &str,
        trajectory_id: &str,
        error: &str,
    ) -> Result<(), PersistenceError> {
        self.mark_ots_trajectory_failed(tenant, trajectory_id, error)
            .await
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
        tenant: &str,
        trajectory_id: &str,
    ) -> Result<Option<OtsTrajectoryDocument>, PersistenceError> {
        self.get_ots_trajectory(tenant, trajectory_id)
            .await
            .map(|document| document.map(pg_ots_document_to_turso))
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
        tenant: &str,
        trajectory_id: &str,
    ) -> Result<(), PersistenceError> {
        self.mark_ots_trajectory_persisted(tenant, trajectory_id)
            .await
    }

    async fn mark_ots_trajectory_failed(
        &self,
        tenant: &str,
        trajectory_id: &str,
        error: &str,
    ) -> Result<(), PersistenceError> {
        self.mark_ots_trajectory_failed(tenant, trajectory_id, error)
            .await
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
        tenant: &str,
        trajectory_id: &str,
    ) -> Result<Option<OtsTrajectoryDocument>, PersistenceError> {
        self.get_ots_trajectory(tenant, trajectory_id).await
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

    async fn get_blob_if_size_at_most(
        &self,
        key: &str,
        max_bytes: usize,
    ) -> Result<Option<Vec<u8>>, String> {
        self.get_blob_if_size_at_most(key, max_bytes).await
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

    async fn get_blob_if_size_at_most(
        &self,
        key: &str,
        max_bytes: usize,
    ) -> Result<Option<Vec<u8>>, String> {
        self.get_blob_if_size_at_most(key, max_bytes).await
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

    async fn get_pending_decision(
        &self,
        tenant: &str,
        id: &str,
    ) -> Result<Option<String>, PersistenceError> {
        self.get_pending_decision(tenant, id).await
    }

    async fn commit_policy_approval(
        &self,
        commit: PolicyApprovalCommit<'_>,
    ) -> Result<(), PersistenceError> {
        self.commit_policy_approval(PostgresPolicyApprovalCommit {
            tenant: commit.tenant,
            decision_id: commit.decision_id,
            approved_decision_json: commit.approved_decision_json,
            policy_id: commit.policy_id,
            cedar_text: commit.cedar_text,
            created_by: commit.created_by,
        })
        .await
    }

    async fn rollback_policy_approval(
        &self,
        tenant: &str,
        decision_id: &str,
        pending_decision_json: &str,
        policy_id: &str,
    ) -> Result<(), PersistenceError> {
        self.rollback_policy_approval(tenant, decision_id, pending_decision_json, policy_id)
            .await
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

    async fn get_pending_decision(
        &self,
        tenant: &str,
        id: &str,
    ) -> Result<Option<String>, PersistenceError> {
        self.get_pending_decision(tenant, id).await
    }

    async fn commit_policy_approval(
        &self,
        commit: PolicyApprovalCommit<'_>,
    ) -> Result<(), PersistenceError> {
        self.commit_policy_approval(TursoPolicyApprovalCommit {
            tenant: commit.tenant,
            decision_id: commit.decision_id,
            approved_decision_json: commit.approved_decision_json,
            policy_id: commit.policy_id,
            cedar_text: commit.cedar_text,
            created_by: commit.created_by,
        })
        .await
    }

    async fn rollback_policy_approval(
        &self,
        tenant: &str,
        decision_id: &str,
        pending_decision_json: &str,
        policy_id: &str,
    ) -> Result<(), PersistenceError> {
        self.rollback_policy_approval(tenant, decision_id, pending_decision_json, policy_id)
            .await
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
        capture_seq: row.capture_seq,
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
        tenant: row.tenant,
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
        tenant: row.tenant,
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

fn pg_ots_document_to_turso(
    document: temper_store_postgres::PostgresOtsTrajectoryDocument,
) -> OtsTrajectoryDocument {
    OtsTrajectoryDocument {
        trajectory_id: document.trajectory_id,
        tenant: document.tenant,
        agent_id: document.agent_id,
        session_id: document.session_id,
        outcome: document.outcome,
        data: document.data,
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
        self.create_data_only_entity_native_with_state(
            record.tenant,
            record.entity_type,
            record.entity_id,
            record.status,
            record.fields,
            record.state,
            record.event,
        )
        .await
    }
}

pub(crate) use redaction::redact_secrets;
pub(crate) use trajectory_row::bounded_request_body;
use trajectory_row::{
    trajectory_matched_policy_ids_json, trajectory_request_body_json, trajectory_source_label,
};

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
            capture_seq: entry.capture_seq,
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
            capture_seq: entry.capture_seq,
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
                capture_seq: entry.capture_seq,
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
