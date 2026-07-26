//! Entity journal persistence and snapshot materialization.

use super::*;

impl EntityActor {
    /// Serialize actor state for snapshot persistence, excluding recent event history.
    ///
    /// The stored snapshot is already a segment boundary, so its hot tail budget
    /// is reset in the payload. Lifetime sequence/count fields remain intact. A
    /// journal provenance marker is emitted only when the caller supplies the exact
    /// durable journal sequence that produced this state.
    pub(crate) fn serialize_snapshot_state(
        state: &EntityState,
        journal_sequence: Option<u64>,
    ) -> Result<Vec<u8>, PersistenceError> {
        if let Some(sequence) = journal_sequence
            && (sequence == 0 || sequence != state.sequence_nr)
        {
            return Err(PersistenceError::Serialization(format!(
                "snapshot journal provenance {sequence} does not match actor sequence {}",
                state.sequence_nr
            )));
        }
        let mut value = serde_json::to_value(state)
            .map_err(|e| PersistenceError::Serialization(e.to_string()))?;
        if let Some(obj) = value.as_object_mut() {
            obj.remove("events");
            obj.insert("events_since_snapshot".to_string(), serde_json::json!(0));
            obj.insert(
                "last_snapshot_sequence_nr".to_string(),
                serde_json::json!(state.sequence_nr),
            );
            obj.remove(SNAPSHOT_JOURNAL_SEQUENCE_FIELD);
            if let Some(sequence) = journal_sequence {
                obj.insert(
                    SNAPSHOT_JOURNAL_SEQUENCE_FIELD.to_string(),
                    serde_json::json!(sequence),
                );
            }
        }
        serde_json::to_vec(&value).map_err(|e| PersistenceError::Serialization(e.to_string()))
    }

    /// Attempt to load actor state from snapshot payload bytes.
    pub(super) fn apply_snapshot_bytes(
        state: &mut EntityState,
        sequence_nr: u64,
        bytes: &[u8],
    ) -> Option<SnapshotProvenance> {
        let mut value = match serde_json::from_slice::<serde_json::Value>(bytes) {
            Ok(v) => v,
            Err(_) => return None,
        };
        let obj = value.as_object_mut()?;
        // Before the explicit provenance field existed, actor-written snapshots
        // omitted the bounded in-memory `events` tail and retained the exact
        // aggregate sequence. Writers after segmented replay accounting was
        // introduced also stored the matching boundary and a zero tail; older
        // writers omitted both of those later fields. Generic migration
        // snapshots with an `events` field remain provenance-free.
        let segmented_coordinates = match (
            obj.get("last_snapshot_sequence_nr")
                .and_then(serde_json::Value::as_u64),
            obj.get("events_since_snapshot")
                .and_then(serde_json::Value::as_u64),
        ) {
            (Some(last_snapshot_sequence), Some(0)) => last_snapshot_sequence == sequence_nr,
            (None, None) => true,
            _ => false,
        };
        let legacy_journal_sequence = (!obj.contains_key("events")
            && obj.get("sequence_nr").and_then(serde_json::Value::as_u64) == Some(sequence_nr)
            && segmented_coordinates)
            .then_some(sequence_nr);
        let provenance = match obj.remove(SNAPSHOT_JOURNAL_SEQUENCE_FIELD) {
            None => match legacy_journal_sequence {
                Some(through_sequence) => SnapshotProvenance::LegacyJournal { through_sequence },
                None => SnapshotProvenance::Legacy,
            },
            Some(value) if value.as_u64() == Some(sequence_nr) => SnapshotProvenance::Journal {
                through_sequence: sequence_nr,
            },
            Some(_) => return None,
        };

        // Snapshot intentionally excludes in-memory recent history.
        obj.insert("events".to_string(), serde_json::json!([]));
        if !obj.contains_key("total_event_count") {
            obj.insert(
                "total_event_count".to_string(),
                serde_json::json!(sequence_nr as usize),
            );
        }
        obj.insert("events_since_snapshot".to_string(), serde_json::json!(0));
        obj.insert(
            "last_snapshot_sequence_nr".to_string(),
            serde_json::json!(sequence_nr),
        );

        match serde_json::from_value::<EntityState>(value) {
            Ok(mut restored) => {
                restored.sequence_nr = sequence_nr;
                restored.events_since_snapshot = 0;
                restored.last_snapshot_sequence_nr = sequence_nr;
                *state = restored;
                Some(provenance)
            }
            Err(_) => None,
        }
    }

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

