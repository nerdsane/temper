//! Journal operations on the server's object-safe event-store handle.

use temper_runtime::persistence::{
    EntityKeyRow, PersistenceAppend, PersistenceAppendResult, PersistenceEnvelope,
    PersistenceError, PersistenceSequenceGuard, validate_guarded_persistence_append_batch,
    validate_persistence_append_batch,
};

use super::{BoxedEventStore, append_reconciliation};

impl BoxedEventStore {
    pub async fn append(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
    ) -> Result<u64, PersistenceError> {
        let result = self
            .0
            .append(persistence_id, expected_sequence, events)
            .await;
        append_reconciliation::reconcile_append_result(
            self.0.as_ref(),
            persistence_id,
            expected_sequence,
            events,
            result,
        )
        .await
    }

    pub async fn append_batch(
        &self,
        appends: &[PersistenceAppend],
    ) -> Result<Vec<PersistenceAppendResult>, PersistenceError> {
        validate_persistence_append_batch(appends)?;
        let result = self.0.append_batch(appends).await;
        append_reconciliation::reconcile_batch_result(self.0.as_ref(), appends, result).await
    }

    /// Commit a batch only if independent journal sequences still match.
    ///
    /// Guarded errors are never reconciled from target journal bytes: the
    /// target event cannot prove that every compare-only guard passed in the
    /// same atomic commit.
    pub async fn append_batch_guarded(
        &self,
        appends: &[PersistenceAppend],
        guards: &[PersistenceSequenceGuard],
    ) -> Result<Vec<PersistenceAppendResult>, PersistenceError> {
        validate_guarded_persistence_append_batch(appends, guards)?;
        self.0.append_batch_guarded(appends, guards).await
    }

    pub async fn read_events(
        &self,
        persistence_id: &str,
        from_sequence: u64,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        self.0.read_events(persistence_id, from_sequence).await
    }

    /// Read a storage-enforced bounded journal suffix.
    pub async fn read_events_bounded(
        &self,
        persistence_id: &str,
        from_sequence: u64,
        limit: usize,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        self.0
            .read_events_bounded(persistence_id, from_sequence, limit)
            .await
    }

    pub async fn read_latest_events(
        &self,
        persistence_ids: &[String],
    ) -> Result<Vec<Option<PersistenceEnvelope>>, PersistenceError> {
        self.0.read_latest_events(persistence_ids).await
    }

