//! TransitionTable constructors.
//!
//! Builds transition tables from I/O Automaton specifications. The IOA format
//! has explicit `from`, `to`, `guard` fields — no inference needed.

use temper_spec::automaton::{self, Automaton};
use temper_spec::tlaplus::StateMachine;

use super::types::{Effect, Guard, TransitionRule, TransitionTable};

impl TransitionTable {
    /// Build a TransitionTable from I/O Automaton TOML source.
    ///
    /// This is the primary constructor for production use. The IOA format
    /// has explicit guards and effects — no `CanXxx` predicate inference.
    pub fn from_ioa_source(ioa_toml: &str) -> Self {
        let automaton = automaton::parse_automaton(ioa_toml)
            .expect("failed to parse I/O Automaton TOML");
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

        let rules = automaton
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
                        automaton::Guard::MinCount { var, min } => {
                            guards.push(Guard::CounterMin {
                                var: var.clone(),
                                min: *min,
                            });
                        }
                        automaton::Guard::IsTrue { var } => {
                            guards.push(Guard::BoolTrue(var.clone()));
                        }
                        _ => {}
                    }
                }

                let guard = match guards.len() {
                    0 => Guard::Always,
                    1 => guards.into_iter().next().unwrap(),
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
                        }
                    }
                } else {
                    // Fallback: infer item effects from action name convention.
                    // Increment/decrement all counter state vars declared in the spec.
                    let name_lower = a.name.to_lowercase();
                    if name_lower.contains("additem") || name_lower.contains("add_item") {
                        effects.push(Effect::IncrementItems);
                        for var in &counter_vars {
                            if var != "items" {
                                effects.push(Effect::IncrementCounter(var.clone()));
                            }
                        }
                    } else if name_lower.contains("removeitem") || name_lower.contains("remove_item") {
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

        TransitionTable {
            entity_name: automaton.automaton.name.clone(),
            states: automaton.automaton.states.clone(),
            initial_state: automaton.automaton.initial.clone(),
            rules,
        }
    }

    /// Build a [`TransitionTable`] from a legacy [`StateMachine`] specification.
    ///
    /// Parses `effect_expr` strings to infer item effects. Does NOT resolve
    /// `CanXxx` guard predicates — use [`from_ioa_source()`](Self::from_ioa_source) instead.
    #[deprecated(note = "use TransitionTable::from_ioa_source() — from_state_machine misses CanXxx guard resolution")]
    pub fn from_state_machine(sm: &StateMachine) -> Self {
        let rules = sm
            .transitions
            .iter()
            .map(|t| {
                let guard = if t.from_states.is_empty() {
                    Guard::Always
                } else {
                    Guard::StateIn(t.from_states.clone())
                };

                let mut effects: Vec<Effect> = Vec::new();
                if let Some(ref to) = t.to_state {
                    effects.push(Effect::SetState(to.clone()));
                }

                let expr = t.effect_expr.to_lowercase();
                if expr.contains("items' = items \\union")
                    || expr.contains("items' = items \\cup")
                {
                    effects.push(Effect::IncrementItems);
                }
                if expr.contains("items' = items \\")
                    && !expr.contains("union")
                    && !expr.contains("cup")
                {
                    effects.push(Effect::DecrementItems);
                }

                effects.push(Effect::EmitEvent(t.name.clone()));

                TransitionRule {
                    name: t.name.clone(),
                    from_states: t.from_states.clone(),
                    to_state: t.to_state.clone(),
                    guard,
                    effects,
                }
            })
            .collect();

        let initial_state = sm
            .states
            .first()
            .cloned()
            .unwrap_or_default();

        TransitionTable {
            entity_name: sm.module_name.clone(),
            states: sm.states.clone(),
            initial_state,
            rules,
        }
    }
}

// Legacy TLA+ builder — only needed by tests.
#[cfg(test)]
impl TransitionTable {
    /// Build from raw TLA+ source via temper-verify model resolution.
    /// Only available in tests (avoids pulling stateright/proptest into production).
    pub fn from_tla_source(tla_source: &str) -> Self {
        let model: temper_verify::TemperModel =
            temper_verify::build_model_from_tla(tla_source, 3);
        Self::from_verified_model(&model)
    }

    fn from_verified_model(model: &temper_verify::TemperModel) -> Self {
        let rules: Vec<TransitionRule> = model
            .transitions
            .iter()
            .map(|rt| {
                let mut effects: Vec<Effect> = Vec::new();
                if let Some(ref target) = rt.to_state {
                    effects.push(Effect::SetState(target.clone()));
                }
                if rt.is_add_item {
                    effects.push(Effect::IncrementItems);
                }
                if rt.modifies_items && !rt.is_add_item {
                    effects.push(Effect::DecrementItems);
                }
                effects.push(Effect::EmitEvent(rt.name.clone()));

                let mut guards = vec![];
                if !rt.from_states.is_empty() {
                    guards.push(Guard::StateIn(rt.from_states.clone()));
                }
                if rt.requires_items {
                    guards.push(Guard::ItemCountMin(1));
                }

                let guard = match guards.len() {
                    0 => Guard::Always,
                    1 => guards.into_iter().next().unwrap(),
                    _ => Guard::And(guards),
                };

                TransitionRule {
                    name: rt.name.clone(),
                    from_states: rt.from_states.clone(),
                    to_state: rt.to_state.clone(),
                    guard,
                    effects,
                }
            })
            .collect();

        let initial_state = model.states.first().cloned().unwrap_or_default();

        TransitionTable {
            entity_name: "Entity".to_string(),
            states: model.states.clone(),
            initial_state,
            rules,
        }
    }
}
