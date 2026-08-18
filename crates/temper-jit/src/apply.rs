//! Shared effect application — the definition of what each [`Effect`] means.
//!
//! Runtime adapters implement [`EffectTarget`] and call [`apply_effects`].
//! Schedule, spawn, emit, and custom names come back as work for the adapter
//! to run. See ADR-0166.

use serde::{Deserialize, Serialize};

use crate::table::Effect;
use temper_runtime::scheduler::sim_uuid;

/// Work produced by applying effects. The adapter runs these.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AppliedEffects {
    /// Named custom effects (post-transition hooks or routed tells).
    pub custom: Vec<String>,
    /// Named emit events (log or routed tells).
    pub emit: Vec<String>,
    /// Timer requests with a fixed delay.
    pub scheduled: Vec<ScheduledAction>,
    /// Child-entity create requests.
    pub spawns: Vec<SpawnRequest>,
    /// Timer requests resolved after fields are synced.
    pub schedule_at: Vec<ScheduleAtRequest>,
}

/// A scheduled action to fire after a delay.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScheduledAction {
    /// The action name to dispatch.
    pub action: String,
    /// Delay in seconds before dispatching the action.
    pub delay_seconds: u64,
}

/// A request to spawn a child entity (executed post-transition by the runtime).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SpawnRequest {
    /// The child entity type.
    pub entity_type: String,
    /// The child entity ID.
    pub entity_id: String,
    /// Optional action to dispatch on the child after creation.
    pub initial_action: Option<String>,
    /// Optional field on the parent to store the child's ID.
    pub store_id_in: Option<String>,
    /// Optional list of field names to copy from parent state into the child's params.
    pub copy_fields: Option<Vec<String>>,
    /// Field values copied from parent state (populated when `copy_fields` is set).
    #[serde(default)]
    pub copied_field_values: serde_json::Map<String, serde_json::Value>,
}

/// A deferred schedule-at request — resolved after field sync.
///
/// Unlike [`ScheduledAction`] (fixed delay), this reads an absolute ISO 8601
/// timestamp from an entity field and computes the delay at resolution time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScheduleAtRequest {
    /// The action name to dispatch.
    pub action: String,
    /// The entity field containing the ISO 8601 timestamp.
    pub field: String,
}

/// Mutable entity bag that [`apply_effects`] writes into.
///
/// Implement this on the runtime's state type. Do not put event logs, blob
/// overflow, or HTTP types here.
pub trait EffectTarget {
    /// Set the state-machine status.
    fn set_status(&mut self, status: String);

    /// Add `amount` to a named counter.
    fn add_counter(&mut self, var: &str, amount: usize);

    /// Subtract `amount` from a named counter, saturating at zero.
    fn sub_counter(&mut self, var: &str, amount: usize);

    /// Replace a named counter.
    fn set_counter(&mut self, var: &str, value: usize);

    /// Set a named boolean.
    fn set_bool(&mut self, var: &str, value: bool);

    /// Append a string to a named list.
    fn list_append(&mut self, var: &str, value: String);

    /// Remove the element at `index` from a named list, if in range.
    fn list_remove_at(&mut self, var: &str, index: usize);

    /// Store a string on a named field (used by spawn `store_id_in`).
    fn store_field_string(&mut self, field: &str, value: String);

    /// Read a named field for spawn `copy_fields`.
    fn field_value(&self, field: &str) -> Option<serde_json::Value>;

    /// Called when `SetCounterFromParam` cannot parse a non-negative integer.
    fn on_skipped_counter(&self, _var: &str, _param: &str) {}
}

