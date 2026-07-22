use temper_runtime::persistence::{EventMetadata, PersistenceEnvelope};
use temper_runtime::scheduler::sim_uuid;

use crate::entity_actor::{EntityEvent, EntityState, process_action_with_xref};

use super::super::FileStreamContentError;

pub(super) fn initial_file_state(
    file_id: &str,
    table: &temper_jit::table::TransitionTable,
    initial_fields: serde_json::Value,
) -> EntityState {
    let mut fields = initial_fields;
    if let Some(obj) = fields.as_object_mut() {
        obj.entry("Id".to_string())
            .or_insert(serde_json::Value::String(file_id.to_string()));
        obj.entry("Status".to_string())
            .or_insert(serde_json::Value::String(table.initial_state.clone()));
    }

    EntityState {
        entity_type: "File".to_string(),
        entity_id: file_id.to_string(),
        status: table.initial_state.clone(),
        item_count: 0,
        counters: std::collections::BTreeMap::new(),
        booleans: std::collections::BTreeMap::new(),
        lists: std::collections::BTreeMap::new(),
        fields,
        events: std::collections::VecDeque::new(),
        total_event_count: 0,
        events_since_snapshot: 0,
        last_snapshot_sequence_nr: 0,
        sequence_nr: 0,
        processed_idempotency_keys: std::collections::BTreeMap::new(),
    }
}

pub(super) fn apply_synthetic_file_action(
    state: &mut EntityState,
    table: &temper_jit::table::TransitionTable,
    action: &str,
    params: serde_json::Value,
    cross_entity_booleans: &std::collections::BTreeMap<String, bool>,
) -> Result<EntityEvent, FileStreamContentError> {
    let result = process_action_with_xref(state, table, action, &params, cross_entity_booleans);
    if !result.overflow_blobs.is_empty() {
        return Err(FileStreamContentError::State(format!(
            "File.{action} produced field-overflow blobs on the atomic initial content path"
        )));
    }
    if !result.success {
        return Err(FileStreamContentError::ActionRejected(
            result
                .error
                .unwrap_or_else(|| format!("File.{action} rejected")),
        ));
    }
    result.event.ok_or_else(|| {
        FileStreamContentError::State(format!(
            "File.{action} succeeded without producing a durable event"
        ))
    })
}

pub(super) fn push_synthetic_event(
    state: &mut EntityState,
    events: &mut Vec<EntityEvent>,
    event: EntityEvent,
) {
    state.sequence_nr = state.sequence_nr.saturating_add(1);
    state.push_event_bounded(event.clone());
    events.push(event);
}

pub(super) fn synthetic_envelope(
    persistence_id: &str,
    sequence_nr: u64,
    event: &EntityEvent,
) -> Result<PersistenceEnvelope, FileStreamContentError> {
    let payload = serde_json::to_value(event)
        .map_err(|e| FileStreamContentError::State(format!("failed to serialize event: {e}")))?;
    Ok(PersistenceEnvelope {
        sequence_nr,
        event_type: event.action.clone(),
        payload,
        metadata: EventMetadata {
            event_id: sim_uuid(),
            causation_id: sim_uuid(),
            correlation_id: sim_uuid(),
            timestamp: event.timestamp,
            actor_id: persistence_id.to_string(),
        },
    })
}
