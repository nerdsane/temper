//! Exact scoped entity bootstrap creation and action recovery.

use std::collections::BTreeMap;

use temper_runtime::persistence::schema_deployment::SchemaExecutionPin;
use temper_runtime::tenant::TenantId;

use crate::entity_actor::{EntityEvent, SCHEMA_BOOTSTRAP_ACTION_OUTCOME_FIELD};
use crate::state::ServerState;

/// Immutable sequence of the bootstrap-owned first event.
pub(crate) struct BootstrapEntityCreation {
    /// Durable sequence of the bootstrap-owned `Created` event.
    pub(crate) creation_sequence: u64,
}

/// Exact post-action state co-committed in the scoped entity journal.
pub(crate) struct BootstrapActionJournalOutcome {
    /// Authoritative sequence of the initial action event.
    pub(crate) sequence: u64,
    /// Exact internal actor fields immediately after that action.
    pub(crate) fields: serde_json::Value,
    /// Exact lifecycle state immediately after that action.
    pub(crate) status: String,
}

impl ServerState {
    /// Create or recover a scoped entity owned by one durable bootstrap operation.
    pub(crate) async fn get_or_create_scoped_entity_for_bootstrap(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        initial_fields: serde_json::Value,
        schema_pin: SchemaExecutionPin,
        creation_idempotency_key: String,
    ) -> Result<BootstrapEntityCreation, String> {
        let response = self
            .get_or_create_entity_with_schema_pin(
                tenant,
                entity_type,
                entity_id,
                initial_fields,
                Some(schema_pin.clone()),
                Some(creation_idempotency_key.clone()),
            )
            .await?;
        let creation_sequence = if let Some((store, _)) = self.event_journal() {
            let persistence_id = format!(
                "{tenant}:{entity_type}:{}",
                temper_runtime::persistence::schema_deployment::scoped_journal_entity_id(
                    entity_id,
                    &schema_pin,
                )
            );
            let first = store
                .read_events_limited(&persistence_id, 0, 1)
                .await
                .map_err(|error| format!("bootstrap creation journal read failed: {error}"))?;
            let Some(envelope) = first.first() else {
                return Err("bootstrap creation journal is empty after actor creation".into());
            };
            let event = serde_json::from_value::<EntityEvent>(envelope.payload.clone())
                .map_err(|error| format!("bootstrap creation event is invalid: {error}"))?;
            if event.action != "Created"
                || event.idempotency_key.as_deref() != Some(&creation_idempotency_key)
            {
                return Err(
                    "BootstrapTargetConflict: existing journal is owned by another creation".into(),
                );
            }
            envelope.sequence_nr
        } else {
            response
                .state
                .processed_idempotency_keys
                .get(&creation_idempotency_key)
                .copied()
                .ok_or_else(|| {
                    "BootstrapTargetConflict: existing actor is owned by another creation"
                        .to_string()
                })?
        };
        Ok(BootstrapEntityCreation { creation_sequence })
    }

    /// Recover an exact initial-action outcome without consulting current actor state.
    #[expect(
        clippy::too_many_arguments,
        reason = "journal identity, exact action evidence, and replay budget stay explicit"
    )]
    pub(crate) async fn scoped_bootstrap_action_outcome(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        schema_pin: &SchemaExecutionPin,
        action_idempotency_key: &str,
        expected_action: &str,
        canonical_parameters_json: &str,
        replay_budget: usize,
    ) -> Result<Option<BootstrapActionJournalOutcome>, String> {
        if replay_budget == 0 || replay_budget > 10_000 {
            return Err("bootstrap action replay budget must be between 1 and 10000".into());
        }
        let Some((store, _)) = self.event_journal() else {
            return Ok(None);
        };
        let persistence_id = format!(
            "{tenant}:{entity_type}:{}",
            temper_runtime::persistence::schema_deployment::scoped_journal_entity_id(
                entity_id, schema_pin,
            )
        );
        const PAGE_SIZE: usize = 256;
        let mut from_sequence = 0;
        let mut consumed = 0usize;
        loop {
            let remaining = replay_budget.saturating_sub(consumed);
            if remaining == 0 {
                return Err("bootstrap action replay budget exhausted before exact outcome".into());
            }
            let events = store
                .read_events_limited(&persistence_id, from_sequence, remaining.min(PAGE_SIZE))
                .await
                .map_err(|error| format!("bootstrap action journal read failed: {error}"))?;
            if events.is_empty() {
                return Ok(None);
            }
            for envelope in &events {
                consumed = consumed.saturating_add(1);
                let matches_key = envelope
                    .payload
                    .get("idempotency_key")
                    .and_then(serde_json::Value::as_str)
                    == Some(action_idempotency_key);
                if !matches_key {
                    continue;
                }
                if envelope
                    .payload
                    .get("action")
                    .and_then(serde_json::Value::as_str)
                    != Some(expected_action)
                {
                    return Err("bootstrap action event does not match the reserved action".into());
                }
                let expected_params: serde_json::Value =
                    serde_json::from_str(canonical_parameters_json).map_err(|error| {
                        format!("reserved action parameters are invalid: {error}")
                    })?;
                if envelope.payload.get("params") != Some(&expected_params) {
                    return Err(
                        "bootstrap action event parameters do not match the reservation".into(),
                    );
                }
                let outcome = envelope
                    .payload
                    .get(SCHEMA_BOOTSTRAP_ACTION_OUTCOME_FIELD)
                    .and_then(serde_json::Value::as_object)
                    .ok_or_else(|| {
                        "bootstrap action event is missing its durable outcome".to_string()
                    })?;
                let fields = outcome.get("fields").cloned().ok_or_else(|| {
                    "bootstrap action event is missing durable result fields".to_string()
                })?;
                let status = outcome
                    .get("status")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| {
                        "bootstrap action event is missing durable result status".to_string()
                    })?
                    .to_string();
                return Ok(Some(BootstrapActionJournalOutcome {
                    sequence: envelope.sequence_nr,
                    fields,
                    status,
                }));
            }
            from_sequence = events
                .last()
                .map_or(from_sequence, |event| event.sequence_nr);
            if events.len() < remaining.min(PAGE_SIZE) {
                return Ok(None);
            }
        }
    }

    /// Check that the first durable event belongs to the reserved bootstrap operation.
    pub(super) async fn bootstrap_creation_is_owned_by(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        schema_pin: Option<&SchemaExecutionPin>,
        processed_idempotency_keys: &BTreeMap<String, u64>,
        creation_idempotency_key: &str,
    ) -> Result<bool, String> {
        let Some((schema_pin, (store, _))) = schema_pin.zip(self.event_journal()) else {
            return Ok(processed_idempotency_keys.contains_key(creation_idempotency_key));
        };
        let persistence_id = format!(
            "{tenant}:{entity_type}:{}",
            temper_runtime::persistence::schema_deployment::scoped_journal_entity_id(
                entity_id, schema_pin,
            )
        );
        let first = store
            .read_events_limited(&persistence_id, 0, 1)
            .await
            .map_err(|error| format!("bootstrap creation journal read failed: {error}"))?;
        Ok(first.first().is_some_and(|envelope| {
            serde_json::from_value::<EntityEvent>(envelope.payload.clone())
                .ok()
                .and_then(|event| event.idempotency_key)
                .as_deref()
                == Some(creation_idempotency_key)
        }))
    }
}
