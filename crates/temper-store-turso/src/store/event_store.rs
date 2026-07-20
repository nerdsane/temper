//! [`EventStore`] trait implementation for Turso/libSQL.

use libsql::{TransactionBehavior, Value, params, params_from_iter};
use std::time::Duration;
use temper_runtime::persistence::{
    EntityVectorCandidate, EntityVectorRow, EventMetadata, EventStore, JournalRead,
    PersistenceAppend, PersistenceAppendResult, PersistenceEnvelope, PersistenceError, pack_f32_le,
    storage_error, unpack_f32_le,
};
use temper_runtime::tenant::parse_persistence_id_parts;
use tracing::{error, instrument, warn};

use super::TursoEventStore;
use super::append_config::{append_attempt_timeout, append_max_attempts};
use super::instrumentation::record_turso_query_duration;
use super::write_gate::WritePriority;
use crate::metrics::record_turso_write_retry;
use crate::retry::{is_transient_write_error, retry_delay_ms};

const APPEND_BATCH_INSERT_CHUNK_ROWS: usize = 400;

struct PreparedEventInsert {
    tenant: String,
    entity_type: String,
    entity_id: String,
    sequence_nr: u64,
    event_type: String,
    payload_json: String,
    metadata_json: String,
    expected_sequence: u64,
}

mod append;
mod batch;
mod indexes;
mod reads;
mod snapshots;
mod tenants;
mod write_contract;

impl EventStore for TursoEventStore {
    async fn append(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
    ) -> Result<u64, PersistenceError> {
        self.append_impl(persistence_id, expected_sequence, events)
            .await
    }

    async fn lookup_by_key(
        &self,
        tenant: &str,
        entity_type: &str,
        key_name: &str,
        key_hash: &str,
    ) -> Result<Option<String>, PersistenceError> {
        self.lookup_by_key_impl(tenant, entity_type, key_name, key_hash)
            .await
    }

    async fn append_with_index_rows(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
        key_rows: &[temper_runtime::persistence::EntityKeyRow],
        vector_rows: &[EntityVectorRow],
        reconcile_vectors: bool,
    ) -> Result<u64, PersistenceError> {
        self.append_with_index_rows_impl(
            persistence_id,
            expected_sequence,
            events,
            key_rows,
            vector_rows,
            reconcile_vectors,
        )
        .await
    }

    async fn backfill_entity_vectors(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        vector_rows: &[EntityVectorRow],
    ) -> Result<(), PersistenceError> {
        self.backfill_entity_vectors_impl(tenant, entity_type, entity_id, vector_rows)
            .await
    }

    async fn vector_candidates(
        &self,
        tenant: &str,
        entity_type: &str,
        decl_name: &str,
        model_tag: &str,
        limit: usize,
    ) -> Result<Vec<EntityVectorCandidate>, PersistenceError> {
        self.vector_candidates_impl(tenant, entity_type, decl_name, model_tag, limit)
            .await
    }

    async fn mark_vector_index_backfilled(
        &self,
        tenant: &str,
        entity_type: &str,
        vector_set: &str,
    ) -> Result<(), PersistenceError> {
        self.mark_vector_index_backfilled_impl(tenant, entity_type, vector_set)
            .await
    }

    async fn vector_index_backfilled_types(
        &self,
        tenant: &str,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        self.vector_index_backfilled_types_impl(tenant).await
    }

    async fn vectored_entity_ids_for_type(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Vec<String>, PersistenceError> {
        self.vectored_entity_ids_for_type_impl(tenant, entity_type)
            .await
    }

    async fn append_batch(
        &self,
        appends: &[PersistenceAppend],
    ) -> Result<Vec<PersistenceAppendResult>, PersistenceError> {
        self.append_batch_impl(appends).await
    }

    async fn read_events(
        &self,
        persistence_id: &str,
        from_sequence: u64,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        self.read_events_impl(persistence_id, from_sequence).await
    }

    async fn read_events_with_head(
        &self,
        persistence_id: &str,
        from_sequence: u64,
    ) -> Result<JournalRead, PersistenceError> {
        self.read_events_with_head_impl(persistence_id, from_sequence)
            .await
    }

    async fn save_snapshot(
        &self,
        persistence_id: &str,
        sequence_nr: u64,
        snapshot: &[u8],
    ) -> Result<(), PersistenceError> {
        self.save_snapshot_impl(persistence_id, sequence_nr, snapshot)
            .await
    }

    async fn replace_snapshot(
        &self,
        persistence_id: &str,
        sequence_nr: u64,
        expected_snapshot: &[u8],
        snapshot: &[u8],
    ) -> Result<(), PersistenceError> {
        self.replace_snapshot_impl(persistence_id, sequence_nr, expected_snapshot, snapshot)
            .await
    }

    async fn load_snapshot(
        &self,
        persistence_id: &str,
    ) -> Result<Option<(u64, Vec<u8>)>, PersistenceError> {
        self.load_snapshot_impl(persistence_id).await
    }

    async fn list_entity_ids(
        &self,
        tenant: &str,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        self.list_entity_ids_impl(tenant).await
    }

    async fn list_entity_ids_by_type(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Vec<String>, PersistenceError> {
        self.list_entity_ids_by_type_impl(tenant, entity_type).await
    }

    async fn list_entity_ids_limited(
        &self,
        tenant: &str,
        entity_type: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        self.list_entity_ids_limited_impl(tenant, entity_type, limit)
            .await
    }
}