/// Apply every effect to `state`. Returns side-effect work for the adapter.
pub fn apply_effects(
    state: &mut impl EffectTarget,
    effects: &[Effect],
    params: &serde_json::Value,
) -> AppliedEffects {
    let mut applied = AppliedEffects::default();

    for effect in effects {
        match effect {
            Effect::SetState(status) => {
                state.set_status(status.clone());
            }
            Effect::IncrementItems => {
                state.add_counter("items", 1);
            }
            Effect::DecrementItems => {
                state.sub_counter("items", 1);
            }
            Effect::IncrementCounter(var) => {
                state.add_counter(var, 1);
            }
            Effect::IncrementCounterByParam { var, param } => {
                state.add_counter(var, counter_delta_from_params(params, param));
            }
            Effect::DecrementCounter(var) => {
                state.sub_counter(var, 1);
            }
            Effect::DecrementCounterByParam { var, param } => {
                state.sub_counter(var, counter_delta_from_params(params, param));
            }
            Effect::SetCounterFromParam { var, param } => {
                let parsed = params
                    .get(param)
                    .and_then(|v| {
                        v.as_u64()
                            .or_else(|| v.as_i64().and_then(|n| u64::try_from(n).ok()))
                    })
                    .and_then(|n| usize::try_from(n).ok());
                match parsed {
                    Some(value) => state.set_counter(var, value),
                    None => state.on_skipped_counter(var, param),
                }
            }
            Effect::SetBool { var, value } => {
                state.set_bool(var, *value);
            }
            Effect::ListAppend(var) => {
                if let Some(val) = params.get(var).and_then(|v| v.as_str()) {
                    state.list_append(var, val.to_string());
                }
            }
            Effect::ListRemoveAt(var) => {
                let index_key = format!("{var}_index");
                if let Some(idx) = params.get(&index_key).and_then(|v| v.as_u64()) {
                    state.list_remove_at(var, idx as usize);
                }
            }
            Effect::EmitEvent(evt) => {
                applied.emit.push(evt.clone());
            }
            Effect::Custom(effect_name) => {
                applied.custom.push(effect_name.clone());
            }
            Effect::ScheduleAction {
                action,
                delay_seconds,
            } => {
                applied.scheduled.push(ScheduledAction {
                    action: action.clone(),
                    delay_seconds: *delay_seconds,
                });
            }
            Effect::SpawnEntity {
                entity_type,
                entity_id_source,
                initial_action,
                store_id_in,
                copy_fields,
            } => {
                let child_id = if entity_id_source == "{uuid}" {
                    sim_uuid().to_string()
                } else {
                    params
                        .get(entity_id_source)
                        .and_then(|v| v.as_str())
                        .map(ToString::to_string)
                        .unwrap_or_else(|| sim_uuid().to_string())
                };

                if let Some(field_name) = store_id_in {
                    state.store_field_string(field_name, child_id.clone());
                }

                let mut copied_field_values = serde_json::Map::new();
                if let Some(fields_to_copy) = copy_fields {
                    for field_name in fields_to_copy {
                        if let Some(value) = state.field_value(field_name) {
                            copied_field_values.insert(field_name.clone(), value);
                        }
                    }
                }

                applied.spawns.push(SpawnRequest {
                    entity_type: entity_type.clone(),
                    entity_id: child_id,
                    initial_action: initial_action.clone(),
                    store_id_in: store_id_in.clone(),
                    copy_fields: copy_fields.clone(),
                    copied_field_values,
                });
            }
            Effect::ScheduleAtAction { action, field } => {
                applied.schedule_at.push(ScheduleAtRequest {
                    action: action.clone(),
                    field: field.clone(),
                });
            }
        }
    }

    applied
}

