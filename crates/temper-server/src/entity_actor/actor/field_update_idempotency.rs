//! Canonical request-intent binding for durable HTTP PATCH/PUT retries.

use sha2::{Digest, Sha256};

use super::field_updates::{
    FIELD_UPDATE_EVENT_TYPE, FIELD_UPDATE_SCHEMA, FIELDS_PATCHED_EVENT_TYPE,
    FIELDS_REPLACED_EVENT_TYPE, PersistedFieldUpdate,
};
use super::*;

pub(super) fn field_update_intent_fingerprint(
    fields: &serde_json::Value,
    replace: bool,
) -> Result<String, ActorError> {
    fn canonicalize(value: &serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::Array(values) => {
                serde_json::Value::Array(values.iter().map(canonicalize).collect())
            }
            serde_json::Value::Object(object) => {
                let mut names = object.keys().collect::<Vec<_>>();
                names.sort_unstable();
                let mut canonical = serde_json::Map::new();
                for name in names {
                    canonical.insert(name.clone(), canonicalize(&object[name]));
                }
                serde_json::Value::Object(canonical)
            }
            scalar => scalar.clone(),
        }
    }

    let canonical_fields = canonicalize(fields);
    let canonical_intent = serde_json::to_vec(&(FIELD_UPDATE_SCHEMA, replace, canonical_fields))
        .map_err(|error| {
            ActorError::custom(format!(
                "failed to serialize field-update intent for idempotency: {error}"
            ))
        })?;
    Ok(format!("{:x}", Sha256::digest(canonical_intent)))
}

impl EntityActor {
    pub(super) async fn processed_field_update_matches_intent(
        &self,
        state: &EntityState,
        fields: &serde_json::Value,
        replace: bool,
        idempotency_key: &str,
        intent_fingerprint: &str,
    ) -> Result<bool, ActorError> {
        let sequence = state
            .processed_idempotency_keys
            .get(idempotency_key)
            .copied()
            .ok_or_else(|| {
                ActorError::custom(format!(
                    "processed field-update idempotency key '{}' has no durable sequence for {}:{}",
                    idempotency_key, self.entity_type, self.entity_id
                ))
            })?;

        let Some(store) = self.event_journal.as_ref() else {
            let Some(event) = state
                .events
                .iter()
                .rev()
                .find(|event| event.idempotency_key.as_deref() == Some(idempotency_key))
            else {
                return Err(ActorError::custom(format!(
                    "cannot verify in-memory field-update intent for {}:{} key '{}'",
                    self.entity_type, self.entity_id, idempotency_key
                )));
            };
            let stored_replace = match event.action.as_str() {
                FIELDS_PATCHED_EVENT_TYPE => false,
                FIELDS_REPLACED_EVENT_TYPE => true,
                _ => return Ok(false),
            };
            return Ok(stored_replace == replace && event.params == *fields);
        };

        let envelopes = store
            .read_events_page(
                &self.persistence_id(),
                sequence.saturating_sub(1),
                sequence,
                1,
            )
            .await
            .map_err(|error| {
                ActorError::custom(format!(
                    "failed to reload durable field-update intent for {}:{} key '{}': {error}",
                    self.entity_type, self.entity_id, idempotency_key
                ))
            })?;
        let Some(envelope) = envelopes.first() else {
            return Err(ActorError::custom(format!(
                "durable field-update idempotency sequence {sequence} is missing for {}:{} key '{}'",
                self.entity_type, self.entity_id, idempotency_key
            )));
        };
        if envelope.sequence_nr != sequence {
            return Err(ActorError::custom(format!(
                "durable field-update idempotency sequence mismatch for {}:{} key '{}' (expected {sequence}, found {})",
                self.entity_type, self.entity_id, idempotency_key, envelope.sequence_nr
            )));
        }
        if envelope.event_type != FIELD_UPDATE_EVENT_TYPE {
            return Ok(false);
        }

        let persisted =
            serde_json::from_value::<PersistedFieldUpdate>(envelope.payload.clone()).map_err(
                |error| {
                    ActorError::custom(format!(
                        "failed to decode durable field-update intent for {}:{} at sequence {sequence}: {error}",
                        self.entity_type, self.entity_id
                    ))
                },
            )?;
        if persisted.schema != FIELD_UPDATE_SCHEMA
            || persisted.idempotency_key.as_deref() != Some(idempotency_key)
        {
            return Err(ActorError::custom(format!(
                "durable field-update identity mismatch for {}:{} key '{}' at sequence {sequence}",
                self.entity_type, self.entity_id, idempotency_key
            )));
        }

        let stored_fingerprint =
            field_update_intent_fingerprint(&persisted.fields, persisted.replace)?;
        if let Some(recorded_fingerprint) = persisted.intent_fingerprint.as_deref()
            && recorded_fingerprint != stored_fingerprint
        {
            return Err(ActorError::custom(format!(
                "durable field-update intent fingerprint mismatch for {}:{} key '{}' at sequence {sequence}",
                self.entity_type, self.entity_id, idempotency_key
            )));
        }
        Ok(stored_fingerprint == intent_fingerprint)
    }
}
