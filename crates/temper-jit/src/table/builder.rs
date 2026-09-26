//! TransitionTable constructors.
//!
//! Builds transition tables from I/O Automaton specifications using the shared
//! translation layer in `temper-spec`. The shared layer eliminates duplicated
//! guard/effect translation logic between JIT and verification paths.

use temper_spec::automaton::{self, Automaton, ResolvedEffect, translate_actions};

use super::types::{
    CompositeActionMetadata, CompositeCedarGate, Effect, SubWriteSpec, TransitionRule,
    TransitionTable,
};

impl TransitionTable {
    /// Build a TransitionTable from I/O Automaton TOML source.
    ///
    /// Returns an error if the TOML fails to parse. Prefer this over
    /// [`from_ioa_source`](Self::from_ioa_source) in production code
    /// where parse errors should be propagated.
    pub fn try_from_ioa_source(ioa_toml: &str) -> Result<Self, String> {
        let automaton = automaton::parse_automaton(ioa_toml)
            .map_err(|e| format!("failed to parse I/O Automaton TOML: {e}"))?;
        Ok(Self::from_automaton(&automaton))
    }

    /// Build a TransitionTable from I/O Automaton TOML source.
    ///
    /// # Panics
    ///
    /// Panics if the TOML fails to parse. Use [`try_from_ioa_source`](Self::try_from_ioa_source)
    /// for fallible construction.
    pub fn from_ioa_source(ioa_toml: &str) -> Self {
        Self::try_from_ioa_source(ioa_toml).expect("failed to parse I/O Automaton TOML")
    }

    /// Build a TransitionTable directly from a parsed [`Automaton`].
    ///
    /// Each action becomes a [`TransitionRule`] with guards and effects
    /// derived from the IOA specification via the shared translation layer.
    /// Output actions are skipped (they don't transition state).
    pub fn from_automaton(automaton: &Automaton) -> Self {
        let resolved_actions = translate_actions(automaton);

        let rules: Vec<TransitionRule> = resolved_actions
            .into_iter()
            .map(|a| {
                let mut effects = Vec::new();
                if let Some(ref to) = a.to_state {
                    effects.push(Effect::SetState(to.clone()));
                }
                for e in a.effects {
                    effects.push(convert_effect(e));
                }

                TransitionRule {
                    name: a.name,
                    from_states: a.from_states,
                    to_state: a.to_state,
                    guard: a.guard,
                    effects,
                }
            })
            .collect();

        // Build action-name → rule-indices index for O(log K) lookup.
        let mut rule_index = std::collections::BTreeMap::new();
        for (i, rule) in rules.iter().enumerate() {
            rule_index
                .entry(rule.name.clone())
                .or_insert_with(Vec::new)
                .push(i);
        }

        // ADR-0045 / ADR-0047: collect per-state-variable overflow metadata.
        let mut state_var_metadata = std::collections::BTreeMap::new();
        for sv in &automaton.state {
            if sv.overflow_inline_max_bytes.is_some() || sv.overflow_ttl_seconds.is_some() {
                state_var_metadata.insert(
                    sv.name.clone(),
                    super::types::StateVarMetadata {
                        overflow_inline_max_bytes: sv.overflow_inline_max_bytes,
                        overflow_ttl_seconds: sv.overflow_ttl_seconds,
                    },
                );
            }
        }

        let mut composite_actions = std::collections::BTreeMap::new();
        for action in &automaton.actions {
            if !action.kind.eq_ignore_ascii_case("composite") {
                continue;
            }
            composite_actions.insert(
                action.name.clone(),
                CompositeActionMetadata {
                    cedar_gate: action.cedar_gate.as_ref().map(|gate| CompositeCedarGate {
                        principal: gate.principal.clone(),
                        resource: gate.resource.clone(),
                        action: gate.action.clone(),
                    }),
                    record_parent_event: action.record_parent_event,
                    sub_writes: action
                        .sub_writes
                        .iter()
                        .map(|write| SubWriteSpec {
                            target_entity: write.target_entity.clone(),
                            action: write.action.clone(),
                            generated_from: write.generated_from.clone(),
                        })
                        .collect(),
                },
            );
        }

        TransitionTable {
            entity_name: automaton.automaton.name.clone(),
            states: automaton.automaton.states.clone(),
            initial_state: automaton.automaton.initial.clone(),
            rules,
            keys: automaton
                .keys
                .iter()
                .map(|k| super::types::DeclaredKey {
                    name: k.name.clone(),
                    properties: k.properties.clone(),
                })
                .collect(),
            vectors: automaton
                .vectors
                .iter()
                .map(|v| super::types::DeclaredVector {
                    name: v.name.clone(),
                    property: v.property.clone(),
                    model_property: v.model_property.clone(),
                    dims: v.dims,
                    metric: v.metric.clone(),
                })
                .collect(),
            state_var_metadata,
            composite_actions,
            strict_action_params: automaton.automaton.strict_action_params,
            initial_values: super::action_contract::InitialValues::from_declarations(
                &automaton.state,
            ),
            action_contracts: automaton
                .actions
                .iter()
                .map(|action| {
                    (
                        action.name.clone(),
                        super::action_contract::ActionContract {
                            params: action
                                .params
                                .iter()
                                .map(|param| param.name().to_owned())
                                .collect(),
                            param_types: action
                                .params
                                .iter()
                                .filter_map(|param| match param {
                                    temper_spec::automaton::ActionParam::Typed {
                                        name,
                                        param_type,
                                    } => Some((name.clone(), param_type.clone())),
                                    temper_spec::automaton::ActionParam::Named(_) => None,
                                })
                                .collect(),
                            constraints: action.constraints.clone(),
                        },
                    )
                })
                .collect(),
            rule_index,
        }
    }
}

