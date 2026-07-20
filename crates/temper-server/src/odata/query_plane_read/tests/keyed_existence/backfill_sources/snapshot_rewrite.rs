use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::*;
use crate::storage::{BackendLabel, BoxedEventStore, StorageStack};
use temper_runtime::persistence::{
    KeyIndexBackfillFence, PersistenceAppend, PersistenceAppendResult, PersistenceError,
};

#[derive(Clone)]
struct SnapshotRewriteDuringBackfillStore {
    inner: SimEventStore,
    persistence_id: String,
    replacement: Vec<u8>,
    rewritten: Arc<AtomicBool>,
}

impl EventStore for SnapshotRewriteDuringBackfillStore {
    fn supports_authoritative_key_index(&self) -> bool {
        true
    }

    async fn append(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
    ) -> Result<u64, PersistenceError> {
        EventStore::append(&self.inner, persistence_id, expected_sequence, events).await
    }

    async fn append_batch(
        &self,
        appends: &[PersistenceAppend],
    ) -> Result<Vec<PersistenceAppendResult>, PersistenceError> {
        EventStore::append_batch(&self.inner, appends).await
    }

    async fn read_events(
        &self,
        persistence_id: &str,
        from_sequence: u64,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        EventStore::read_events(&self.inner, persistence_id, from_sequence).await
    }

    async fn save_snapshot(
        &self,
        persistence_id: &str,
        sequence_nr: u64,
        snapshot: &[u8],
    ) -> Result<(), PersistenceError> {
        EventStore::save_snapshot(&self.inner, persistence_id, sequence_nr, snapshot).await
    }

    async fn load_snapshot(
        &self,
        persistence_id: &str,
    ) -> Result<Option<(u64, Vec<u8>)>, PersistenceError> {
        let captured = EventStore::load_snapshot(&self.inner, persistence_id).await?;
        if persistence_id == self.persistence_id && !self.rewritten.swap(true, Ordering::SeqCst) {
            let sequence_nr = captured
                .as_ref()
                .map(|(sequence_nr, _)| *sequence_nr)
                .expect("rewrite fixture requires a captured snapshot");
            EventStore::save_snapshot(&self.inner, persistence_id, sequence_nr, &self.replacement)
                .await?;
        }
        Ok(captured)
    }

    async fn list_entity_ids(
        &self,
        tenant: &str,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        EventStore::list_entity_ids(&self.inner, tenant).await
    }

    async fn list_entity_ids_by_type(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Vec<String>, PersistenceError> {
        EventStore::list_entity_ids_by_type(&self.inner, tenant, entity_type).await
    }

    async fn list_entity_ids_for_key_reconciliation(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Vec<String>, PersistenceError> {
        EventStore::list_entity_ids_for_key_reconciliation(&self.inner, tenant, entity_type).await
    }

    async fn backfill_entity_keys(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        expected_sequence: u64,
        contract_fence: KeyIndexBackfillFence<'_>,
        key_rows: &[EntityKeyRow],
    ) -> Result<(), PersistenceError> {
        EventStore::backfill_entity_keys(
            &self.inner,
            tenant,
            entity_type,
            entity_id,
            expected_sequence,
            contract_fence,
            key_rows,
        )
        .await
    }

    async fn lookup_by_key(
        &self,
        tenant: &str,
        entity_type: &str,
        key_name: &str,
        key_hash: &str,
    ) -> Result<Option<String>, PersistenceError> {
        EventStore::lookup_by_key(&self.inner, tenant, entity_type, key_name, key_hash).await
    }

    async fn key_index_backfilled_types(
        &self,
        tenant: &str,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        EventStore::key_index_backfilled_types(&self.inner, tenant).await
    }

    async fn key_index_reconciliation_revision(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<u64, PersistenceError> {
        EventStore::key_index_reconciliation_revision(&self.inner, tenant, entity_type).await
    }

    async fn begin_key_index_backfill(
        &self,
        tenant: &str,
        entity_type: &str,
        key_set: &str,
    ) -> Result<u64, PersistenceError> {
        EventStore::begin_key_index_backfill(&self.inner, tenant, entity_type, key_set).await
    }

    async fn mark_key_index_backfilled_if_revision(
        &self,
        tenant: &str,
        entity_type: &str,
        key_set: &str,
        expected_revision: u64,
    ) -> Result<bool, PersistenceError> {
        EventStore::mark_key_index_backfilled_if_revision(
            &self.inner,
            tenant,
            entity_type,
            key_set,
            expected_revision,
        )
        .await
    }
}

#[tokio::test]
async fn same_sequence_snapshot_rewrite_invalidates_the_backfill_row() {
    let (_guard, _clock, _ids) = install_deterministic_context(269);
    let tenant = TenantId::default();
    let before_workspace = "ws-before-rewrite";
    let after_workspace = "ws-after-rewrite";
    let journal_path = "/journal-path";
    let entity_id = "ord-snapshot-rewrite";
    let persistence_id = format!("{tenant}:Order:{entity_id}");
    let inner = SimEventStore::no_faults(269);

    EventStore::save_snapshot(
        &inner,
        &persistence_id,
        1,
        &legacy_snapshot(entity_id, before_workspace, "/snapshot-path"),
    )
    .await
    .expect("seed snapshot baseline before repair");
    EventStore::append(
        &inner,
        &persistence_id,
        0,
        &[journal_path_delta(
            &persistence_id,
            journal_path,
            "snapshot-rewrite-delta",
        )],
    )
    .await
    .expect("seed journal delta at the same numeric sequence");

    let store = SnapshotRewriteDuringBackfillStore {
        inner: inner.clone(),
        persistence_id: persistence_id.clone(),
        replacement: legacy_snapshot(entity_id, after_workspace, "/snapshot-path"),
        rewritten: Arc::new(AtomicBool::new(false)),
    };
    let mut state = build_order_state("snapshot-rewrite-backfill");
    state.set_storage_stack(StorageStack::new(
        BackendLabel::Sim,
        BoxedEventStore::new(store),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    ));

    state.populate_key_index_from_snapshots(&tenant).await;
    assert!(
        !state
            .key_index_backfill_complete(&tenant, "Order", ORDER_KEY_SET_SIGNATURE)
            .await,
        "a same-sequence snapshot content change must reject the captured repair row"
    );
    assert_eq!(
        EventStore::lookup_by_key(
            &inner,
            tenant.as_str(),
            "Order",
            "ws_path",
            &ws_path_hash(before_workspace, journal_path),
        )
        .await
        .expect("lookup stale mixed ownership"),
        None,
        "the pre-rewrite snapshot component must not be certified"
    );

    state.populate_key_index_from_snapshots(&tenant).await;
    assert!(
        state
            .key_index_backfill_complete(&tenant, "Order", ORDER_KEY_SET_SIGNATURE)
            .await,
        "the next pass must converge on the stable replacement snapshot"
    );
    assert_eq!(
        EventStore::lookup_by_key(
            &inner,
            tenant.as_str(),
            "Order",
            "ws_path",
            &ws_path_hash(after_workspace, journal_path),
        )
        .await
        .expect("lookup stable replacement ownership")
        .as_deref(),
        Some(entity_id)
    );
}
