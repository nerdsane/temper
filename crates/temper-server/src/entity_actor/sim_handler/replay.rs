//! Deterministic reconstruction of simulation-visible entity state.

use temper_jit::table::TransitionTable;

use super::super::types::{EntityEvent, EntityState, FIELD_UPDATE_EVENT_TYPE};

pub(super) fn rebuild(
    state: &mut EntityState,
    table: &TransitionTable,
    journal: Vec<EntityEvent>,
) -> Result<(), String> {
    for (index, event) in journal.into_iter().enumerate() {
        if event.action == FIELD_UPDATE_EVENT_TYPE {
            let fields = event
                .params
                .get("fields")
                .expect("simulation field-update journal entry must contain fields");
            let replace = event
                .params
                .get("replace")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            assert!(
                super::super::effects::apply_field_update(state, fields, replace),
                "simulation field-update journal fields must be an object"
            );
        } else {
            let from_status = event.from_status.clone();
            if let Some(effects) = table.replay_effects(&state.status, &event.action) {
                let effects = effects.to_vec();
                let _ = super::super::effects::apply_effects(state, &effects, &event.params);
            }
            super::super::effects::apply_new_state_fallback(state, &from_status, &event.to_status);
            super::super::effects::sync_fields_with_metadata(
                state,
                &event.params,
                super::super::effects::FieldSyncMode::InlineTruncate,
                Some(&table.state_var_metadata),
            );
        }
        let sequence_nr = u64::try_from(index + 1)
            .map_err(|_| "simulation event sequence overflow".to_string())?;
        state.record_committed_event(event, sequence_nr);
    }
    Ok(())
}
