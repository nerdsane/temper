//! Semantic validation for security-sensitive full-journal replay.

use temper_jit::table::{Effect, TransitionTable};
use temper_runtime::actor::ActorError;
use temper_runtime::persistence::{CompositeEvent, PersistenceEnvelope};

use super::types::{EntityEvent, EntityState};

pub(super) fn validate_strict_composite_event(
    tenant: &str,
    state: &EntityState,
    envelope: &PersistenceEnvelope,
) -> Result<(), ActorError> {
    let event: CompositeEvent =
        serde_json::from_value(envelope.payload.clone()).map_err(|error| {
            ActorError::custom(format!(
                "incompatible composite audit event for {}:{} at sequence {}: {error}",
                state.entity_type, state.entity_id, envelope.sequence_nr
            ))
        })?;
    if event.tenant != tenant
        || event.parent_entity_type != state.entity_type
        || event.parent_entity_id != state.entity_id
    {
        return Err(ActorError::custom(format!(
            "misbound composite audit event for {}:{} at sequence {}",
            state.entity_type, state.entity_id, envelope.sequence_nr
        )));
    }
    Ok(())
}

pub(super) fn validate_strict_entity_event(
    table: &TransitionTable,
    state: &EntityState,
    envelope: &PersistenceEnvelope,
    event: &EntityEvent,
) -> Result<(), ActorError> {
    if envelope.event_type != event.action {
        return Err(incompatible_event(
            state,
            envelope,
            format!(
                "envelope type '{}' differs from payload action '{}'",
                envelope.event_type, event.action
            ),
        ));
    }

    if event.action == "Deleted" {
        if event.from_status != state.status || event.to_status != "Deleted" {
            return Err(incompatible_event(
                state,
                envelope,
                format!(
                    "tombstone transition '{} -> {}' does not match current '{}' -> 'Deleted'",
                    event.from_status, event.to_status, state.status
                ),
            ));
        }
        return Ok(());
    }

    if event.action == "Created" && event.from_status.is_empty() {
        if envelope.sequence_nr != 1
            || state.total_event_count != 0
            || event.to_status != table.initial_state
        {
            return Err(incompatible_event(
                state,
                envelope,
                format!(
                    "bootstrap Created event must be sequence 1 and target initial state '{}'",
                    table.initial_state
                ),
            ));
        }
        return Ok(());
    }

    if event.from_status != state.status {
        return Err(incompatible_event(
            state,
            envelope,
            format!(
                "payload from-status '{}' does not match replay state '{}'",
                event.from_status, state.status
            ),
        ));
    }

    let Some(rule) = table.rules.iter().find(|rule| {
        rule.name == event.action
            && (rule.from_states.is_empty()
                || rule
                    .from_states
                    .iter()
                    .any(|status| status == &state.status))
    }) else {
        return Err(incompatible_event(
            state,
            envelope,
            format!(
                "action '{}' has no transition from '{}' in the active spec",
                event.action, state.status
            ),
        ));
    };

    let mut expected_status = state.status.clone();
    for effect in &rule.effects {
        if let Effect::SetState(status) = effect {
            expected_status.clone_from(status);
        }
    }
    let fallback_status = rule.to_state.as_deref().unwrap_or(&state.status);
    if expected_status == state.status && !fallback_status.is_empty() {
        expected_status = fallback_status.to_string();
    }
    if event.to_status != expected_status {
        return Err(incompatible_event(
            state,
            envelope,
            format!(
                "payload target '{}' differs from active transition target '{}'",
                event.to_status, expected_status
            ),
        ));
    }
    Ok(())
}

fn incompatible_event(
    state: &EntityState,
    envelope: &PersistenceEnvelope,
    detail: String,
) -> ActorError {
    ActorError::custom(format!(
        "incompatible event for {}:{} at sequence {}: {detail}",
        state.entity_type, state.entity_id, envelope.sequence_nr
    ))
}
