use super::*;

impl EntityActor {
    fn apply_field_update(
        state: &mut EntityState,
        fields: &serde_json::Value,
        replace: bool,
    ) -> Result<(), &'static str> {
        let updates = fields
            .as_object()
            .ok_or("field update payload must be a JSON object")?;

        if replace {
            state.fields = serde_json::Value::Object(updates.clone());
        } else {
            let existing = state
                .fields
                .as_object_mut()
                .ok_or("entity fields must be a JSON object")?;
            for (key, value) in updates {
                existing.insert(key.clone(), value.clone());
            }
        }

        let fields = state
            .fields
            .as_object_mut()
            .ok_or("entity fields must be a JSON object")?;
        fields.insert(
            "Id".to_string(),
            serde_json::Value::String(state.entity_id.clone()),
        );
        fields.insert(
            "Status".to_string(),
            serde_json::Value::String(state.status.clone()),
        );
        Ok(())
    }

    /// Apply a durable field-update event during entity replay.
    pub(super) fn apply_field_update_event(
        state: &mut EntityState,
        event: &EntityEvent,
    ) -> Result<(), &'static str> {
        let fields = event
            .params
            .get("fields")
            .ok_or("field update event is missing fields")?;
        let replace = event
            .params
            .get("replace")
            .and_then(serde_json::Value::as_bool)
            .ok_or("field update event is missing replace mode")?;
        Self::apply_field_update(state, fields, replace)
    }

    /// Persist and publish one PATCH or PUT field mutation.
    pub(super) async fn commit_field_update(
        &self,
        state: &mut EntityState,
        fields: &serde_json::Value,
        replace: bool,
    ) -> Result<(), String> {
        let table = self.table.read().expect("table lock poisoned").clone();
        let mut base = state.clone();
        let mut retries_remaining = FIELD_UPDATE_RETRY_BUDGET;

        loop {
            if !base.can_accept_event() {
                return Err(format!(
                    "Event budget exhausted ({MAX_EVENTS_SINCE_SNAPSHOT} max since snapshot)"
                ));
            }

            let mut candidate = base.clone();
            Self::apply_field_update(&mut candidate, fields, replace).map_err(str::to_string)?;
            let event = EntityEvent {
                action: FIELD_UPDATE_EVENT_TYPE.to_string(),
                from_status: candidate.status.clone(),
                to_status: candidate.status.clone(),
                timestamp: sim_now(),
                params: serde_json::json!({
                    "fields": fields,
                    "replace": replace,
                }),
                idempotency_key: None,
            };

            if let (Some(store), Some(backend)) = (self.event_journal.as_ref(), self.event_backend)
            {
                match self
                    .persist_event(
                        store,
                        backend,
                        &self.persistence_id(),
                        &mut candidate,
                        &event,
                    )
                    .await
                {
                    Ok(_) => {}
                    Err(PersistenceError::ConcurrencyViolation { actual, .. })
                        if retries_remaining > 0 =>
                    {
                        retries_remaining -= 1;
                        base = recover_entity_state_from_store(
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
                        .map_err(|error| format!("field update replay failed: {error}"))?;
                        debug_assert!(
                            base.sequence_nr >= actual,
                            "POSTCONDITION: field update replay under-reached authoritative sequence \
                             (base.sequence_nr={} < actual={actual})",
                            base.sequence_nr
                        );
                        *state = base.clone();
                        continue;
                    }
                    Err(PersistenceError::ConcurrencyViolation { .. }) => {
                        return Err("field update retry budget exhausted".to_string());
                    }
                    Err(error) => return Err(format!("persistence failed: {error}")),
                }
            }

            candidate.push_event_bounded(event);
            if let Some(store) = self.event_journal.as_ref()
                && let Err(error) = Self::maybe_save_snapshot(
                    store,
                    self.snapshot_queue.as_ref(),
                    &self.persistence_id(),
                    &mut candidate,
                )
                .await
            {
                tracing::warn!(
                    entity = %candidate.entity_id,
                    seq = candidate.sequence_nr,
                    error = %error,
                    "failed to persist snapshot after field update"
                );
            }
            *state = candidate;
            return Ok(());
        }
    }
}
