//! Bounded entity recovery from snapshots and durable journals.

use super::*;

mod stable_source;

pub(crate) use stable_source::{
    CapturedEntitySnapshot, StableEntitySource, recover_entity_state_from_stable_sources,
    stable_entity_source_is_current,
};

const JOURNAL_REPLAY_PAGE_SIZE: usize = 1_024;

#[derive(Clone, Copy)]
struct ReplayPolicy {
    strict_journal_read: bool,
    load_snapshot: bool,
    strict_event_decode: bool,
    replay_full_journal: bool,
}

async fn apply_replayed_envelope(
    table: &TransitionTable,
    backend: BackendLabel,
    state: &mut EntityState,
    tenant: &str,
    blob_store: Option<&crate::blob_store::BlobStore>,
    envelope: &PersistenceEnvelope,
    strict_event_decode: bool,
) -> Result<(), ActorError> {
    // Deleted is terminal, but every later legacy/corrupt envelope still consumes
    // its durable sequence so the reconstructed state reaches the captured fence.
    if state.status == "Deleted" {
        state.sequence_nr = envelope.sequence_nr;
        return Ok(());
    }
    // `event_type` is normally the domain action name, so CompositeEvent is not
    // a reserved discriminator. Skip only the runtime audit schema; a domain
    // action with the same name must continue through ordinary replay, while an
    // undecodable payload must still fail strict authoritative recovery.
    if envelope.event_type == COMPOSITE_EVENT_TYPE
        && serde_json::from_value::<CompositeEvent>(envelope.payload.clone()).is_ok()
    {
        state.sequence_nr = envelope.sequence_nr;
        return Ok(());
    }

    if envelope.event_type == FIELD_UPDATE_EVENT_TYPE
        && let Ok(update) = serde_json::from_value::<PersistedFieldUpdate>(envelope.payload.clone())
        && update.schema == FIELD_UPDATE_SCHEMA
    {
        let status = state.status.clone();
        EntityActor::apply_field_update(state, &update.fields, update.replace).map_err(
            |error| {
                ActorError::custom(format!(
                    "failed to replay {} for {}:{}: {error}",
                    envelope.event_type, state.entity_type, state.entity_id
                ))
            },
        )?;
        state.push_event_bounded(EntityEvent {
            action: if update.replace {
                FIELDS_REPLACED_EVENT_TYPE.to_string()
            } else {
                FIELDS_PATCHED_EVENT_TYPE.to_string()
            },
            from_status: status.clone(),
            to_status: status,
            timestamp: envelope.metadata.timestamp,
            params: update.fields,
            idempotency_key: update.idempotency_key,
        });
        state.sequence_nr = envelope.sequence_nr;
        return Ok(());
    }

    let parsed_event = serde_json::from_value::<EntityEvent>(envelope.payload.clone());
    if envelope.transitions_to_deleted() {
        let tombstone = parsed_event.unwrap_or_else(|_| EntityEvent {
            action: "Deleted".to_string(),
            from_status: state.status.clone(),
            to_status: "Deleted".to_string(),
            timestamp: envelope.metadata.timestamp,
            params: serde_json::json!({}),
            idempotency_key: None,
        });
        state.status = tombstone.to_status.clone();
        if let Some(fields) = state.fields.as_object_mut() {
            fields.insert(
                "Status".to_string(),
                serde_json::Value::String(state.status.clone()),
            );
        }
        state.push_event_bounded(tombstone);
        state.sequence_nr = envelope.sequence_nr;
        return Ok(());
    }

    match parsed_event {
        Ok(event) => {
            // The guard already passed at commit time. Replay the historical
            // effects without re-gating, then honor the stored target state.
            let from_status = event.from_status.clone();
            if let Some(effects) = table.replay_effects(&state.status, &event.action) {
                let effects = effects.to_vec();
                let (_custom_effects, _scheduled_actions, _spawn_requests, _schedule_at_requests) =
                    crate::entity_actor::effects::apply_effects(state, &effects, &event.params);
            }
            crate::entity_actor::effects::apply_new_state_fallback(
                state,
                &from_status,
                &event.to_status,
            );

            let field_sync_mode =
                EntityActor::field_sync_mode_for_backend(Some(backend), blob_store);
            let overflow_blobs = crate::entity_actor::effects::sync_fields_with_metadata(
                state,
                &event.params,
                field_sync_mode,
                Some(&table.state_var_metadata),
            );
            if !overflow_blobs.is_empty()
                && let Err(error) =
                    EntityActor::persist_overflow_blobs(blob_store, &overflow_blobs).await
            {
                tracing::warn!(
                    entity = %state.entity_id,
                    error = %error,
                    overflow_count = overflow_blobs.len(),
                    "failed to persist replayed overflow blobs — blob-ref envelopes may dangle"
                );
            }
            state.push_event_bounded(event);
        }
        Err(error) if strict_event_decode => {
            return Err(ActorError::custom(format!(
                "incompatible durable event schema for {}:{} at sequence {} ({}): {error}",
                state.entity_type, state.entity_id, envelope.sequence_nr, envelope.event_type
            )));
        }
        Err(error) => {
            tracing::warn!(
                entity = %state.entity_id,
                event_id = %envelope.metadata.event_id,
                sequence_nr = envelope.sequence_nr,
                event_type = %envelope.event_type,
                error = %error,
                "skipping event with incompatible schema during replay"
            );
            tracing::warn!(
                tenant = %tenant,
                entity_type = %state.entity_type,
                "event replay error"
            );
        }
    }
    state.sequence_nr = envelope.sequence_nr;
    Ok(())
}

