//! Persist events, snapshots, and journal replay for [`EntityActor`].

use std::sync::Arc;
use std::time::Instant;

use temper_jit::table::TransitionTable;
use temper_runtime::actor::ActorError;
use temper_runtime::persistence::{
    COMPOSITE_EVENT_TYPE, EventMetadata, PersistenceEnvelope, PersistenceError,
};
use temper_runtime::scheduler::sim_uuid;

use crate::entity_actor::effects::FieldSyncMode;
use crate::entity_actor::snapshot_queue::{SnapshotEnqueueOutcome, SnapshotWriteQueue};
use crate::entity_actor::types::{EntityEvent, EntityState, MAX_EVENTS_SINCE_SNAPSHOT};
use crate::storage::{BackendLabel, BoxedEventStore};

use super::EntityActor;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReplayPolicy {
    LenientSnapshot,
    StrictSnapshot,
    StrictFullJournal,
}

impl ReplayPolicy {
    fn loads_snapshot(self) -> bool {
        self != Self::StrictFullJournal
    }

    fn strict_journal_read(self) -> bool {
        self != Self::LenientSnapshot
    }

    fn strict_event_validation(self) -> bool {
        self == Self::StrictFullJournal
    }
}

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
        state: &mut EntityState,
        event: &EntityEvent,
    ) -> Result<u64, PersistenceError> {
        let payload = serde_json::to_value(event)
            .map_err(|e| PersistenceError::Serialization(e.to_string()))?;
        let envelope = PersistenceEnvelope {
            sequence_nr: state.sequence_nr + 1,
            event_type: event.action.clone(),
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
            let table = self.table.read().expect("table lock poisoned");
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
        let append_start = Instant::now(); // determinism-ok: production-only event-store wait metric
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

    /// Replay events from the configured store to rebuild state (called in pre_start).
    ///
    /// Re-evaluates each event through the `TransitionTable` to reconstruct
    /// all state variables (status, counters, booleans). This is option 2 from
    /// the replay design: the TransitionTable is the authoritative source of
    /// effects, so replay produces the same state as the original execution.
    pub(super) async fn replay_events(
        table: &TransitionTable,
        store: &BoxedEventStore,
        backend: BackendLabel,
        state: &mut EntityState,
        tenant: &str,
        blob_store: Option<&crate::blob_store::BlobStore>,
        replay_policy: ReplayPolicy,
    ) -> Result<(), ActorError> {
        let replay_start = Instant::now(); // determinism-ok: wall-clock for production replay duration metric only
        let persistence_id = format!("{tenant}:{}:{}", state.entity_type, state.entity_id);
        let persistence_id = persistence_id.as_str();
        let mut from_sequence = 0;
        let mut loaded_snapshot = false;

        if replay_policy.loads_snapshot() {
            match store.load_snapshot(persistence_id).await {
                Ok(Some((snapshot_seq, snapshot_bytes))) => {
                    if Self::apply_snapshot_bytes(state, snapshot_seq, &snapshot_bytes) {
                        from_sequence = snapshot_seq;
                        loaded_snapshot = true;
                        tracing::info!(
                            entity = %state.entity_id,
                            seq = snapshot_seq,
                            "loaded snapshot before replay"
                        );
                    } else {
                        tracing::warn!(
                            entity = %state.entity_id,
                            seq = snapshot_seq,
                            "failed to deserialize snapshot, falling back to full replay"
                        );
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(
                        entity = %state.entity_id,
                        error = %e,
                        "failed to load snapshot, falling back to full replay"
                    );
                }
            }
        }

        match store.read_events(persistence_id, from_sequence).await {
            Ok(envelopes) => {
                if envelopes.len() > MAX_EVENTS_SINCE_SNAPSHOT {
                    return Err(ActorError::custom(format!(
                        "snapshot tail replay budget exceeded for {}:{} ({} > {} events since snapshot)",
                        state.entity_type,
                        state.entity_id,
                        envelopes.len(),
                        MAX_EVENTS_SINCE_SNAPSHOT
                    )));
                }
                let mut expected_sequence = from_sequence.saturating_add(1);
                for (index, env) in envelopes.iter().enumerate() {
                    if replay_policy.strict_event_validation() {
                        if env.sequence_nr != expected_sequence {
                            return Err(ActorError::custom(format!(
                                "non-contiguous journal for {}:{}: expected sequence {}, found {}",
                                state.entity_type,
                                state.entity_id,
                                expected_sequence,
                                env.sequence_nr
                            )));
                        }
                        expected_sequence = env.sequence_nr.checked_add(1).ok_or_else(|| {
                            ActorError::custom(format!(
                                "journal sequence overflow for {}:{}",
                                state.entity_type, state.entity_id
                            ))
                        })?;
                        if env.metadata.actor_id != persistence_id {
                            return Err(ActorError::custom(format!(
                                "journal event for {}:{} at sequence {} is bound to actor '{}'",
                                state.entity_type,
                                state.entity_id,
                                env.sequence_nr,
                                env.metadata.actor_id
                            )));
                        }
                    }

                    if env.event_type == COMPOSITE_EVENT_TYPE {
                        if replay_policy.strict_event_validation() {
                            crate::entity_actor::replay_validation::validate_strict_composite_event(
                                tenant, state, env,
                            )?;
                        }
                        state.sequence_nr = env.sequence_nr;
                        continue;
                    }

                    let parsed_event = serde_json::from_value::<EntityEvent>(env.payload.clone());

                    // Tombstone is terminal: once deleted, entity must not replay
                    // into a live state. Stop at the first Deleted event.
                    if env.event_type == "Deleted" {
                        let tombstone = match parsed_event {
                            Ok(mut event) => {
                                if replay_policy.strict_event_validation() {
                                    crate::entity_actor::replay_validation::validate_strict_entity_event(
                                        table, state, env, &event,
                                    )?;
                                }
                                event.params =
                                    crate::entity_actor::effects::sanitize_action_params(
                                        &event.params,
                                    )
                                    .into_owned();
                                event
                            }
                            Err(error) if replay_policy.strict_event_validation() => {
                                return Err(ActorError::custom(format!(
                                    "invalid tombstone event for {}:{} at sequence {}: {error}",
                                    state.entity_type, state.entity_id, env.sequence_nr
                                )));
                            }
                            Err(_) => EntityEvent {
                                action: "Deleted".to_string(),
                                from_status: state.status.clone(),
                                to_status: "Deleted".to_string(),
                                timestamp: env.metadata.timestamp,
                                params: serde_json::json!({}),
                                idempotency_key: None,
                            },
                        };
                        state.status = tombstone.to_status.clone();
                        if let Some(obj) = state.fields.as_object_mut() {
                            obj.insert(
                                "Status".to_string(),
                                serde_json::Value::String(state.status.clone()),
                            );
                        }
                        state.push_event_bounded(tombstone);
                        state.sequence_nr = env.sequence_nr;
                        if replay_policy.strict_event_validation() && index + 1 != envelopes.len() {
                            return Err(ActorError::custom(format!(
                                "journal for {}:{} contains events after terminal tombstone at sequence {}",
                                state.entity_type, state.entity_id, env.sequence_nr
                            )));
                        }
                        break;
                    }

                    match parsed_event {
                        Ok(mut event) => {
                            if replay_policy.strict_event_validation() {
                                crate::entity_actor::replay_validation::validate_strict_entity_event(
                                    table, state, env, &event,
                                )?;
                            }
                            event.params =
                                crate::entity_actor::effects::sanitize_action_params(&event.params)
                                    .into_owned();
                            // A persisted event is a historical fact: its guard
                            // already passed at commit time and its `to_status`
                            // is authoritative. Replay therefore re-derives the
                            // transition's EFFECTS from the table but never
                            // re-gates it — guards (especially cross-entity ones,
                            // whose related-entity context is not reconstructed
                            // here) must not silently drop committed history.
                            // `replay_effects` returns the matching rule's
                            // effects ignoring guards; `None` means the table no
                            // longer knows this action/from-state, in which case
                            // the stored `to_status` alone carries the state.
                            let from_status = event.from_status.clone();
                            if let Some(effects) =
                                table.replay_effects(&state.status, &event.action)
                            {
                                let effects = effects.to_vec();
                                // Shared effect application — same code as handle() and simulation.
                                let (
                                    _custom_effects,
                                    _scheduled_actions,
                                    _spawn_requests,
                                    _schedule_at_requests,
                                ) = crate::entity_actor::effects::apply_effects(
                                    state,
                                    &effects,
                                    &event.params,
                                );
                            }
                            // Always honor the durably-stored target status. This
                            // is safe (status is always persisted on the event)
                            // and is the single source of truth for the post-
                            // transition state across both the known-action and
                            // unknown-action cases.
                            crate::entity_actor::effects::apply_new_state_fallback(
                                state,
                                &from_status,
                                &event.to_status,
                            );

                            // Sync action params into fields — mirrors the live
                            // process_action() path (effects.rs:155) so data fields
                            // like Title, Description, Priority survive replay.
                            let field_sync_mode =
                                Self::field_sync_mode_for_backend(Some(backend), blob_store);
                            let overflow_blobs =
                                crate::entity_actor::effects::sync_fields_with_metadata(
                                    state,
                                    &event.params,
                                    field_sync_mode,
                                    Some(&table.state_var_metadata),
                                );
                            // Persist replayed overflow blobs so blob-ref envelopes
                            // resolve on subsequent OData reads. Content-addressed
                            // dedup makes this idempotent — if the original live
                            // action already persisted the blob, INSERT OR IGNORE
                            // is a no-op. If the prior server died between emitting
                            // the event and persisting the blob, this is the
                            // recovery path. See ADR-0040, ADR-0045.
                            if !overflow_blobs.is_empty()
                                && let Err(e) =
                                    Self::persist_overflow_blobs(blob_store, &overflow_blobs).await
                            {
                                tracing::warn!(
                                    entity = %state.entity_id,
                                    error = %e,
                                    overflow_count = overflow_blobs.len(),
                                    "failed to persist replayed overflow blobs — blob-ref envelopes may dangle"
                                );
                            }

                            state.push_event_bounded(event);
                        }
                        Err(e) => {
                            if replay_policy.strict_event_validation() {
                                return Err(ActorError::custom(format!(
                                    "invalid event for {}:{} at sequence {}: {e}",
                                    state.entity_type, state.entity_id, env.sequence_nr
                                )));
                            }
                            // Schema-mismatched event: log and skip rather than panic.
                            // This preserves entity hydration across spec evolution —
                            // the last valid state is used and replay continues.
                            tracing::warn!(
                                entity = %state.entity_id,
                                event_id = %env.metadata.event_id,
                                sequence_nr = env.sequence_nr,
                                event_type = %env.event_type,
                                error = %e,
                                "skipping event with incompatible schema during replay"
                            );
                            tracing::warn!(tenant = %tenant, entity_type = %state.entity_type, "event replay error");
                        }
                    }
                    state.sequence_nr = env.sequence_nr;
                }
                if !envelopes.is_empty() {
                    let replayed_tail = state
                        .sequence_nr
                        .saturating_sub(state.last_snapshot_sequence_nr)
                        as usize;
                    if replayed_tail > MAX_EVENTS_SINCE_SNAPSHOT {
                        tracing::error!(
                            entity = %state.entity_id,
                            replayed_tail,
                            cap = MAX_EVENTS_SINCE_SNAPSHOT,
                            "snapshot tail exceeds bounded replay cap"
                        );
                    }
                    tracing::info!(
                        entity = %state.entity_id,
                        snapshot_loaded = loaded_snapshot,
                        replayed = envelopes.len(),
                        status = %state.status,
                        seq = state.sequence_nr,
                        total_events = state.total_event_count,
                        events_since_snapshot = state.events_since_snapshot,
                        recent_events = state.events.len(),
                        counters = ?state.counters,
                        booleans = ?state.booleans,
                        "state rebuilt from event journal via TransitionTable"
                    );
                } else if loaded_snapshot {
                    tracing::info!(
                        entity = %state.entity_id,
                        seq = state.sequence_nr,
                        total_events = state.total_event_count,
                        events_since_snapshot = state.events_since_snapshot,
                        "state restored from snapshot (no delta events)"
                    );
                }
            }
            Err(e) => {
                if replay_policy.strict_journal_read() {
                    return Err(ActorError::custom(format!(
                        "failed to read events for replay of {}:{}: {e}",
                        state.entity_type, state.entity_id
                    )));
                }
                tracing::error!(
                    entity = %state.entity_id, error = %e,
                    "failed to read events for replay — starting fresh"
                );
            }
        }
        crate::runtime_metrics::record_event_replay_duration(
            replay_start.elapsed(),
            tenant,
            &state.entity_type,
        );
        Ok(())
    }
}

/// Rebuild an entity's current state from its snapshot + event tail.
///
/// `strict_journal_read`: when true, a journal read failure PROPAGATES as an error
/// instead of being swallowed into a "start fresh"/stale state. The key-index backfill
/// passes `true` so it can tell "no events" apart from "could not read the journal" —
/// keying decisions and the per-type watermark depend on that distinction (ADR-0153
/// soundness gate). Actor hydration passes `false` (keep serving on a transient read).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn recover_entity_state_from_store(
    tenant: &str,
    entity_type: &str,
    entity_id: &str,
    table: &TransitionTable,
    store: &BoxedEventStore,
    backend: BackendLabel,
    initial_fields: &serde_json::Value,
    blob_store: Option<&crate::blob_store::BlobStore>,
    strict_journal_read: bool,
) -> Result<EntityState, ActorError> {
    let mut state = EntityActor::build_initial_state(entity_type, entity_id, table, initial_fields);
    EntityActor::replay_events(
        table,
        store,
        backend,
        &mut state,
        tenant,
        blob_store,
        if strict_journal_read {
            ReplayPolicy::StrictSnapshot
        } else {
            ReplayPolicy::LenientSnapshot
        },
    )
    .await?;
    Ok(state)
}

/// Rebuild security-sensitive state from the complete durable journal.
///
/// This intentionally ignores snapshots and fails closed on read errors,
/// sequence gaps, incompatible events, or history after a terminal tombstone.
/// Identity resolution uses this path so a stale or corrupt snapshot cannot
/// preserve revoked authority.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn recover_authoritative_entity_state_from_store(
    tenant: &str,
    entity_type: &str,
    entity_id: &str,
    table: &TransitionTable,
    store: &BoxedEventStore,
    backend: BackendLabel,
    initial_fields: &serde_json::Value,
    blob_store: Option<&crate::blob_store::BlobStore>,
) -> Result<EntityState, ActorError> {
    let mut state = EntityActor::build_initial_state(entity_type, entity_id, table, initial_fields);
    EntityActor::replay_events(
        table,
        store,
        backend,
        &mut state,
        tenant,
        blob_store,
        ReplayPolicy::StrictFullJournal,
    )
    .await?;
    Ok(state)
}
