//! Durable baseline event for the first journal write after snapshot-only recovery.

use std::collections::{BTreeSet, VecDeque};

use super::*;

pub(crate) use temper_runtime::persistence::{
    STATE_MATERIALIZATION_EVENT_TYPE, STATE_MATERIALIZATION_PAYLOAD_BYTE_BUDGET,
    STATE_MATERIALIZATION_SCHEMA,
};

/// Complete state baseline that makes later journal deltas independently replayable.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub(crate) struct PersistedStateMaterialization {
    pub(crate) schema: String,
    pub(crate) state: EntityState,
}

#[derive(serde::Serialize)]
struct StateMaterializationRef<'a> {
    schema: &'static str,
    state: MaterializedEntityStateRef<'a>,
}

#[derive(serde::Serialize)]
struct MaterializedEntityStateRef<'a> {
    entity_type: &'a str,
    entity_id: &'a str,
    status: &'a str,
    item_count: usize,
    counters: &'a BTreeMap<String, usize>,
    booleans: &'a BTreeMap<String, bool>,
    lists: &'a BTreeMap<String, Vec<String>>,
    fields: &'a serde_json::Value,
    events: &'a [EntityEvent],
    total_event_count: usize,
    events_since_snapshot: usize,
    last_snapshot_sequence_nr: u64,
    sequence_nr: u64,
    processed_idempotency_keys: &'a BTreeMap<String, u64>,
}

fn bounded_processed_idempotency_keys(state: &EntityState) -> BTreeMap<String, u64> {
    let mut newest = BTreeSet::new();
    for (key, sequence) in &state.processed_idempotency_keys {
        newest.insert((*sequence, key.clone()));
        if newest.len() > crate::entity_actor::types::MAX_DURABLE_IDEMPOTENCY_KEYS_PER_ENTITY {
            newest.pop_first();
        }
    }
    newest
        .into_iter()
        .map(|(sequence, key)| (key, sequence))
        .collect()
}

/// Build the internal event that transfers a snapshot-only generation into the journal.
pub(crate) fn state_materialization_envelope(
    persistence_id: &str,
    state: &EntityState,
    timestamp: chrono::DateTime<chrono::Utc>,
) -> Result<PersistenceEnvelope, PersistenceError> {
    let processed_idempotency_keys = bounded_processed_idempotency_keys(state);
    let materialization = StateMaterializationRef {
        schema: STATE_MATERIALIZATION_SCHEMA,
        state: MaterializedEntityStateRef {
            entity_type: &state.entity_type,
            entity_id: &state.entity_id,
            status: &state.status,
            item_count: state.item_count,
            counters: &state.counters,
            booleans: &state.booleans,
            lists: &state.lists,
            fields: &state.fields,
            events: &[],
            total_event_count: state.total_event_count,
            events_since_snapshot: 0,
            last_snapshot_sequence_nr: 0,
            sequence_nr: 0,
            processed_idempotency_keys: &processed_idempotency_keys,
        },
    };
    let fits_budget = temper_runtime::persistence::serialized_json_fits_byte_budget(
        &materialization,
        STATE_MATERIALIZATION_PAYLOAD_BYTE_BUDGET,
    )
    .map_err(PersistenceError::Serialization)?;
    if !fits_budget {
        return Err(PersistenceError::Serialization(format!(
            "state materialization byte budget exhausted ({STATE_MATERIALIZATION_PAYLOAD_BYTE_BUDGET} bytes)"
        )));
    }
    let mut fields = state.fields.clone();
    let fields_object = fields.as_object_mut().ok_or_else(|| {
        PersistenceError::Serialization(
            "state materialization fields must be a JSON object".to_string(),
        )
    })?;
    fields_object.insert(
        "Id".to_string(),
        serde_json::Value::String(state.entity_id.clone()),
    );
    fields_object.insert(
        "Status".to_string(),
        serde_json::Value::String(state.status.clone()),
    );
    let materialized_state = EntityState {
        entity_type: state.entity_type.clone(),
        entity_id: state.entity_id.clone(),
        status: state.status.clone(),
        item_count: state.item_count,
        counters: state.counters.clone(),
        booleans: state.booleans.clone(),
        lists: state.lists.clone(),
        fields,
        events: VecDeque::new(),
        total_event_count: state.total_event_count,
        events_since_snapshot: 0,
        last_snapshot_sequence_nr: 0,
        sequence_nr: 0,
        processed_idempotency_keys,
    };
    let persisted = PersistedStateMaterialization {
        schema: STATE_MATERIALIZATION_SCHEMA.to_string(),
        state: materialized_state,
    };
    let fits_budget = temper_runtime::persistence::serialized_json_fits_byte_budget(
        &persisted,
        STATE_MATERIALIZATION_PAYLOAD_BYTE_BUDGET,
    )
    .map_err(PersistenceError::Serialization)?;
    if !fits_budget {
        return Err(PersistenceError::Serialization(format!(
            "state materialization byte budget exhausted ({STATE_MATERIALIZATION_PAYLOAD_BYTE_BUDGET} bytes)"
        )));
    }
    let payload = serde_json::to_value(persisted)
        .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
    Ok(PersistenceEnvelope {
        sequence_nr: 0,
        event_type: STATE_MATERIALIZATION_EVENT_TYPE.to_string(),
        payload,
        metadata: EventMetadata {
            event_id: sim_uuid(),
            causation_id: sim_uuid(),
            correlation_id: sim_uuid(),
            timestamp,
            actor_id: persistence_id.to_string(),
        },
    })
}
