//! Durable HTTP PATCH/PUT application, persistence, and retry.

use super::*;

pub(super) const FIELDS_PATCHED_EVENT_TYPE: &str = "Temper.FieldsPatched";
pub(super) const FIELDS_REPLACED_EVENT_TYPE: &str = "Temper.FieldsReplaced";
pub(super) const FIELD_UPDATE_EVENT_TYPE: &str = "Temper.Internal.FieldUpdate.v1";
pub(super) const FIELD_UPDATE_SCHEMA: &str = "temper.field-update.v1";

/// Private journal payload for HTTP PATCH/PUT writes.
///
/// The event type alone is deliberately insufficient to identify this payload:
/// specs may legally declare an action with the same name. Replay recognizes a
/// field update only when both the internal event type and this schema marker
/// match, otherwise it falls through to normal EntityEvent replay.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub(super) struct PersistedFieldUpdate {
    pub(super) schema: String,
    pub(super) fields: serde_json::Value,
    pub(super) replace: bool,
    #[serde(default)]
    pub(super) idempotency_key: Option<String>,
}

struct FieldUpdatePersistence<'a> {
    fields: &'a serde_json::Value,
    replace: bool,
    idempotency_key: &'a str,
    timestamp: chrono::DateTime<chrono::Utc>,
}

impl EntityActor {
    pub(super) fn apply_field_update(
        state: &mut EntityState,
        fields: &serde_json::Value,
        replace: bool,
    ) -> Result<(), String> {
        let updates = fields
            .as_object()
            .ok_or_else(|| "field update payload must be a JSON object".to_string())?;

        if replace {
            state.fields = serde_json::Value::Object(updates.clone());
        } else {
            let existing = state
                .fields
                .as_object_mut()
                .ok_or_else(|| "entity fields must be a JSON object".to_string())?;
            for (name, value) in updates {
                existing.insert(name.clone(), value.clone());
            }
        }

        let entity_id = state.entity_id.clone();
        let status = state.status.clone();
        let object = state
            .fields
            .as_object_mut()
            .ok_or_else(|| "entity fields must remain a JSON object".to_string())?;
        object.insert("Id".to_string(), serde_json::Value::String(entity_id));
        object.insert("Status".to_string(), serde_json::Value::String(status));
        Ok(())
    }

    /// Persist an HTTP PATCH/PUT field update using a private, versioned payload.
    async fn persist_field_update(
        &self,
        store: &BoxedEventStore,
        backend: BackendLabel,
        state: &mut EntityState,
        update: FieldUpdatePersistence<'_>,
    ) -> Result<u64, PersistenceError> {
        let payload = serde_json::to_value(PersistedFieldUpdate {
            schema: FIELD_UPDATE_SCHEMA.to_string(),
            fields: update.fields.clone(),
            replace: update.replace,
            idempotency_key: Some(update.idempotency_key.to_string()),
        })
        .map_err(|e| PersistenceError::Serialization(e.to_string()))?;
        let to_status = state.status.clone();
        let persistence_id = self.persistence_id();
        self.persist_payload(
            store,
            backend,
            &persistence_id,
            state,
            PersistencePayload {
                event_type: FIELD_UPDATE_EVENT_TYPE,
                payload,
                timestamp: update.timestamp,
                to_status: &to_status,
            },
        )
        .await
    }

    /// Rebuild from a clean initial state after an optimistic-concurrency loss.
    ///
    /// Replaying onto the actor's speculative/pre-race state would apply the
    /// journal twice when no snapshot exists. A fresh rebuild is therefore part
    /// of the retry contract, and journal reads are strict: an actor that cannot
    /// catch up must stop instead of continuing to serve stale state.
    async fn recover_authoritative_state(
        &self,
        store: &BoxedEventStore,
        backend: BackendLabel,
    ) -> Result<EntityState, ActorError> {
        let table = self.table.read().expect("table lock poisoned").clone();
        recover_entity_state_from_store(
            &self.tenant,
            &self.entity_type,
            &self.entity_id,
            &table,
            store,
            backend,
            &self.initial_fields,
            self.blob_store.as_ref(),
            true,
        )
        .await
    }

