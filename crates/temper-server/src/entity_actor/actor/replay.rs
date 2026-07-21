//! Snapshot recovery and strict journal replay.

use super::*;

impl EntityActor {
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
        // When true, a journal read failure PROPAGATES as an error instead of being
        // swallowed ("start fresh"). Actor hydration and concurrency recovery
        // require strict reads because exposing snapshot state without proving the
        // journal tail was replayed can lose a committed transition. Background
        // diagnostics may opt into lenient reads when they never publish state.
        strict_journal_read: bool,
    ) -> Result<Option<(u64, Vec<u8>)>, ActorError> {
        let replay_start = Instant::now(); // determinism-ok: wall-clock for production replay duration metric only
        let persistence_id = format!("{tenant}:{}:{}", state.entity_type, state.entity_id);
        let persistence_id = persistence_id.as_str();
        let mut from_sequence = 0;
        let mut loaded_snapshot = None;
        let mut timeout_clock_authoritative = false;

        match store.load_snapshot(persistence_id).await {
            Ok(Some((snapshot_seq, snapshot_bytes))) => {
                if let Some(snapshot_clock_authoritative) =
                    Self::apply_snapshot_bytes(state, snapshot_seq, &snapshot_bytes)
                {
                    from_sequence = snapshot_seq;
                    timeout_clock_authoritative = snapshot_clock_authoritative;
                    loaded_snapshot = Some((snapshot_seq, snapshot_bytes));
                    tracing::info!(
                        entity = %state.entity_id,
                        seq = snapshot_seq,
                        "loaded snapshot before replay"
                    );
                } else {
                    if strict_journal_read {
                        return Err(ActorError::custom(format!(
                            "failed to deserialize snapshot at sequence {snapshot_seq} for {persistence_id}"
                        )));
                    }
                    tracing::warn!(
                        entity = %state.entity_id,
                        seq = snapshot_seq,
                        "failed to deserialize snapshot, falling back to full replay"
                    );
                }
            }
            Ok(None) => {}
            Err(e) => {
                if strict_journal_read {
                    return Err(ActorError::custom(format!(
                        "failed to load snapshot for {persistence_id}: {e}"
                    )));
                }
                tracing::warn!(
                    entity = %state.entity_id,
                    error = %e,
                    "failed to load snapshot, falling back to full replay"
                );
            }
        }

        match store
            .read_events_with_head(persistence_id, from_sequence)
            .await
        {
            Ok(read) => {
                Self::validate_journal_read(persistence_id, from_sequence, &read)?;
                Self::validate_snapshot_timeout_clock_against_journal_head(
                    persistence_id,
                    state,
                    read.journal_head_sequence_nr,
                    timeout_clock_authoritative,
                )?;
                let envelopes = read.events;
                if envelopes.len() > MAX_EVENTS_SINCE_SNAPSHOT {
                    return Err(ActorError::custom(format!(
                        "snapshot tail replay budget exceeded for {}:{} ({} > {} events since snapshot)",
                        state.entity_type,
                        state.entity_id,
                        envelopes.len(),
                        MAX_EVENTS_SINCE_SNAPSHOT
                    )));
                }
                for env in &envelopes {
                    if env.event_type == COMPOSITE_EVENT_TYPE {
                        state.sequence_nr = env.sequence_nr;
                        continue;
                    }
                    // Make the current committed envelope identity visible to
                    // timeout-clock and idempotency metadata before applying
                    // the event. Snapshot lifetime counts can legitimately lag
                    // journal sequence numbers (for example after composite
                    // markers), so the previous sequence is not a safe version.
                    state.sequence_nr = env.sequence_nr;

                    let persisted_clock =
                        decode_entity_event_clock(persistence_id, env.sequence_nr, &env.payload)
                            .map_err(ActorError::custom)?;
                    if timeout_clock_authoritative && persisted_clock.is_none() {
                        tracing::warn!(
                            entity = %state.entity_id,
                            sequence_nr = env.sequence_nr,
                            "legacy state-timeout clock payload follows an authoritative checkpoint; deriving this rollout boundary under the current table"
                        );
                    }
                    let parsed_event = serde_json::from_value::<EntityEvent>(env.payload.clone());

                    // Tombstone is terminal: once deleted, entity must not replay
                    // into a live state. Stop at the first Deleted event.
                    if is_entity_tombstone(&env.event_type, &env.payload) {
                        let tombstone = match parsed_event {
                            Ok(event) => event,
                            Err(_error) if persisted_clock.is_none() => EntityEvent {
                                action: "Deleted".to_string(),
                                from_status: state.status.clone(),
                                to_status: "Deleted".to_string(),
                                timestamp: env.metadata.timestamp,
                                params: serde_json::json!({}),
                                idempotency_key: None,
                            },
                            Err(error) => {
                                return Err(ActorError::custom(format!(
                                    "cannot replay current tombstone at sequence {} for \
                                     {persistence_id}: {error}",
                                    env.sequence_nr
                                )));
                            }
                        };
                        state.status = tombstone.to_status.clone();
                        if let Some(obj) = state.fields.as_object_mut() {
                            obj.insert(
                                "Status".to_string(),
                                serde_json::Value::String(state.status.clone()),
                            );
                        }
                        if let Some(clock) = persisted_clock {
                            let _terminal_clock_authoritative = apply_replayed_state_timeout_clock(
                                persistence_id,
                                table,
                                state,
                                &tombstone,
                                env.sequence_nr,
                                clock,
                                timeout_clock_authoritative,
                            )
                            .map_err(ActorError::custom)?;
                        } else {
                            apply_legacy_state_timeout_clock(
                                table,
                                state,
                                &tombstone,
                                env.sequence_nr,
                            );
                        }
                        state.push_event_bounded(tombstone);
                        break;
                    }

                    match parsed_event {
                        Ok(event) => {
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

                            if let Some(clock) = persisted_clock {
                                timeout_clock_authoritative = apply_replayed_state_timeout_clock(
                                    persistence_id,
                                    table,
                                    state,
                                    &event,
                                    env.sequence_nr,
                                    clock,
                                    timeout_clock_authoritative,
                                )
                                .map_err(ActorError::custom)?;
                            } else {
                                // Legacy event payloads did not record their
                                // table-at-commit timeout interpretation.
                                apply_legacy_state_timeout_clock(
                                    table,
                                    state,
                                    &event,
                                    env.sequence_nr,
                                );
                                timeout_clock_authoritative = false;
                            }
                            state.push_event_bounded(event);
                        }
                        Err(e) => {
                            if persisted_clock.is_some()
                                || (strict_journal_read && !table.state_timeouts.is_empty())
                            {
                                return Err(ActorError::custom(format!(
                                    "cannot safely replay incompatible event at sequence {} for current or timeout-enabled {persistence_id}: {e}",
                                    env.sequence_nr
                                )));
                            }
                            // Schema-mismatched event: log and skip rather than panic.
                            // Journal integrity was already proven over the raw
                            // contiguous envelopes above. A timeout-free schema can
                            // preserve its established evolution behavior by keeping
                            // the last valid domain state while still consuming this
                            // durable sequence number so later appends cannot collide.
                            // Timeout-enabled schemas fail above because an unknown
                            // event may have entered or exited a timed state.
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
                    state.events_since_snapshot = replayed_tail;
                    tracing::info!(
                        entity = %state.entity_id,
                        snapshot_loaded = loaded_snapshot.is_some(),
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
                } else if loaded_snapshot.is_some() {
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
                if strict_journal_read {
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
        Ok(loaded_snapshot)
    }
}

/// Rebuild an entity's current state from its snapshot + event tail.
///
/// `strict_journal_read`: when true, a journal read failure propagates as an error
/// instead of being swallowed into a "start fresh"/stale state. Actor hydration,
/// concurrency recovery, and key-index backfill require strict reads because each
/// publishes a decision based on the recovered state. Background diagnostics may
/// remain lenient when a partial replay is only reported as an observation.
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
    let _loaded_snapshot = EntityActor::replay_events(
        table,
        store,
        backend,
        &mut state,
        tenant,
        blob_store,
        strict_journal_read,
    )
    .await?;
    Ok(state)
}
