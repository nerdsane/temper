use super::*;

pub(super) async fn apply_replayed_envelope(
    table: &TransitionTable,
    backend: BackendLabel,
    state: &mut EntityState,
    tenant: &str,
    blob_store: Option<&crate::blob_store::BlobStore>,
    envelope: &PersistenceEnvelope,
    strict_event_decode: bool,
) -> Result<(), ActorError> {
    fn advance_durable_tail(state: &mut EntityState, sequence_nr: u64) {
        state.sequence_nr = sequence_nr;
        state.events_since_snapshot =
            usize::try_from(sequence_nr.saturating_sub(state.last_snapshot_sequence_nr))
                .unwrap_or(usize::MAX);
    }

    if is_state_materialization_event_for(envelope, &state.entity_type, &state.entity_id) {
        let materialization = serde_json::from_value::<PersistedStateMaterialization>(
            envelope.payload.clone(),
        )
        .map_err(|error| {
            ActorError::custom(format!(
                "failed to decode durable state materialization for {}:{} at sequence {}: {error}",
                state.entity_type, state.entity_id, envelope.sequence_nr
            ))
        })?;
        if materialization.schema != STATE_MATERIALIZATION_SCHEMA {
            return Err(ActorError::custom(format!(
                "unsupported durable state materialization schema '{}' for {}:{} at sequence {}",
                materialization.schema, state.entity_type, state.entity_id, envelope.sequence_nr
            )));
        }
        if envelope.sequence_nr != 1 || state.sequence_nr != 0 {
            return Err(ActorError::custom(format!(
                "durable state materialization for {}:{} must be the first journal event, found sequence {}",
                state.entity_type, state.entity_id, envelope.sequence_nr
            )));
        }
        if materialization.state.entity_type != state.entity_type
            || materialization.state.entity_id != state.entity_id
        {
            return Err(ActorError::custom(format!(
                "durable state materialization identity mismatch for {}:{} at sequence {}",
                state.entity_type, state.entity_id, envelope.sequence_nr
            )));
        }
        if !table.states.contains(&materialization.state.status) {
            return Err(ActorError::custom(format!(
                "durable state materialization has invalid status '{}' for {}:{} at sequence {}",
                materialization.state.status,
                state.entity_type,
                state.entity_id,
                envelope.sequence_nr
            )));
        }
        if materialization.state.sequence_nr != 0
            || materialization.state.last_snapshot_sequence_nr != 0
            || materialization.state.events_since_snapshot != 0
            || !materialization.state.events.is_empty()
            || materialization.state.processed_idempotency_keys.len()
                > crate::entity_actor::types::MAX_DURABLE_IDEMPOTENCY_KEYS_PER_ENTITY
        {
            return Err(ActorError::custom(format!(
                "durable state materialization has invalid journal coordinates for {}:{} at sequence {}",
                state.entity_type, state.entity_id, envelope.sequence_nr
            )));
        }
        let materialized_fields = materialization.state.fields.as_object().ok_or_else(|| {
            ActorError::custom(format!(
                "durable state materialization fields are not an object for {}:{} at sequence {}",
                state.entity_type, state.entity_id, envelope.sequence_nr
            ))
        })?;
        if materialized_fields
            .get("Id")
            .and_then(serde_json::Value::as_str)
            != Some(state.entity_id.as_str())
            || materialized_fields
                .get("Status")
                .and_then(serde_json::Value::as_str)
                != Some(materialization.state.status.as_str())
        {
            return Err(ActorError::custom(format!(
                "durable state materialization field identity mismatch for {}:{} at sequence {}",
                state.entity_type, state.entity_id, envelope.sequence_nr
            )));
        }
        *state = materialization.state;
        advance_durable_tail(state, envelope.sequence_nr);
        return Ok(());
    }

    if envelope.event_type == POST_DISPATCH_EFFECTS_EVENT_TYPE {
        let persisted = serde_json::from_value::<PersistedPostDispatchEffects>(
            envelope.payload.clone(),
        )
        .map_err(|error| {
            ActorError::custom(format!(
                "failed to decode durable post-dispatch effects for {}:{} at sequence {}: {error}",
                state.entity_type, state.entity_id, envelope.sequence_nr
            ))
        })?;
        if persisted.schema != POST_DISPATCH_EFFECTS_SCHEMA
            || persisted.idempotency_key.is_empty()
            || persisted.source_sequence != envelope.sequence_nr
            || (persisted.custom_effects.is_empty()
                && persisted.scheduled_actions.is_empty()
                && persisted.spawn_requests.is_empty())
        {
            return Err(ActorError::custom(format!(
                "invalid durable post-dispatch effects for {}:{} at sequence {}",
                state.entity_type, state.entity_id, envelope.sequence_nr
            )));
        }
        state.record_durable_idempotency_key(&persisted.idempotency_key, envelope.sequence_nr);
        advance_durable_tail(state, envelope.sequence_nr);
        return Ok(());
    }

    // Deleted is terminal, but every later legacy/corrupt envelope still consumes
    // its durable sequence so the reconstructed state reaches the captured fence.
    if state.status == "Deleted" {
        advance_durable_tail(state, envelope.sequence_nr);
        return Ok(());
    }
    // `event_type` is normally the domain action name, so CompositeEvent is not
    // a reserved discriminator. Skip only the runtime audit schema; a domain
    // action with the same name must continue through ordinary replay, while an
    // undecodable payload must still fail strict authoritative recovery.
    if envelope.event_type == COMPOSITE_EVENT_TYPE
        && let Ok(composite) = serde_json::from_value::<CompositeEvent>(envelope.payload.clone())
    {
        advance_durable_tail(state, envelope.sequence_nr);
        state.record_durable_idempotency_key(
            &composite.composite_idempotency_key,
            envelope.sequence_nr,
        );
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
        advance_durable_tail(state, envelope.sequence_nr);
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
        advance_durable_tail(state, envelope.sequence_nr);
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
                return Err(ActorError::custom(format!(
                    "field-overflow blob persistence failed during replay for {}:{} at sequence {}: {error}",
                    state.entity_type, state.entity_id, envelope.sequence_nr
                )));
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
    advance_durable_tail(state, envelope.sequence_nr);
    Ok(())
}