/// Convert a shared [`ResolvedEffect`] to the JIT [`Effect`] type.
fn convert_effect(effect: ResolvedEffect) -> Effect {
    match effect {
        ResolvedEffect::SetCounter { var, value } => Effect::SetCounter { var, value },
        ResolvedEffect::AddCounter { var, value } => Effect::AddCounter { var, value },
        ResolvedEffect::SubCounter { var, value } => Effect::SubCounter { var, value },
        ResolvedEffect::SetBool { var, value } => Effect::SetBool { var, value },
        ResolvedEffect::ListAppend { var, value } => Effect::ListAppend { var, value },
        ResolvedEffect::ListRemoveAt { var, index } => Effect::ListRemoveAt { var, index },
        ResolvedEffect::Dispatch(name) => Effect::Custom(name),
        ResolvedEffect::Schedule {
            action,
            delay_seconds,
        } => Effect::ScheduleAction {
            action,
            delay_seconds,
        },
        ResolvedEffect::ScheduleAt { action, field } => Effect::ScheduleAtAction { action, field },
        ResolvedEffect::Spawn {
            entity_type,
            initial_action,
            store_id_in,
            id,
        } => Effect::SpawnEntity {
            entity_type,
            initial_action,
            store_id_in,
            id,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_schedule_effect_maps_to_schedule_action() {
        let spec = r#"
[automaton]
name = "OAuthToken"
states = ["Active", "Refreshing", "Expired"]
initial = "Active"

[[action]]
name = "Activate"
from = ["Refreshing"]
to = "Active"
effect = ["schedule('Refresh', 2700)"]

[[action]]
name = "Refresh"
from = ["Active"]
to = "Refreshing"
"#;

        let table = TransitionTable::from_ioa_source(spec);
        let rule = table.rules.iter().find(|r| r.name == "Activate").unwrap();

        let has_schedule = rule.effects.iter().any(|e| {
            matches!(
                e,
                Effect::ScheduleAction { action, delay_seconds }
                    if action == "Refresh" && *delay_seconds == 2700
            )
        });
        assert!(
            has_schedule,
            "expected ScheduleAction effect, got: {:?}",
            rule.effects
        );
    }

    #[test]
    fn composite_metadata_is_registered_on_transition_table() {
        let spec = r#"
[automaton]
name = "Repository"
states = ["Active"]
initial = "Active"

[[action]]
name = "IngestPack"
kind = "Composite"
from = ["Active"]
to = "Active"
record_parent_event = false

[[action.cedar_gate]]
principal = "request.principal"
resource = "this"
action = "Repository::IngestPack"

[[action.sub_writes]]
target_entity = "Blob"
action = "Create"
generated_from = "pack_bytes"
"#;

        let table = TransitionTable::from_ioa_source(spec);
        let metadata = table.composite_actions.get("IngestPack").unwrap();

        assert_eq!(
            metadata
                .cedar_gate
                .as_ref()
                .map(|gate| gate.action.as_str()),
            Some("Repository::IngestPack")
        );
        assert!(!metadata.record_parent_event);
        assert_eq!(metadata.sub_writes.len(), 1);
        assert_eq!(metadata.sub_writes[0].target_entity, "Blob");
    }
}

#[cfg(test)]
mod cross_entity_tests {
    use super::*;
    use crate::EvalContext;
    use crate::table::guard::Related;

    #[test]
    fn test_cross_entity_guard_maps_to_cross_entity_state_in() {
        let spec = r#"
[automaton]
name = "Parent"
states = ["Waiting", "Ready"]
initial = "Waiting"

[[action]]
name = "Proceed"
from = ["Waiting"]
to = "Ready"
guard = "empty(child_id) || Child[child_id].status in ['Done']"
"#;

        let table = TransitionTable::from_ioa_source(spec);
        let rule = table.rules.iter().find(|r| r.name == "Proceed").unwrap();

        assert_eq!(rule.from_states, vec!["Waiting".to_string()]);
        assert_eq!(
            rule.guard.to_string(),
            "empty(child_id) || Child[child_id].status in ['Done']"
        );
    }

    #[test]
    fn test_cross_entity_guard_reads_resolved_statuses() {
        let spec = r#"
[automaton]
name = "Parent"
states = ["Waiting", "Ready"]
initial = "Waiting"

[[action]]
name = "Proceed"
from = ["Waiting"]
to = "Ready"
guard = "Child[child_id].status in ['Done']"
"#;
        let table = TransitionTable::from_ioa_source(spec);
        let key = ("Child".to_string(), "child_id".to_string());
        let mut ctx = EvalContext::default();
        assert!(
            !table
                .evaluate_ctx("Waiting", &ctx, "Proceed")
                .unwrap()
                .success
        );
        ctx.related
            .insert(key.clone(), Related::Statuses(vec![Some("Done".into())]));
        assert!(
            table
                .evaluate_ctx("Waiting", &ctx, "Proceed")
                .unwrap()
                .success
        );
        ctx.related
            .insert(key, Related::Statuses(vec![Some("Running".into())]));
        assert!(
            !table
                .evaluate_ctx("Waiting", &ctx, "Proceed")
                .unwrap()
                .success
        );
    }

    #[test]
    fn test_spawn_effect_maps_to_spawn_entity() {
        let spec = r#"
[automaton]
name = "Parent"
states = ["Idle", "Active"]
initial = "Idle"

[[action]]
name = "Start"
from = ["Idle"]
to = "Active"
effect = ["spawn('SubTask', 'Begin', subtask_id)"]
"#;

        let table = TransitionTable::from_ioa_source(spec);
        let rule = table.rules.iter().find(|r| r.name == "Start").unwrap();

        assert!(
            rule.effects.contains(&Effect::SpawnEntity {
                entity_type: "SubTask".into(),
                initial_action: "Begin".into(),
                store_id_in: Some("subtask_id".into()),
                id: None,
            }),
            "expected SpawnEntity effect, got: {:?}",
            rule.effects
        );
    }
}
