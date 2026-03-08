//! TransitionTable constructors.
//!
//! Builds transition tables from I/O Automaton specifications using the shared
//! translation layer in `temper-spec`. The shared layer eliminates duplicated
//! guard/effect translation logic between JIT and verification paths.

use temper_spec::automaton::{self, Automaton, ResolvedEffect, ResolvedGuard, translate_actions};

use super::types::{Effect, Guard, TransitionRule, TransitionTable};

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
                let guard = convert_guard(a.guard);

                let mut effects = Vec::new();
                if let Some(ref to) = a.to_state {
                    effects.push(Effect::SetState(to.clone()));
                }
                for e in a.effects {
                    effects.push(convert_effect(e));
                }
                effects.push(Effect::EmitEvent(a.name.clone()));

                TransitionRule {
                    name: a.name,
                    from_states: a.from_states,
                    to_state: a.to_state,
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

/// Convert a shared [`ResolvedGuard`] to the JIT [`Guard`] type.
fn convert_guard(guard: ResolvedGuard) -> Guard {
    match guard {
        ResolvedGuard::Always => Guard::Always,
        ResolvedGuard::StateIn(values) => Guard::StateIn(values),
        ResolvedGuard::CounterMin { var, min } => Guard::CounterMin { var, min },
        ResolvedGuard::CounterMax { var, max } => Guard::CounterMax { var, max },
        ResolvedGuard::BoolTrue(var) => Guard::BoolTrue(var),
        ResolvedGuard::ListContains { var, value } => Guard::ListContains { var, value },
        ResolvedGuard::ListLengthMin { var, min } => Guard::ListLengthMin { var, min },
        ResolvedGuard::CrossEntityState {
            entity_type,
            entity_id_source,
            required_status,
        } => Guard::CrossEntityStateIn {
            entity_type,
            entity_id_source,
            required_status,
        },
        ResolvedGuard::And(guards) => Guard::And(guards.into_iter().map(convert_guard).collect()),
    }
}

/// Convert a shared [`ResolvedEffect`] to the JIT [`Effect`] type.
fn convert_effect(effect: ResolvedEffect) -> Effect {
    match effect {
        ResolvedEffect::IncrementCounter(ref var) if var == "items" => Effect::IncrementItems,
        ResolvedEffect::DecrementCounter(ref var) if var == "items" => Effect::DecrementItems,
        ResolvedEffect::IncrementCounter(var) => Effect::IncrementCounter(var),
        ResolvedEffect::DecrementCounter(var) => Effect::DecrementCounter(var),
        ResolvedEffect::SetBool { var, value } => Effect::SetBool { var, value },
        ResolvedEffect::ListAppend(var) => Effect::ListAppend(var),
        ResolvedEffect::ListRemoveAt(var) => Effect::ListRemoveAt(var),
        ResolvedEffect::Emit(event) => Effect::EmitEvent(event),
        ResolvedEffect::Trigger(name) => Effect::Custom(name),
        ResolvedEffect::Schedule {
            action,
            delay_seconds,
        } => Effect::ScheduleAction {
            action,
            delay_seconds,
        },
        ResolvedEffect::Spawn {
            entity_type,
            entity_id_source,
            initial_action,
            store_id_in,
        } => Effect::SpawnEntity {
            entity_type,
            entity_id_source,
            initial_action,
            store_id_in,
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
