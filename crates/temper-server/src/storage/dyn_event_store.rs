//! Object-safe forwarding adapter for concrete event stores.

use super::*;

impl<T> DynEventStore for T
where
    T: EventStore,
{
    fn supports_authoritative_key_index(&self) -> bool {
        EventStore::supports_authoritative_key_index(self)
    }

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
        reconciliation: IndexReconciliation,
    ) -> EventStoreFuture<'a, Result<u64, PersistenceError>> {
        Box::pin(EventStore::append_with_index_rows(
            self,
            persistence_id,
            expected_sequence,
            events,
            key_rows,
            vector_rows,
            reconciliation,
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

    fn lookup_by_key_with_sequence<'a>(
        &'a self,
        tenant: &'a str,
        entity_type: &'a str,
        key_name: &'a str,
        key_hash: &'a str,
    ) -> EventStoreFuture<'a, Result<Option<EntityKeyLookup>, PersistenceError>> {
        Box::pin(EventStore::lookup_by_key_with_sequence(
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
        expected_sequence: u64,
        key_rows: &'a [temper_runtime::persistence::EntityKeyRow],
    ) -> EventStoreFuture<'a, Result<(), PersistenceError>> {
        Box::pin(EventStore::backfill_entity_keys(
            self,
            tenant,
            entity_type,
            entity_id,
            expected_sequence,
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

    fn key_index_reconciliation_revision<'a>(
        &'a self,
        tenant: &'a str,
        entity_type: &'a str,
    ) -> EventStoreFuture<'a, Result<u64, PersistenceError>> {
        Box::pin(EventStore::key_index_reconciliation_revision(
            self,
            tenant,
            entity_type,
        ))
    }

    fn begin_key_index_backfill<'a>(
        &'a self,
        tenant: &'a str,
        entity_type: &'a str,
        key_set: &'a str,
    ) -> EventStoreFuture<'a, Result<u64, PersistenceError>> {
        Box::pin(EventStore::begin_key_index_backfill(
            self,
            tenant,
            entity_type,
            key_set,
        ))
    }

    fn mark_key_index_backfilled_if_revision<'a>(
        &'a self,
        tenant: &'a str,
        entity_type: &'a str,
        key_set: &'a str,
        expected_revision: u64,
    ) -> EventStoreFuture<'a, Result<bool, PersistenceError>> {
        Box::pin(EventStore::mark_key_index_backfilled_if_revision(
            self,
            tenant,
            entity_type,
            key_set,
            expected_revision,
        ))
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

    fn list_entity_ids_for_key_reconciliation<'a>(
        &'a self,
        tenant: &'a str,
        entity_type: &'a str,
    ) -> EventStoreFuture<'a, Result<Vec<String>, PersistenceError>> {
        Box::pin(EventStore::list_entity_ids_for_key_reconciliation(
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
}