    pub(super) fn record_state_key_contract(&self, table: &TransitionTable) {
        *self
            .state_key_contract
            .write()
            .expect("state key contract lock poisoned") =
            crate::key_index::declared_key_write_contract(table);
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
    #[expect(
        clippy::too_many_arguments,
        reason = "persistence boundary carries both pre-event and candidate state"
    )]
    pub(super) async fn persist_event(
        &self,
        store: &BoxedEventStore,
        backend: BackendLabel,
        persistence_id: &str,
        table: &TransitionTable,
        state_before_event: &EntityState,
        state: &mut EntityState,
        event: &EntityEvent,
        post_dispatch_effects: Option<PersistedPostDispatchEffects>,
    ) -> Result<u64, PersistenceError> {
        let payload = serde_json::to_value(event)
            .map_err(|e| PersistenceError::Serialization(e.to_string()))?;
        self.persist_payload(
            store,
            backend,
            persistence_id,
            table,
            state_before_event,
            state,
            PersistencePayload {
                event_type: &event.action,
                payload,
                timestamp: event.timestamp,
                to_status: &event.to_status,
                post_dispatch_effects,
            },
        )
        .await
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "persistence boundary carries both pre-event and candidate state"
    )]
    pub(super) async fn persist_payload(
        &self,
        store: &BoxedEventStore,
        backend: BackendLabel,
        persistence_id: &str,
        table: &TransitionTable,
        state_before_event: &EntityState,
        state: &mut EntityState,
        input: PersistencePayload<'_>,
    ) -> Result<u64, PersistenceError> {
        let PersistencePayload {
            event_type,
            payload,
            timestamp,
            to_status,
            post_dispatch_effects,
        } = input;
        let envelope = PersistenceEnvelope {
            sequence_nr: state.sequence_nr + 1,
            event_type: event_type.to_string(),
            payload,
            metadata: EventMetadata {
                event_id: sim_uuid(),
                causation_id: sim_uuid(),
                correlation_id: sim_uuid(),
                timestamp,
                actor_id: persistence_id.to_string(),
            },
        };
        let snapshot_source = self
            .snapshot_source
            .read()
            .expect("snapshot source lock poisoned")
            .clone();
        let materializes_snapshot =
            state.sequence_nr == 0 && matches!(&snapshot_source, SnapshotSourceFence::Exact { .. });
        let has_post_dispatch_effects = post_dispatch_effects.is_some();
        let mut envelopes = Vec::with_capacity(3);
        if materializes_snapshot {
            envelopes.push(state_materialization_envelope(
                persistence_id,
                state_before_event,
                timestamp,
            )?);
        }
        envelopes.push(envelope);
        if let Some(mut post_dispatch_effects) = post_dispatch_effects {
            let source_sequence = state
                .sequence_nr
                .checked_add(envelopes.len() as u64)
                .and_then(|sequence| sequence.checked_add(1))
                .ok_or_else(|| {
                    PersistenceError::Storage(format!(
                        "post-dispatch effect sequence overflow for {persistence_id}"
                    ))
                })?;
            post_dispatch_effects.source_sequence = source_sequence;
            let payload = serde_json::to_value(post_dispatch_effects)
                .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
            envelopes.push(PersistenceEnvelope {
                sequence_nr: source_sequence,
                event_type: POST_DISPATCH_EFFECTS_EVENT_TYPE.to_string(),
                payload,
                metadata: EventMetadata {
                    event_id: sim_uuid(),
                    causation_id: sim_uuid(),
                    correlation_id: sim_uuid(),
                    timestamp,
                    actor_id: persistence_id.to_string(),
                },
            });
        }

        // W2 / temper#146: measure append wait — the hypothesis is that
        // writer-lock / fsync serialization is a cold-start bottleneck.
        // ADR-0153/0155: derive the declared key rows AND the vector-index rows from
        // the new state and co-commit them with the journal append, so a keyed read
        // is correct without a scan and a kNN read reflects the write deterministically.
        let (key_rows, vector_rows, reconciliation) = {
            // A keyed type owns exact reconciliation even when its current row set
            // is empty (delete, all-null, or otherwise unkeyable). Special Delete
            // persists the tombstone before mutating `state.status`, so the event's
            // target status is the authoritative deletion signal here.
            // Every spec-governed write owns an exact key row set. An empty
            // declaration set must still purge rows left by an older contract.
            let reconcile_keys = true;
            let index_entity = to_status != "Deleted";
            // The type declares vector paths → the store reconciles this entity's
            // vector rows (delete stale + insert current) even when no row is emitted
            // this write (a delete transition or a cleared property), so stale rows are
            // purged instead of being ranked forever (ADR-0155).
            let reconcile_vectors = !table.vectors.is_empty();
            let key_rows =
                crate::key_index::derive_entity_key_rows(&table.keys, &state.fields, index_entity);
            let mut vector_rows = Vec::new();
            if let Some(field_map) = state.fields.as_object() {
                // A soft-deleted (tombstone) entity is never indexed — it emits no
                // vector rows, so the reconcile below PURGES any it had, even though
                // its embedding field may still be present. Mirrors how the field-index
                // projection removes a deleted entity.
                for decl in table.vectors.iter().filter(|_| index_entity) {
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
            (
                key_rows,
                vector_rows,
                IndexReconciliation {
                    keys: reconcile_keys,
                    key_set_signature: Some(crate::key_index::declared_key_write_contract(table)),
                    vectors: reconcile_vectors,
                    snapshot_source,
                },
            )
        };
        let append_start = Instant::now();
        let result = store
            .append_with_index_rows(
                persistence_id,
                state.sequence_nr,
                &envelopes,
                &key_rows,
                &vector_rows,
                reconciliation,
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
                if materializes_snapshot {
                    state.record_internal_envelope();
                    rebase_materialized_idempotency_keys(state);
                    *self
                        .snapshot_source
                        .write()
                        .expect("snapshot source lock poisoned") = SnapshotSourceFence::Absent;
                }
                if has_post_dispatch_effects {
                    state.record_internal_envelope();
                }
                tracing::debug!(entity = %state.entity_id, seq = new_seq, "event persisted");
                Ok(new_seq)
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
        snapshot_source: &mut SnapshotSourceFence,
        key_contract: Option<&str>,
    ) -> Result<Option<Vec<u8>>, PersistenceError> {
        let applied_sequence = snapshot_queue
            .and_then(|queue| queue.applied_sequence_for_contract(persistence_id, key_contract));
        let current_source_sequence = match snapshot_source {
            SnapshotSourceFence::Exact { sequence_nr, .. } => Some(*sequence_nr),
            SnapshotSourceFence::Absent | SnapshotSourceFence::Unchecked => None,
        };
        let applied_source_is_newer = applied_sequence.is_some_and(|sequence_nr| {
            current_source_sequence.is_none_or(|current| sequence_nr > current)
        });
        if applied_source_is_newer
            && let Some(applied_source) = match snapshot_queue {
                Some(queue) => {
                    queue
                        .applied_source_for_contract(persistence_id, key_contract)
                        .await
                }
                None => None,
            }
        {
            let applied_sequence = match &applied_source {
                SnapshotSourceFence::Exact { sequence_nr, .. } => *sequence_nr,
                SnapshotSourceFence::Absent | SnapshotSourceFence::Unchecked => {
                    unreachable!("an applied snapshot source is always exact")
                }
            };
            if current_source_sequence.is_none_or(|sequence_nr| applied_sequence > sequence_nr) {
                *snapshot_source = applied_source;
            }
        }
        if state.sequence_nr == 0 {
            return Ok(None);
        }
        if matches!(
            snapshot_source,
            SnapshotSourceFence::Exact { sequence_nr, .. } if *sequence_nr > state.sequence_nr
        ) {
            // A migration snapshot may be numerically ahead of the journal.
            // Stores correctly treat the lower save as a monotonic no-op, so do
            // not enqueue it or claim its attempted bytes as the durable source.
            return Ok(None);
        }
        if let Some(applied_sequence) = applied_sequence
            && applied_sequence > state.last_snapshot_sequence_nr
        {
            let applied_sequence = applied_sequence.min(state.sequence_nr);
            state.last_snapshot_sequence_nr = applied_sequence;
            state.events_since_snapshot =
                state.sequence_nr.saturating_sub(applied_sequence) as usize;
        }

        let interval = Self::snapshot_interval();
        let pending_sequence = snapshot_queue
            .and_then(|queue| queue.pending_sequence_for_contract(persistence_id, key_contract))
            .unwrap_or(0);
        let latest_snapshot_boundary = state.last_snapshot_sequence_nr.max(pending_sequence);
        if state.sequence_nr.saturating_sub(latest_snapshot_boundary) < interval {
            return Ok(None);
        }

        let snapshot = Self::serialize_snapshot_state(state, Some(state.sequence_nr))?;
        if let Some(queue) = snapshot_queue {
            match queue.enqueue(
                persistence_id.to_string(),
                state.sequence_nr,
                snapshot,
                snapshot_source.clone(),
                key_contract.map(str::to_string),
            ) {
                SnapshotEnqueueOutcome::Enqueued
                | SnapshotEnqueueOutcome::Coalesced
                | SnapshotEnqueueOutcome::StaleSkipped => return Ok(None),
                SnapshotEnqueueOutcome::Full => {
                    tracing::warn!(
                        entity = %state.entity_id,
                        seq = state.sequence_nr,
                        "snapshot write queue full; keeping replay tail open"
                    );
                    return Ok(None);
                }
            }
        }

        store
            .save_snapshot_if_source(
                persistence_id,
                state.sequence_nr,
                &snapshot,
                snapshot_source,
                key_contract,
            )
            .await?;
        *snapshot_source = SnapshotSourceFence::Exact {
            sequence_nr: state.sequence_nr,
            state: snapshot.clone(),
        };
        state.last_snapshot_sequence_nr = state.sequence_nr;
        state.events_since_snapshot = 0;
        Ok(Some(snapshot))
    }
}
