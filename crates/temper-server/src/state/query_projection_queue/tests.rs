use super::*;
use crate::storage::{
    EntityCatalogRow, QueryFieldIndexOrder, QueryFieldIndexPage, QueryProjectionFieldsRow,
};
use async_trait::async_trait;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use temper_runtime::persistence::PersistenceError;

#[derive(Default)]
struct RecordingQueryPlane {
    upserts: AtomicUsize,
    fail_upserts: AtomicUsize,
}

#[async_trait]
impl QueryPlaneStore for RecordingQueryPlane {
    async fn upsert_projection(
        &self,
        _tenant: &str,
        _entity_type: &str,
        _entity_id: &str,
        _status: &str,
        _fields: &serde_json::Value,
        _state: &serde_json::Value,
        _sequence_nr: u64,
    ) -> Result<(), PersistenceError> {
        self.upserts.fetch_add(1, Ordering::SeqCst);
        if self.fail_upserts.load(Ordering::SeqCst) > 0 {
            self.fail_upserts.fetch_sub(1, Ordering::SeqCst);
            return Err(PersistenceError::Storage(
                "transient projection failure".to_string(),
            ));
        }
        Ok(())
    }

    async fn remove_projection(
        &self,
        _tenant: &str,
        _entity_type: &str,
        _entity_id: &str,
    ) -> Result<(), PersistenceError> {
        Ok(())
    }

    async fn remove_projection_through_sequence(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        _sequence_nr: u64,
    ) -> Result<(), PersistenceError> {
        self.remove_projection(tenant, entity_type, entity_id).await
    }

    async fn query_field_index(
        &self,
        _tenant: &str,
        _entity_type: &str,
        _where_clause: &str,
        _params: Vec<String>,
    ) -> Result<Option<Vec<String>>, PersistenceError> {
        Ok(None)
    }

    async fn query_field_index_page(
        &self,
        _tenant: &str,
        _entity_type: &str,
        _where_clause: &str,
        _params: Vec<String>,
        _order_by: &[QueryFieldIndexOrder],
        _skip: usize,
        _top: usize,
        _include_count: bool,
    ) -> Result<Option<QueryFieldIndexPage>, PersistenceError> {
        Ok(None)
    }

    async fn load_projection_fields_many(
        &self,
        _tenant: &str,
        _entity_type: &str,
        _entity_ids: &[String],
        _field_names: &[&str],
    ) -> Result<Option<Vec<QueryProjectionFieldsRow>>, PersistenceError> {
        Ok(None)
    }

    async fn load_entity_catalog_rows(
        &self,
        _tenant: &str,
        _entity_type: &str,
        _entity_ids: &[String],
    ) -> Result<Option<Vec<EntityCatalogRow>>, PersistenceError> {
        Ok(None)
    }

    async fn projected_entity_counts_by_tenant(
        &self,
    ) -> Result<Option<Vec<(String, u64)>>, PersistenceError> {
        Ok(None)
    }
}

#[test]
fn enqueue_coalesces_newer_entity_update() {
    let store = Arc::new(RecordingQueryPlane::default());
    let queue = QueryProjectionWriteQueue::new_for_test(store, 10, 10);

    assert_eq!(
        queue.enqueue_upsert(
            "tenant".to_string(),
            "Session".to_string(),
            "s-1".to_string(),
            "Running".to_string(),
            serde_json::json!({"Title": "old"}),
            serde_json::json!({"sequence_nr": 1}),
            1,
            "test",
        ),
        ProjectionEnqueueOutcome::Enqueued
    );
    assert_eq!(
        queue.enqueue_upsert(
            "tenant".to_string(),
            "Session".to_string(),
            "s-1".to_string(),
            "Idle".to_string(),
            serde_json::json!({"Title": "new"}),
            serde_json::json!({"sequence_nr": 2}),
            2,
            "test",
        ),
        ProjectionEnqueueOutcome::Coalesced
    );

    let batch = queue.take_batch();
    assert_eq!(batch.len(), 1);
    assert_eq!(batch[0].sequence_nr, 2);
}

#[test]
fn enqueue_skips_stale_update_before_store_access() {
    let store = Arc::new(RecordingQueryPlane::default());
    let queue = QueryProjectionWriteQueue::new_for_test(store, 10, 10);

    assert_eq!(
        queue.enqueue_upsert(
            "tenant".to_string(),
            "Session".to_string(),
            "s-1".to_string(),
            "Idle".to_string(),
            serde_json::json!({}),
            serde_json::json!({"sequence_nr": 3}),
            3,
            "test",
        ),
        ProjectionEnqueueOutcome::Enqueued
    );
    assert_eq!(
        queue.enqueue_upsert(
            "tenant".to_string(),
            "Session".to_string(),
            "s-1".to_string(),
            "Running".to_string(),
            serde_json::json!({}),
            serde_json::json!({"sequence_nr": 2}),
            2,
            "test",
        ),
        ProjectionEnqueueOutcome::StaleSkipped
    );

    let batch = queue.take_batch();
    assert_eq!(batch.len(), 1);
    assert_eq!(batch[0].sequence_nr, 3);
}

#[test]
fn enqueue_rejects_new_entity_when_capacity_is_exhausted() {
    let store = Arc::new(RecordingQueryPlane::default());
    let queue = QueryProjectionWriteQueue::new_for_test(store, 1, 10);

    assert_eq!(
        queue.enqueue_remove(
            "tenant".to_string(),
            "Session".to_string(),
            "s-1".to_string(),
            1,
            "test",
        ),
        ProjectionEnqueueOutcome::Enqueued
    );
    assert_eq!(
        queue.enqueue_remove(
            "tenant".to_string(),
            "Session".to_string(),
            "s-2".to_string(),
            1,
            "test",
        ),
        ProjectionEnqueueOutcome::Full
    );
}

#[tokio::test]
async fn failed_projection_write_is_retried_before_drop() {
    let store = Arc::new(RecordingQueryPlane {
        fail_upserts: AtomicUsize::new(1),
        ..RecordingQueryPlane::default()
    });
    let queue = QueryProjectionWriteQueue::new_for_test_with_retry(
        store.clone(),
        10,
        10,
        2,
        Duration::from_millis(1),
    );

    assert_eq!(
        queue.enqueue_upsert(
            "tenant".to_string(),
            "Session".to_string(),
            "s-1".to_string(),
            "Idle".to_string(),
            serde_json::json!({"Title": "retry"}),
            serde_json::json!({"sequence_nr": 4}),
            4,
            "test",
        ),
        ProjectionEnqueueOutcome::Enqueued
    );
    let mut batch = queue.take_batch();
    assert_eq!(batch.len(), 1);

    queue.apply(batch.pop().expect("queued update")).await;

    assert_eq!(store.upserts.load(Ordering::SeqCst), 2);
    assert_eq!(store.fail_upserts.load(Ordering::SeqCst), 0);
}
