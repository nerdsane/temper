//! Cloneable boxed event-store facade.

use super::*;

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

    /// Read a journal tail and its head from the same logical store snapshot.
    pub async fn read_events_with_head(
        &self,
        persistence_id: &str,
        from_sequence: u64,
    ) -> Result<JournalRead, PersistenceError> {
        self.0
            .read_events_with_head(persistence_id, from_sequence)
            .await
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

    /// Compare and replace one existing snapshot without creating a new boundary.
    pub async fn replace_snapshot(
        &self,
        persistence_id: &str,
        sequence_nr: u64,
        expected_snapshot: &[u8],
        snapshot: &[u8],
    ) -> Result<(), PersistenceError> {
        self.0
            .replace_snapshot(persistence_id, sequence_nr, expected_snapshot, snapshot)
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
}
