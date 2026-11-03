//! Object-safe event journal adapter for runtime event stores.

use std::future::Future;
use std::pin::Pin;

use temper_runtime::persistence::{
    EventStore, PersistenceAppend, PersistenceAppendResult, PersistenceEnvelope, PersistenceError,
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

    fn read_events_page<'a>(
        &'a self,
        persistence_id: &'a str,
        before_sequence: Option<u64>,
        limit: usize,
    ) -> EventStoreFuture<'a, Result<Vec<PersistenceEnvelope>, PersistenceError>>;

    fn first_event_at<'a>(
        &'a self,
        persistence_id: &'a str,
    ) -> EventStoreFuture<'a, Result<Option<chrono::DateTime<chrono::Utc>>, PersistenceError>> {
        Box::pin(async move {
            Ok(self
                .read_events_page(persistence_id, Some(2), 1)
                .await?
                .first()
                .filter(|event| event.sequence_nr == 1)
                .map(|event| {
                    event
                        .payload
                        .get("timestamp")
                        .and_then(|value| serde_json::from_value(value.clone()).ok())
                        .unwrap_or(event.metadata.timestamp)
                }))
        })
    }

    fn append_with_keys<'a>(
        &'a self,
        persistence_id: &'a str,
        expected_sequence: u64,
        events: &'a [PersistenceEnvelope],
        key_rows: &'a [temper_runtime::persistence::EntityKeyRow],
    ) -> EventStoreFuture<'a, Result<u64, PersistenceError>>;

    fn append_with_rows<'a>(
        &'a self,
        persistence_id: &'a str,
        expected_sequence: u64,
        events: &'a [PersistenceEnvelope],
        rows: temper_runtime::persistence::CommitRows<'a>,
    ) -> EventStoreFuture<'a, Result<u64, PersistenceError>>;

    fn commits_entity_rows(&self) -> bool;

    fn list_scheduled_actions<'a>(
        &'a self,
        tenant: &'a str,
    ) -> EventStoreFuture<
        'a,
        Result<Vec<temper_runtime::persistence::ScheduledActionRecord>, PersistenceError>,
    >;

    fn list_entity_scheduled_actions<'a>(
        &'a self,
        tenant: &'a str,
        entity_type: &'a str,
        entity_id: &'a str,
    ) -> EventStoreFuture<
        'a,
        Result<Vec<temper_runtime::persistence::ScheduledActionRecord>, PersistenceError>,
    >;

    fn delete_scheduled_action<'a>(
        &'a self,
        tenant: &'a str,
        entity_type: &'a str,
        entity_id: &'a str,
        arm_id: &'a str,
    ) -> EventStoreFuture<'a, Result<(), PersistenceError>>;

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

    fn latest_sequence_nr<'a>(
        &'a self,
        persistence_id: &'a str,
    ) -> EventStoreFuture<'a, Result<u64, PersistenceError>>;

    fn journal_heads<'a>(
        &'a self,
        tenant: &'a str,
    ) -> EventStoreFuture<'a, Result<Vec<temper_runtime::persistence::JournalHead>, PersistenceError>>;

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
}

#[path = "dyn_event_store/blanket.rs"]
mod blanket;
