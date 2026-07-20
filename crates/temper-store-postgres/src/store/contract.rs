//! Thin runtime event-store contract over domain-focused Postgres operations.

use super::*;

impl EventStore for PostgresEventStore {
    async fn append(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
    ) -> Result<u64, PersistenceError> {
        PostgresEventStore::append(self, persistence_id, expected_sequence, events).await
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
        PostgresEventStore::append_with_index_rows(
            self,
            persistence_id,
            expected_sequence,
            events,
            key_rows,
            vector_rows,
            reconcile_vectors,
        )
        .await
    }

    async fn backfill_entity_keys(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        key_rows: &[temper_runtime::persistence::EntityKeyRow],
    ) -> Result<(), PersistenceError> {
        PostgresEventStore::backfill_entity_keys(self, tenant, entity_type, entity_id, key_rows)
            .await
    }

    async fn mark_key_index_backfilled(
        &self,
        tenant: &str,
        entity_type: &str,
        key_set: &str,
    ) -> Result<(), PersistenceError> {
        PostgresEventStore::mark_key_index_backfilled(self, tenant, entity_type, key_set).await
    }

    async fn key_index_backfilled_types(
        &self,
        tenant: &str,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        PostgresEventStore::key_index_backfilled_types(self, tenant).await
    }

    async fn keyed_entity_ids_for_type(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Vec<String>, PersistenceError> {
        PostgresEventStore::keyed_entity_ids_for_type(self, tenant, entity_type).await
    }

    async fn lookup_by_key(
        &self,
        tenant: &str,
        entity_type: &str,
        key_name: &str,
        key_hash: &str,
    ) -> Result<Option<String>, PersistenceError> {
        PostgresEventStore::lookup_by_key(self, tenant, entity_type, key_name, key_hash).await
    }

    async fn backfill_entity_vectors(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        vector_rows: &[EntityVectorRow],
    ) -> Result<(), PersistenceError> {
        PostgresEventStore::backfill_entity_vectors(
            self,
            tenant,
            entity_type,
            entity_id,
            vector_rows,
        )
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
        PostgresEventStore::vector_candidates(
            self,
            tenant,
            entity_type,
            decl_name,
            model_tag,
            limit,
        )
        .await
    }

    async fn mark_vector_index_backfilled(
        &self,
        tenant: &str,
        entity_type: &str,
        vector_set: &str,
    ) -> Result<(), PersistenceError> {
        PostgresEventStore::mark_vector_index_backfilled(self, tenant, entity_type, vector_set)
            .await
    }

    async fn vector_index_backfilled_types(
        &self,
        tenant: &str,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        PostgresEventStore::vector_index_backfilled_types(self, tenant).await
    }

    async fn vectored_entity_ids_for_type(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Vec<String>, PersistenceError> {
        PostgresEventStore::vectored_entity_ids_for_type(self, tenant, entity_type).await
    }

    async fn append_batch(
        &self,
        appends: &[PersistenceAppend],
    ) -> Result<Vec<PersistenceAppendResult>, PersistenceError> {
        PostgresEventStore::append_batch(self, appends).await
    }

    async fn read_events(
        &self,
        persistence_id: &str,
        from_sequence: u64,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        PostgresEventStore::read_events(self, persistence_id, from_sequence).await
    }

    async fn read_events_with_head(
        &self,
        persistence_id: &str,
        from_sequence: u64,
    ) -> Result<JournalRead, PersistenceError> {
        PostgresEventStore::read_events_with_head(self, persistence_id, from_sequence).await
    }

    async fn save_snapshot(
        &self,
        persistence_id: &str,
        sequence_nr: u64,
        snapshot: &[u8],
    ) -> Result<(), PersistenceError> {
        PostgresEventStore::save_snapshot(self, persistence_id, sequence_nr, snapshot).await
    }

    async fn replace_snapshot(
        &self,
        persistence_id: &str,
        sequence_nr: u64,
        expected_snapshot: &[u8],
        snapshot: &[u8],
    ) -> Result<(), PersistenceError> {
        PostgresEventStore::replace_snapshot(
            self,
            persistence_id,
            sequence_nr,
            expected_snapshot,
            snapshot,
        )
        .await
    }

    async fn load_snapshot(
        &self,
        persistence_id: &str,
    ) -> Result<Option<(u64, Vec<u8>)>, PersistenceError> {
        PostgresEventStore::load_snapshot(self, persistence_id).await
    }

    async fn list_entity_ids(
        &self,
        tenant: &str,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        PostgresEventStore::list_entity_ids(self, tenant).await
    }

    async fn list_entity_ids_by_type(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Vec<String>, PersistenceError> {
        PostgresEventStore::list_entity_ids_by_type(self, tenant, entity_type).await
    }

    async fn list_entity_ids_limited(
        &self,
        tenant: &str,
        entity_type: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        PostgresEventStore::list_entity_ids_limited(self, tenant, entity_type, limit).await
    }
}
