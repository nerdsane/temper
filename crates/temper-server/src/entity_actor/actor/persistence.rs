//! Event and snapshot persistence helpers.

use super::*;

impl EntityActor {
    pub(super) async fn persist_overflow_blobs(
        blob_store: Option<&crate::blob_store::BlobStore>,
        blobs: &[crate::blobs::OverflowBlobWrite],
    ) -> Result<(), String> {
        let Some(blob_store) = blob_store else {
            return Err("field-overflow blobs require a configured object blob store".to_string());
        };
        crate::blobs::put_overflow_blobs(blob_store, blobs).await
    }

    /// Persistence ID for this entity: "tenant:EntityType:EntityId".
    pub(super) fn persistence_id(&self) -> String {
        format!("{}:{}:{}", self.tenant, self.entity_type, self.entity_id)
    }

    pub(super) fn field_sync_mode_for_backend(
        backend: Option<BackendLabel>,
        blob_store: Option<&crate::blob_store::BlobStore>,
    ) -> FieldSyncMode {
        match backend {
            Some(BackendLabel::Turso | BackendLabel::TursoRouted) => {
                FieldSyncMode::blob_refs_default()
            }
            Some(_) if blob_store.is_some() => FieldSyncMode::blob_refs_default(),
            _ => FieldSyncMode::InlineTruncate,
        }
    }

    /// Persist an event to the configured event store.
    pub(super) async fn persist_event(
        &self,
        store: &BoxedEventStore,
        backend: BackendLabel,
        persistence_id: &str,
        table: &TransitionTable,
        state: &mut EntityState,
        event: &EntityEvent,
    ) -> Result<(u64, PersistedStateTimeoutClock), PersistenceError> {
        let event_version = state
            .sequence_nr
            .checked_add(1)
            .expect("persisted state timeout clock version overflow");
        let (payload, state_timeout_clock) =
            encode_entity_event_payload(table, state, event, event_version)
                .map_err(|e| PersistenceError::Serialization(e.to_string()))?;
        let envelope = PersistenceEnvelope {
            sequence_nr: state.sequence_nr + 1,
            event_type: entity_event_type(event).to_string(),
            payload,
            metadata: EventMetadata {
                event_id: sim_uuid(),
                causation_id: sim_uuid(),
                correlation_id: sim_uuid(),
                timestamp: event.timestamp,
                actor_id: persistence_id.to_string(),
            },
        };

        // W2 / temper#146: measure append wait — the hypothesis is that
        // writer-lock / fsync serialization is a cold-start bottleneck.
        // ADR-0153/0155: derive the declared key rows AND the vector-index rows from
        // the new state and co-commit them with the journal append, so a keyed read
        // is correct without a scan and a kNN read reflects the write deterministically.
        let (key_rows, vector_rows, reconcile_vectors) = {
            // The type declares vector paths → the store reconciles this entity's
            // vector rows (delete stale + insert current) even when no row is emitted
            // this write (a delete transition or a cleared property), so stale rows are
            // purged instead of being ranked forever (ADR-0155).
            let reconcile_vectors = !table.vectors.is_empty();
            let mut key_rows = Vec::new();
            let mut vector_rows = Vec::new();
            if let Some(field_map) = state.fields.as_object() {
                for key in &table.keys {
                    if let Some(hash) =
                        crate::key_index::canonical_key_hash(&key.name, &key.properties, field_map)
                    {
                        key_rows.push(temper_runtime::persistence::EntityKeyRow {
                            key_name: key.name.clone(),
                            key_hash: hash,
                        });
                    }
                }
                // A soft-deleted (tombstone) entity is never indexed — it emits no
                // vector rows, so the reconcile below PURGES any it had, even though
                // its embedding field may still be present. Mirrors how the field-index
                // projection removes a deleted entity.
                let index_vectors = state.status != "Deleted";
                for decl in table.vectors.iter().filter(|_| index_vectors) {
                    // A vector is indexed only when its property parses to `dims`
                    // floats AND its model tag is a non-empty string — otherwise the
                    // path indexes nothing for this entity (like an incomplete key).
                    let Some(vector) = field_map
                        .get(&decl.property)
                        .and_then(|v| crate::vector_index::parse_vector_property(v, decl.dims))
                    else {
                        continue;
                    };
                    let Some(model_tag) = field_map
                        .get(&decl.model_property)
                        .and_then(|v| v.as_str())
                        .filter(|tag| !tag.is_empty())
                    else {
                        continue;
                    };
                    vector_rows.push(temper_runtime::persistence::EntityVectorRow {
                        decl_name: decl.name.clone(),
                        model_tag: model_tag.to_string(),
                        vector,
                    });
                }
            }
            (key_rows, vector_rows, reconcile_vectors)
        };
        let append_start = Instant::now();
        let result = store
            .append_with_index_rows(
                persistence_id,
                state.sequence_nr,
                &[envelope],
                &key_rows,
                &vector_rows,
                reconcile_vectors,
            )
            .await;
        crate::runtime_metrics::record_event_store_append_wait(
            backend.as_str(),
            "append",
            append_start.elapsed(),
        );
        match result {
            Ok(new_seq) => {
                state.sequence_nr = new_seq;
                tracing::debug!(entity = %state.entity_id, seq = new_seq, "event persisted");
                assert_eq!(
                    new_seq, event_version,
                    "single-event append must return the persisted clock version"
                );
                Ok((new_seq, state_timeout_clock))
            }
            Err(e) => {
                tracing::error!(
                    entity = %state.entity_id, error = %e,
                    "failed to persist event — state advanced but not durable"
                );
                Err(e)
            }
        }
    }