async fn replay_events(
    table: &TransitionTable,
    store: &BoxedEventStore,
    backend: BackendLabel,
    state: &mut EntityState,
    tenant: &str,
    blob_store: Option<&crate::blob_store::BlobStore>,
    policy: ReplayPolicy,
    captured_boundary: Option<JournalBoundary>,
) -> Result<(), ActorError> {
    let replay_start = Instant::now(); // determinism-ok: production replay metric only
    let persistence_id = format!("{tenant}:{}:{}", state.entity_type, state.entity_id);
    let initial_state = state.clone();
    let mut from_sequence = 0;
    let mut loaded_snapshot = false;
    let journal_boundary = match captured_boundary {
        Some(boundary) => boundary,
        None => store
            .journal_boundary(&persistence_id)
            .await
            .map_err(|error| {
                ActorError::custom(format!(
                    "failed to read durable journal boundary for {}:{}: {error}",
                    state.entity_type, state.entity_id
                ))
            })?,
    };

    if policy.load_snapshot {
        match store.load_snapshot(&persistence_id).await {
            Ok(Some((snapshot_sequence, snapshot_bytes))) => {
                if EntityActor::apply_snapshot_bytes(state, snapshot_sequence, &snapshot_bytes) {
                    from_sequence = snapshot_sequence;
                    loaded_snapshot = true;
                    tracing::info!(
                        entity = %state.entity_id,
                        seq = snapshot_sequence,
                        "loaded snapshot before replay"
                    );
                } else {
                    tracing::warn!(
                        entity = %state.entity_id,
                        seq = snapshot_sequence,
                        "failed to deserialize snapshot, falling back to full replay"
                    );
                }
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(
                    entity = %state.entity_id,
                    error = %error,
                    "failed to load snapshot, falling back to full replay"
                );
            }
        }
    }

    if loaded_snapshot
        && state.status != "Deleted"
        && let Some(tombstone_sequence) = journal_boundary.first_terminal_sequence
        && tombstone_sequence <= from_sequence
    {
        tracing::warn!(
            entity = %state.entity_id,
            snapshot_sequence = from_sequence,
            tombstone_sequence,
            "discarding live snapshot newer than terminal journal boundary"
        );
        *state = initial_state;
        from_sequence = 0;
        loaded_snapshot = false;
    }

    let replay_event_budget = journal_boundary
        .latest_sequence
        .saturating_sub(from_sequence);
    if !policy.replay_full_journal && replay_event_budget > MAX_EVENTS_SINCE_SNAPSHOT as u64 {
        return Err(ActorError::custom(format!(
            "snapshot tail replay budget exceeded for {}:{} ({} > {} events since snapshot)",
            state.entity_type, state.entity_id, replay_event_budget, MAX_EVENTS_SINCE_SNAPSHOT
        )));
    }

    if replay_event_budget == 0 && policy.strict_journal_read {
        let probe = store
            .read_events_page(
                &persistence_id,
                journal_boundary.latest_sequence,
                journal_boundary.latest_sequence,
                1,
            )
            .await
            .map_err(|error| {
                ActorError::custom(format!(
                    "failed to verify journal readability for {}:{}: {error}",
                    state.entity_type, state.entity_id
                ))
            })?;
        if !probe.is_empty() {
            return Err(ActorError::custom(format!(
                "journal readability probe for {}:{} crossed durable high-water {}",
                state.entity_type, state.entity_id, journal_boundary.latest_sequence
            )));
        }
    }

    let mut cursor = from_sequence;
    let mut replayed_count = 0_u64;
    while cursor < journal_boundary.latest_sequence {
        let remaining = journal_boundary.latest_sequence - cursor;
        let page_len = usize::try_from(remaining.min(JOURNAL_REPLAY_PAGE_SIZE as u64))
            .expect("bounded journal page length fits usize");
        let page = match store
            .read_events_page(
                &persistence_id,
                cursor,
                journal_boundary.latest_sequence,
                page_len,
            )
            .await
        {
            Ok(page) => page,
            Err(error)
                if policy.strict_journal_read
                    || journal_boundary.latest_sequence > from_sequence =>
            {
                return Err(ActorError::custom(format!(
                    "failed to read events for replay of {}:{}: {error}",
                    state.entity_type, state.entity_id
                )));
            }
            Err(error) => {
                tracing::error!(
                    entity = %state.entity_id,
                    error = %error,
                    "failed to read events for replay — starting fresh"
                );
                break;
            }
        };
        if page.len() != page_len {
            return Err(ActorError::custom(format!(
                "journal replay for {}:{} returned {} events where {} were required through durable high-water {}",
                state.entity_type,
                state.entity_id,
                page.len(),
                page_len,
                journal_boundary.latest_sequence
            )));
        }
        for (offset, envelope) in page.iter().enumerate() {
            let expected_sequence = cursor + offset as u64 + 1;
            if envelope.sequence_nr != expected_sequence {
                return Err(ActorError::custom(format!(
                    "journal replay for {}:{} expected sequence {}, received {}",
                    state.entity_type, state.entity_id, expected_sequence, envelope.sequence_nr
                )));
            }
            apply_replayed_envelope(
                table,
                backend,
                state,
                tenant,
                blob_store,
                envelope,
                policy.strict_event_decode,
            )
            .await?;
        }
        cursor = page
            .last()
            .map(|event| event.sequence_nr)
            .expect("validated non-empty journal page");
        replayed_count = replayed_count.saturating_add(page.len() as u64);
    }

    if cursor < journal_boundary.latest_sequence {
        return Err(ActorError::custom(format!(
            "journal replay for {}:{} stopped at sequence {} below durable high-water {}",
            state.entity_type, state.entity_id, cursor, journal_boundary.latest_sequence
        )));
    }
    if journal_boundary.first_terminal_sequence.is_some()
        && (state.status != "Deleted" || state.sequence_nr < journal_boundary.latest_sequence)
    {
        return Err(ActorError::custom(format!(
            "journal replay for {}:{} did not preserve terminal history through sequence {}",
            state.entity_type, state.entity_id, journal_boundary.latest_sequence
        )));
    }

    if replayed_count > 0 {
        tracing::info!(
            entity = %state.entity_id,
            snapshot_loaded = loaded_snapshot,
            replayed = replayed_count,
            status = %state.status,
            seq = state.sequence_nr,
            total_events = state.total_event_count,
            events_since_snapshot = state.events_since_snapshot,
            recent_events = state.events.len(),
            counters = ?state.counters,
            booleans = ?state.booleans,
            "state rebuilt from bounded event-journal pages via TransitionTable"
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
    crate::runtime_metrics::record_event_replay_duration(
        replay_start.elapsed(),
        tenant,
        &state.entity_type,
    );
    Ok(())
}

/// Rebuild an entity's current state from its snapshot plus bounded journal tail.
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
    replay_events(
        table,
        store,
        backend,
        &mut state,
        tenant,
        blob_store,
        ReplayPolicy {
            strict_journal_read,
            load_snapshot: true,
            strict_event_decode: false,
            replay_full_journal: false,
        },
        None,
    )
    .await?;
    Ok(state)
}

/// Rebuild an entity from bounded journal pages, ignoring every derived snapshot.
///
/// Snapshot-only and first-journal generations can carry the same sequence. Once a
/// journal exists, replay from zero is the only proof of which source owns it.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn recover_entity_state_from_journal_through(
    tenant: &str,
    entity_type: &str,
    entity_id: &str,
    table: &TransitionTable,
    store: &BoxedEventStore,
    backend: BackendLabel,
    initial_fields: &serde_json::Value,
    blob_store: Option<&crate::blob_store::BlobStore>,
    journal_boundary: JournalBoundary,
) -> Result<EntityState, ActorError> {
    let mut state = EntityActor::build_initial_state(entity_type, entity_id, table, initial_fields);
    replay_events(
        table,
        store,
        backend,
        &mut state,
        tenant,
        blob_store,
        ReplayPolicy {
            strict_journal_read: true,
            load_snapshot: false,
            strict_event_decode: true,
            replay_full_journal: true,
        },
        Some(journal_boundary),
    )
    .await?;
    Ok(state)
}