fn counter_delta_from_params(params: &serde_json::Value, param: &str) -> usize {
    params
        .get(param)
        .and_then(|value| match value {
            serde_json::Value::Number(number) => number.as_u64().map(|v| v as usize),
            serde_json::Value::String(text) => text.parse::<usize>().ok(),
            _ => None,
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::table::Effect;
    use std::collections::BTreeMap;

    #[derive(Default)]
    struct MemoryTarget {
        status: String,
        counters: BTreeMap<String, usize>,
        booleans: BTreeMap<String, bool>,
        lists: BTreeMap<String, Vec<String>>,
        fields: serde_json::Map<String, serde_json::Value>,
    }

    impl EffectTarget for MemoryTarget {
        fn set_status(&mut self, status: String) {
            self.status = status;
        }

        fn add_counter(&mut self, var: &str, amount: usize) {
            *self.counters.entry(var.to_string()).or_default() += amount;
        }

        fn sub_counter(&mut self, var: &str, amount: usize) {
            let entry = self.counters.entry(var.to_string()).or_default();
            *entry = entry.saturating_sub(amount);
        }

        fn set_counter(&mut self, var: &str, value: usize) {
            self.counters.insert(var.to_string(), value);
        }

        fn set_bool(&mut self, var: &str, value: bool) {
            self.booleans.insert(var.to_string(), value);
        }

        fn list_append(&mut self, var: &str, value: String) {
            self.lists.entry(var.to_string()).or_default().push(value);
        }

        fn list_remove_at(&mut self, var: &str, index: usize) {
            if let Some(list) = self.lists.get_mut(var)
                && index < list.len()
            {
                list.remove(index);
            }
        }

        fn store_field_string(&mut self, field: &str, value: String) {
            self.fields
                .insert(field.to_string(), serde_json::Value::String(value));
        }

        fn field_value(&self, field: &str) -> Option<serde_json::Value> {
            self.fields.get(field).cloned()
        }
    }

    #[test]
    fn apply_mutates_status_counters_lists_and_returns_work() {
        let mut state = MemoryTarget::default();
        state.counters.insert("used_bytes".into(), 10);
        state
            .fields
            .insert("owner".into(), serde_json::json!("rita"));

        let effects = vec![
            Effect::SetState("Active".into()),
            Effect::IncrementItems,
            Effect::IncrementCounterByParam {
                var: "used_bytes".into(),
                param: "size_bytes".into(),
            },
            Effect::DecrementCounterByParam {
                var: "used_bytes".into(),
                param: "released_bytes".into(),
            },
            Effect::ListAppend("tags".into()),
            Effect::SetBool {
                var: "ready".into(),
                value: true,
            },
            Effect::EmitEvent("Opened".into()),
            Effect::Custom("Notify".into()),
            Effect::ScheduleAction {
                action: "Refresh".into(),
                delay_seconds: 3600,
            },
            Effect::ScheduleAtAction {
                action: "Expire".into(),
                field: "expires_at".into(),
            },
            Effect::SpawnEntity {
                entity_type: "Child".into(),
                entity_id_source: "{uuid}".into(),
                initial_action: Some("Start".into()),
                store_id_in: Some("child_id".into()),
                copy_fields: Some(vec!["owner".into()]),
            },
        ];

        let _guard = temper_runtime::scheduler::install_deterministic_context(42);
        let applied = apply_effects(
            &mut state,
            &effects,
            &serde_json::json!({
                "size_bytes": "30",
                "released_bytes": 7,
                "tags": "urgent",
            }),
        );

        assert_eq!(state.status, "Active");
        assert_eq!(state.counters.get("items"), Some(&1));
        assert_eq!(state.counters.get("used_bytes"), Some(&33));
        assert_eq!(state.lists.get("tags"), Some(&vec!["urgent".to_string()]));
        assert_eq!(state.booleans.get("ready"), Some(&true));
        assert_eq!(applied.emit, vec!["Opened".to_string()]);
        assert_eq!(applied.custom, vec!["Notify".to_string()]);
        assert_eq!(applied.scheduled[0].action, "Refresh");
        assert_eq!(applied.schedule_at[0].field, "expires_at");
        assert_eq!(applied.spawns.len(), 1);
        assert_eq!(applied.spawns[0].entity_type, "Child");
        assert_eq!(
            applied.spawns[0].copied_field_values.get("owner"),
            Some(&serde_json::json!("rita"))
        );
        assert!(state.fields.get("child_id").is_some());
    }

    #[test]
    fn set_counter_from_param_skips_non_integer() {
        let mut state = MemoryTarget::default();
        apply_effects(
            &mut state,
            &[Effect::SetCounterFromParam {
                var: "n".into(),
                param: "bad".into(),
            }],
            &serde_json::json!({ "bad": "nope" }),
        );
        assert!(state.counters.get("n").is_none());
    }
}