    /// Save a snapshot when the configured interval is reached.
    pub(super) async fn maybe_save_snapshot(
        store: &BoxedEventStore,
        snapshot_queue: Option<&Arc<SnapshotWriteQueue>>,
        persistence_id: &str,
        state: &mut EntityState,
    ) -> Result<(), PersistenceError> {
        if state.sequence_nr == 0 {
            return Ok(());
        }
        if let Some(queue) = snapshot_queue
            && let Some(applied_sequence) = queue.applied_sequence(persistence_id)
            && applied_sequence > state.last_snapshot_sequence_nr
        {
            let applied_sequence = applied_sequence.min(state.sequence_nr);
            state.last_snapshot_sequence_nr = applied_sequence;
            state.events_since_snapshot =
                state.sequence_nr.saturating_sub(applied_sequence) as usize;
        }

        let interval = Self::snapshot_interval();
        let pending_sequence = snapshot_queue
            .and_then(|queue| queue.pending_sequence(persistence_id))
            .unwrap_or(0);
        let latest_snapshot_boundary = state.last_snapshot_sequence_nr.max(pending_sequence);
        if state.sequence_nr.saturating_sub(latest_snapshot_boundary) < interval {
            return Ok(());
        }

        let snapshot = Self::serialize_snapshot_state(state)?;
        if let Some(queue) = snapshot_queue {
            match queue.enqueue(persistence_id.to_string(), state.sequence_nr, snapshot) {
                SnapshotEnqueueOutcome::Enqueued
                | SnapshotEnqueueOutcome::Coalesced
                | SnapshotEnqueueOutcome::StaleSkipped => return Ok(()),
                SnapshotEnqueueOutcome::Full => {
                    tracing::warn!(
                        entity = %state.entity_id,
                        seq = state.sequence_nr,
                        "snapshot write queue full; keeping replay tail open"
                    );
                    return Ok(());
                }
            }
        }

        store
            .save_snapshot(persistence_id, state.sequence_nr, &snapshot)
            .await?;
        state.last_snapshot_sequence_nr = state.sequence_nr;
        state.events_since_snapshot = 0;
        Ok(())
    }
}