    pub(super) async fn handle_field_update(
        &self,
        state: &mut EntityState,
        fields: serde_json::Value,
        replace: bool,
        idempotency_key: String,
        ctx: &mut ActorContext<Self>,
    ) -> Result<(), ActorError> {
        const MAX_FIELD_UPDATE_RETRIES: u32 = 2;

        let action = if replace {
            FIELDS_REPLACED_EVENT_TYPE
        } else {
            FIELDS_PATCHED_EVENT_TYPE
        };
        let timestamp = sim_now();
        let mut retries = 0;

        loop {
            // The append and the actor reply are separate observations. A retry
            // may arrive after the private field-update event committed but the
            // first ask timed out. Durable replay rebuilds this map from the
            // payload below, so the duplicate returns the committed state without
            // spending another event-budget slot.
            if state.has_processed_idempotency_key(&idempotency_key) {
                ctx.reply(EntityResponse {
                    success: true,
                    state: state.clone(),
                    error: None,
                    custom_effects: vec![],
                    scheduled_actions: vec![],
                    spawn_requests: vec![],
                    spec_governed: true,
                });
                return Ok(());
            }

            // A tombstone is terminal. Persisting a field update after it
            // would create a journal suffix replay deliberately never
            // applies, leaving the recovered sequence behind the durable
            // stream. This check stays inside the retry loop because a
            // concurrency catch-up may discover a delete committed by
            // another writer.
            if state.status == "Deleted" {
                ctx.reply(EntityResponse {
                    success: false,
                    state: state.clone(),
                    error: Some("cannot update a deleted entity".to_string()),
                    custom_effects: vec![],
                    scheduled_actions: vec![],
                    spawn_requests: vec![],
                    spec_governed: true,
                });
                return Ok(());
            }

            if !state.can_accept_event() {
                let workspace_id = event_budget_workspace_id(state);
                crate::event_budget_metrics::record_exhausted(
                    &self.tenant,
                    &state.entity_type,
                    &state.entity_id,
                    &workspace_id,
                );
                tracing::warn!(
                    tenant = %self.tenant,
                    entity_type = %state.entity_type,
                    entity_id = %state.entity_id,
                    workspace_id = %workspace_id,
                    status = %state.status,
                    action,
                    events_since_snapshot = state.events_since_snapshot,
                    total_event_count = state.total_event_count,
                    max_events_since_snapshot = MAX_EVENTS_SINCE_SNAPSHOT,
                    "Event budget exhausted (10000 max since snapshot)"
                );
                ctx.reply(EntityResponse {
                    success: false,
                    state: state.clone(),
                    error: Some(format!(
                        "Event budget exhausted ({MAX_EVENTS_SINCE_SNAPSHOT} max since snapshot)"
                    )),
                    custom_effects: vec![],
                    scheduled_actions: vec![],
                    spawn_requests: vec![],
                    spec_governed: true,
                });
                return Ok(());
            }

            let state_before = state.clone();
            if let Err(error) = Self::apply_field_update(state, &fields, replace) {
                *state = state_before;
                ctx.reply(EntityResponse {
                    success: false,
                    state: state.clone(),
                    error: Some(error),
                    custom_effects: vec![],
                    scheduled_actions: vec![],
                    spawn_requests: vec![],
                    spec_governed: true,
                });
                return Ok(());
            }

            if let (Some(store), Some(backend)) = (self.event_journal.as_ref(), self.event_backend)
            {
                match self
                    .persist_field_update(
                        store,
                        backend,
                        state,
                        FieldUpdatePersistence {
                            fields: &fields,
                            replace,
                            idempotency_key: &idempotency_key,
                            timestamp,
                        },
                    )
                    .await
                {
                    Ok(_) => {}
                    Err(PersistenceError::ConcurrencyViolation { actual, .. }) => {
                        let recovered = self
                                    .recover_authoritative_state(store, backend)
                                    .await
                                    .map_err(|error| {
                                        ActorError::custom(format!(
                                            "failed to catch up {}:{} after field-update concurrency loss: {error}",
                                            self.entity_type, self.entity_id
                                        ))
                                    })?;
                        if recovered.sequence_nr < actual {
                            return Err(ActorError::custom(format!(
                                "field-update catch-up under-reached authoritative sequence for {}:{} ({} < {actual})",
                                self.entity_type, self.entity_id, recovered.sequence_nr
                            )));
                        }
                        *state = recovered;

                        if retries < MAX_FIELD_UPDATE_RETRIES {
                            retries += 1;
                            tracing::warn!(
                                tenant = %self.tenant,
                                entity_type = %self.entity_type,
                                entity_id = %self.entity_id,
                                action,
                                actual_seq = actual,
                                retry = retries,
                                "field update lost optimistic-concurrency race; retrying from authoritative state"
                            );
                            continue;
                        }

                        ctx.reply(EntityResponse {
                            success: false,
                            state: state.clone(),
                            error: Some(
                                "persistence failed: optimistic concurrency retry exhausted"
                                    .to_string(),
                            ),
                            custom_effects: vec![],
                            scheduled_actions: vec![],
                            spawn_requests: vec![],
                            spec_governed: true,
                        });
                        return Ok(());
                    }
                    Err(error) => {
                        *state = state_before;
                        ctx.reply(EntityResponse {
                            success: false,
                            state: state.clone(),
                            error: Some(format!("persistence failed: {error}")),
                            custom_effects: vec![],
                            scheduled_actions: vec![],
                            spawn_requests: vec![],
                            spec_governed: true,
                        });
                        return Ok(());
                    }
                }
            }

            let event = EntityEvent {
                action: action.to_string(),
                from_status: state.status.clone(),
                to_status: state.status.clone(),
                timestamp,
                params: fields.clone(),
                idempotency_key: Some(idempotency_key.clone()),
            };
            state.push_event_bounded(event);
            let persistence_id = self.persistence_id();
            if let Some(store) = self.event_journal.as_ref()
                && let Err(error) = Self::maybe_save_snapshot(
                    store,
                    self.snapshot_queue.as_ref(),
                    &persistence_id,
                    state,
                )
                .await
            {
                tracing::warn!(
                    entity = %state.entity_id,
                    seq = state.sequence_nr,
                    error = %error,
                    "failed to persist snapshot after field update"
                );
            }
            ctx.reply(EntityResponse {
                success: true,
                state: state.clone(),
                error: None,
                custom_effects: vec![],
                scheduled_actions: vec![],
                spawn_requests: vec![],
                spec_governed: true,
            });
            return Ok(());
        }
    }
}
