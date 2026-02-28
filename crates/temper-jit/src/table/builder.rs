//! TransitionTable constructors.
//!
//! Builds transition tables from I/O Automaton specifications. The IOA format
//! has explicit `from`, `to`, `guard` fields — no inference needed.

use temper_spec::automaton::{self, Automaton};

use super::types::{Effect, Guard, TransitionRule, TransitionTable};

impl TransitionTable {
    /// Build a TransitionTable from I/O Automaton TOML source.
    ///
    /// This is the primary constructor for production use. The IOA format
    /// has explicit guards and effects — no `CanXxx` predicate inference.
    pub fn from_ioa_source(ioa_toml: &str) -> Self {
        let automaton =
            automaton::parse_automaton(ioa_toml).expect("failed to parse I/O Automaton TOML");
        Self::from_automaton(&automaton)
    }

    /// Build a TransitionTable directly from a parsed [`Automaton`].
    ///
    /// Each action becomes a [`TransitionRule`] with guards and effects
    /// derived from the IOA specification. Output actions are skipped
    /// (they don't transition state).
    pub fn from_automaton(automaton: &Automaton) -> Self {
        // Collect counter variable names from the spec's [[state]] declarations.
        let counter_vars: Vec<String> = automaton
            .state
            .iter()
            .filter(|s| s.var_type == "counter")
            .map(|s| s.name.clone())
            .collect();

        let rules: Vec<TransitionRule> = automaton
            .actions
            .iter()
            .filter(|a| a.kind != "output")
            .map(|a| {
                // Build guards from IOA action fields
                let mut guards = vec![];
                if !a.from.is_empty() {
                    guards.push(Guard::StateIn(a.from.clone()));
                }
                for g in &a.guard {
                    match g {
                        automaton::Guard::StateIn { values } => {
                            guards.push(Guard::StateIn(values.clone()));
                        }
                        automaton::Guard::MinCount { var, min } => {
                            guards.push(Guard::CounterMin {
                                var: var.clone(),
                                min: *min,
                            });
                        }
                        automaton::Guard::MaxCount { var, max } => {
                            guards.push(Guard::CounterMax {
                                var: var.clone(),
                                max: *max,
                            });
                        }
                        automaton::Guard::IsTrue { var } => {
                            guards.push(Guard::BoolTrue(var.clone()));
                        }
                        automaton::Guard::ListContains { var, value } => {
                            guards.push(Guard::ListContains {
                                var: var.clone(),
                                value: value.clone(),
                            });
                        }
                        automaton::Guard::ListLengthMin { var, min } => {
                            guards.push(Guard::ListLengthMin {
                                var: var.clone(),
                                min: *min,
                            });
                        }
                        automaton::Guard::CrossEntityState {
                            entity_type,
                            entity_id_source,
                            required_status,
                        } => {
                            guards.push(Guard::CrossEntityStateIn {
                                entity_type: entity_type.clone(),
                                entity_id_source: entity_id_source.clone(),
                                required_status: required_status.clone(),
                            });
                        }
                    }
                }

                let guard = match guards.len() {
                    0 => Guard::Always,
                    1 => guards.into_iter().next().unwrap(), // ci-ok: len() == 1
                    _ => Guard::And(guards),
                };

                // Build effects from IOA action fields
                let mut effects = vec![];
                if let Some(ref to) = a.to {
                    effects.push(Effect::SetState(to.clone()));
                }

                // Prefer IOA effect declarations when present.
                if !a.effect.is_empty() {
                    for e in &a.effect {
                        match e {
                            automaton::Effect::Increment { var } => {
                                effects.push(Effect::IncrementCounter(var.clone()));
                            }
                            automaton::Effect::Decrement { var } => {
                                effects.push(Effect::DecrementCounter(var.clone()));
                            }
                            automaton::Effect::SetBool { var, value } => {
                                effects.push(Effect::SetBool {
                                    var: var.clone(),
                                    value: *value,
                                });
                            }
                            automaton::Effect::Emit { event } => {
                                effects.push(Effect::EmitEvent(event.clone()));
                            }
                            automaton::Effect::ListAppend { var } => {
                                effects.push(Effect::ListAppend(var.clone()));
                            }
                            automaton::Effect::ListRemoveAt { var } => {
                                effects.push(Effect::ListRemoveAt(var.clone()));
                            }
                            automaton::Effect::Trigger { name } => {
                                effects.push(Effect::Custom(name.clone()));
                            }
                            automaton::Effect::Schedule {
                                action,
                                delay_seconds,
                            } => {
                                effects.push(Effect::ScheduleAction {
                                    action: action.clone(),
                                    delay_seconds: *delay_seconds,
                                });
                            }
                            automaton::Effect::Spawn {
                                entity_type,
                                entity_id_source,
                                initial_action,
                                store_id_in,
                            } => {
                                effects.push(Effect::SpawnEntity {
                                    entity_type: entity_type.clone(),
                                    entity_id_source: entity_id_source.clone(),
                                    initial_action: initial_action.clone(),
                                    store_id_in: store_id_in.clone(),
                                });
                            }
                        }
                    }
                } else {
                    // Fallback: infer item effects from action name convention.
                    let name_lower = a.name.to_lowercase();
                    if name_lower.contains("additem") || name_lower.contains("add_item") {
                        effects.push(Effect::IncrementItems);
                        for var in &counter_vars {
                            if var != "items" {
                                effects.push(Effect::IncrementCounter(var.clone()));
                            }
                        }
                    } else if name_lower.contains("removeitem")
                        || name_lower.contains("remove_item")
                    {
                        effects.push(Effect::DecrementItems);
                        for var in &counter_vars {
                            if var != "items" {
                                effects.push(Effect::DecrementCounter(var.clone()));
                            }
                        }
                    }
                }

                effects.push(Effect::EmitEvent(a.name.clone()));

                TransitionRule {
                    name: a.name.clone(),
                    from_states: a.from.clone(),
                    to_state: a.to.clone(),
                    guard,
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

        TransitionTable {
            entity_name: automaton.automaton.name.clone(),
            states: automaton.automaton.states.clone(),
            initial_state: automaton.automaton.initial.clone(),
            rules,
            rule_index,
        }
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
effect = [{ type = "schedule", action = "Refresh", delay_seconds = 2700 }]
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
}

#[cfg(test)]
mod cross_entity_tests {
    use super::*;
    use crate::EvalContext;

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
guard = [{ type = "cross_entity_state", entity_type = "Child", entity_id_source = "child_id", required_status = ["Done"] }]
"#;

        let table = TransitionTable::from_ioa_source(spec);
        let rule = table.rules.iter().find(|r| r.name == "Proceed").unwrap();

        // Guard should be And([StateIn, CrossEntityStateIn]) since from=["Waiting"] + guard
        let is_cross = match &rule.guard {
            Guard::CrossEntityStateIn {
                entity_type,
                entity_id_source,
                required_status,
            } => {
                entity_type == "Child"
                    && entity_id_source == "child_id"
                    && required_status == &vec!["Done".to_string()]
            }
            Guard::And(guards) => guards.iter().any(|g| {
                matches!(
                    g,
                    Guard::CrossEntityStateIn { entity_type, entity_id_source, required_status }
                        if entity_type == "Child"
                            && entity_id_source == "child_id"
                            && required_status == &vec!["Done".to_string()]
                )
            }),
            _ => false,
        };
        assert!(
            is_cross,
            "expected CrossEntityStateIn guard, got: {:?}",
            rule.guard
        );
    }

    #[test]
    fn test_cross_entity_guard_check_with_boolean() {
        let guard = Guard::CrossEntityStateIn {
            entity_type: "Child".to_string(),
            entity_id_source: "child_id".to_string(),
            required_status: vec!["Done".to_string()],
        };

        let mut ctx = EvalContext::default();
        // Without the boolean set, guard should fail
        assert!(!guard.check("Waiting", &ctx));

        // With __xref boolean set to true, guard should pass
        ctx.booleans
            .insert("__xref:Child:child_id".to_string(), true);
        assert!(guard.check("Waiting", &ctx));

        // With __xref boolean set to false, guard should fail
        ctx.booleans
            .insert("__xref:Child:child_id".to_string(), false);
        assert!(!guard.check("Waiting", &ctx));
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
effect = [{ type = "spawn", entity_type = "SubTask", entity_id_source = "{uuid}", initial_action = "Begin", store_id_in = "subtask_id" }]
"#;

        let table = TransitionTable::from_ioa_source(spec);
        let rule = table.rules.iter().find(|r| r.name == "Start").unwrap();

        let has_spawn = rule.effects.iter().any(|e| {
            matches!(
                e,
                Effect::SpawnEntity {
                    entity_type,
                    entity_id_source,
                    initial_action,
                    store_id_in,
                } if entity_type == "SubTask"
                    && entity_id_source == "{uuid}"
                    && initial_action.as_deref() == Some("Begin")
                    && store_id_in.as_deref() == Some("subtask_id")
            )
        });
        assert!(
            has_spawn,
            "expected SpawnEntity effect, got: {:?}",
            rule.effects
        );
    }
}