    pub async fn append_with_keys(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
        key_rows: &[EntityKeyRow],
    ) -> Result<u64, PersistenceError> {
        // Journal equality cannot prove the complete declared-key replacement
        // committed: K1, K2, and an explicit clear can all accompany the same
        // event envelope. Preserve any backend error until stores expose an
        // authoritative complete-key-set proof.
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

    pub async fn retire_entity_keys_through_sequence(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        delete_sequence: u64,
    ) -> Result<(), PersistenceError> {
        self.0
            .retire_entity_keys_through_sequence(tenant, entity_type, entity_id, delete_sequence)
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
}

#[cfg(all(test, feature = "sim"))]
mod tests {
    use temper_runtime::persistence::{
        EntityKeyRow, EventMetadata, EventStore, PersistenceAppend, PersistenceEnvelope,
    };
    use temper_store_sim::SimEventStore;

    use super::BoxedEventStore;

    fn envelope(event_id: u128, actor_id: &str) -> PersistenceEnvelope {
        PersistenceEnvelope {
            sequence_nr: 0,
            event_type: "Updated".to_string(),
            payload: serde_json::json!({"value": "same-journal-intent"}),
            metadata: EventMetadata {
                event_id: uuid::Uuid::from_u128(event_id),
                causation_id: uuid::Uuid::from_u128(event_id + 100),
                correlation_id: uuid::Uuid::from_u128(event_id + 200),
                timestamp: chrono::DateTime::UNIX_EPOCH,
                actor_id: actor_id.to_string(),
            },
        }
    }

    fn key(hash: &str) -> EntityKeyRow {
        EntityKeyRow {
            key_name: "by_name".to_string(),
            key_hash: hash.to_string(),
        }
    }

    #[tokio::test]
    async fn key_replacement_or_clear_cannot_reconcile_from_journal_only() {
        let store = SimEventStore::no_faults(901);
        let persistence_id = "default:Widget:key-proof";
        let event = envelope(1, persistence_id);
        let first_key = key("K1");
        EventStore::append_with_keys(
            &store,
            persistence_id,
            0,
            std::slice::from_ref(&event),
            std::slice::from_ref(&first_key),
        )
        .await
        .expect("seed event and K1");

        let boxed = BoxedEventStore::new(store.clone());
        assert!(
            boxed
                .append_with_keys(
                    persistence_id,
                    0,
                    std::slice::from_ref(&event),
                    &[key("K2")],
                )
                .await
                .is_err(),
            "identical journal bytes must not prove a different key set"
        );
        assert!(
            boxed
                .append_with_keys(persistence_id, 0, std::slice::from_ref(&event), &[])
                .await
                .is_err(),
            "identical journal bytes must not prove an explicit key clear"
        );
        assert_eq!(
            boxed
                .append(persistence_id, 0, std::slice::from_ref(&event))
                .await
                .expect("raw append may reconcile because it preserves keys"),
            1
        );
        assert_eq!(
            EventStore::lookup_by_key(&store, "default", "Widget", "by_name", "K1")
                .await
                .expect("lookup K1"),
            Some("key-proof".to_string())
        );
        assert_eq!(
            EventStore::lookup_by_key(&store, "default", "Widget", "by_name", "K2")
                .await
                .expect("lookup K2"),
            None
        );
    }

    #[tokio::test]
    async fn batch_with_any_key_intent_cannot_reconcile_from_journals_only() {
        let store = SimEventStore::no_faults(902);
        let raw_id = "default:Widget:raw-member";
        let keyed_id = "default:Widget:keyed-member";
        let raw_event = envelope(2, raw_id);
        let keyed_event = envelope(3, keyed_id);
        let committed = vec![
            PersistenceAppend {
                persistence_id: raw_id.to_string(),
                expected_sequence: 0,
                events: vec![raw_event.clone()],
                key_rows: None,
                vector_rows: Vec::new(),
                reconcile_vectors: false,
            },
            PersistenceAppend {
                persistence_id: keyed_id.to_string(),
                expected_sequence: 0,
                events: vec![keyed_event.clone()],
                key_rows: Some(vec![key("K1")]),
                vector_rows: Vec::new(),
                reconcile_vectors: false,
            },
        ];
        EventStore::append_batch(&store, &committed)
            .await
            .expect("seed atomic mixed batch");

        let boxed = BoxedEventStore::new(store.clone());
        let mut changed_key = committed.clone();
        changed_key[1].key_rows = Some(vec![key("K2")]);
        assert!(boxed.append_batch(&changed_key).await.is_err());

        let mut explicit_clear = committed.clone();
        explicit_clear[1].key_rows = Some(Vec::new());
        assert!(boxed.append_batch(&explicit_clear).await.is_err());

        let preserve_only = committed
            .iter()
            .cloned()
            .map(|mut append| {
                append.key_rows = None;
                append
            })
            .collect::<Vec<_>>();
        let reconciled = boxed
            .append_batch(&preserve_only)
            .await
            .expect("journal-only preserve batch can be proven exactly");
        assert_eq!(reconciled.len(), 2);
        assert!(reconciled.iter().all(|append| append.sequence_nr == 1));
        assert_eq!(
            EventStore::lookup_by_key(&store, "default", "Widget", "by_name", "K1")
                .await
                .expect("lookup K1"),
            Some("keyed-member".to_string())
        );
        assert_eq!(
            EventStore::lookup_by_key(&store, "default", "Widget", "by_name", "K2")
                .await
                .expect("lookup K2"),
            None
        );
    }
}
