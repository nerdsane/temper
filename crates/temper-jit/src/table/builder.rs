//! TransitionTable constructors.
//!
//! Builds transition tables from TLA+ `StateMachine` specifications or
//! raw TLA+ source with full guard resolution.

use temper_spec::tlaplus::StateMachine;

use super::types::{Effect, Guard, TransitionRule, TransitionTable};

impl TransitionTable {
    /// Build a [`TransitionTable`] from a TLA+ [`StateMachine`] specification.
    ///
    /// Each [`Transition`](temper_spec::tlaplus::Transition) in the spec is
    /// converted into a [`TransitionRule`] with:
    /// - A `StateIn` guard derived from `from_states`.
    /// - A `SetState` effect derived from `to_state`.
    /// - An `EmitEvent` effect for every transition (using the action name).
    pub fn from_state_machine(sm: &StateMachine) -> Self {
        let rules = sm
            .transitions
            .iter()
            .map(|t| {
                // Build the guard --------------------------------------------------
                let guard = if t.from_states.is_empty() {
                    Guard::Always
                } else {
                    Guard::StateIn(t.from_states.clone())
                };

                // Build effects ----------------------------------------------------
                let mut effects: Vec<Effect> = Vec::new();

                if let Some(ref to) = t.to_state {
                    effects.push(Effect::SetState(to.clone()));
                }

                // Derive additional effects from the raw effect expression.
                let expr = t.effect_expr.to_lowercase();
                if expr.contains("items' = items \\union") || expr.contains("items' = items \\cup") {
                    effects.push(Effect::IncrementItems);
                }
                if expr.contains("items' = items \\") && expr.contains("\\{") {
                    // set difference pattern: items' = items \ {item}
                    // already handled below
                }
                if expr.contains("items' = items \\") && !expr.contains("union") && !expr.contains("cup") {
                    effects.push(Effect::DecrementItems);
                }

                // Always emit an event named after the action.
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

        // Determine initial state: first state in the list, or "Draft" as fallback.
        let initial_state = sm
            .states
            .first()
            .cloned()
            .unwrap_or_else(|| "Draft".to_string());

        TransitionTable {
            entity_name: sm.module_name.clone(),
            states: sm.states.clone(),
            initial_state,
            rules,
        }
    }

    /// Build a TransitionTable from raw TLA+ source with full guard resolution.
    ///
    /// This resolves `CanXxx` predicates by parsing their definitions from the
    /// source, producing correct `from_states` and `requires_items` constraints.
    /// This is the constructor that should be used in production -- it matches
    /// what the Stateright model checker and DST simulation verify.
    pub fn from_tla_source(tla_source: &str) -> Self {
        let model: temper_verify::TemperModel = temper_verify::build_model_from_tla(tla_source, 3);

        // Build transition rules directly from the verified model's resolved transitions.
        // These have correct from_states and requires_items from CanXxx guard resolution.
        let rules: Vec<TransitionRule> = model.transitions.iter().map(|rt| {
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

            // Build guard from resolved constraints
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
        }).collect();

        let initial_state = if model.states.contains(&"Draft".to_string()) {
            "Draft".to_string()
        } else {
            model.states.first().cloned().unwrap_or_default()
        };

        TransitionTable {
            entity_name: "Entity".to_string(),
            states: model.states.clone(),
            initial_state,
            rules,
        }
    }
}
