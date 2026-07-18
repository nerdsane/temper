use std::collections::{BTreeMap, BTreeSet};

use temper_spec::automaton;

pub(super) fn model_safety_contract_changed(
    current: &automaton::Automaton,
    incoming: &automaton::Automaton,
) -> bool {
    let current_runtime = automaton::compile_runtime_invariants(current);
    let incoming_runtime = automaton::compile_runtime_invariants(incoming);
    let current_has_model_contract = current.invariants.len() > current_runtime.len();
    let incoming_has_model_contract = incoming.invariants.len() > incoming_runtime.len();
    if !current_has_model_contract && !incoming_has_model_contract {
        return false;
    }

    let invariant_signatures = |spec: &automaton::Automaton| {
        spec.invariants
            .iter()
            .map(|invariant| {
                let mut when = invariant.when.clone();
                when.sort();
                (invariant.name.clone(), when, invariant.assert.clone())
            })
            .collect::<std::collections::BTreeSet<_>>()
    };
    if invariant_signatures(current) != invariant_signatures(incoming) {
        return true;
    }

    !model_reachability_semantics(incoming)
        .is_compatible_extension_of(&model_reachability_semantics(current))
}

#[derive(PartialEq)]
struct ModelReachabilitySemantics {
    status_values: Vec<String>,
    initial_status: String,
    state_initials: BTreeMap<String, (String, serde_json::Value)>,
    actions: Vec<ModelActionSemantics>,
    invariant_trigger_states: BTreeSet<String>,
    terminal_states: BTreeSet<String>,
    has_global_invariant: bool,
}

impl ModelReachabilitySemantics {
    fn is_compatible_extension_of(&self, current: &Self) -> bool {
        if self.initial_status != current.initial_status
            || self.state_initials != current.state_initials
            || !current
                .status_values
                .iter()
                .all(|state| self.status_values.contains(state))
        {
            return false;
        }

        let mut incoming_actions = self.actions.iter().collect::<Vec<_>>();
        for current_action in &current.actions {
            let Some(index) = incoming_actions
                .iter()
                .position(|incoming| *incoming == current_action)
            else {
                return false;
            };
            incoming_actions.remove(index);
        }

        let current_names = current
            .actions
            .iter()
            .map(|action| action.name.as_str())
            .collect::<BTreeSet<_>>();
        let mut added_names = BTreeSet::new();
        incoming_actions.into_iter().all(|action| {
            !current_names.contains(action.name.as_str())
                && added_names.insert(action.name.as_str())
                && self.added_action_is_syntactically_safe(action, current)
        })
    }

    fn added_action_is_syntactically_safe(
        &self,
        action: &ModelActionSemantics,
        current: &Self,
    ) -> bool {
        if self.has_global_invariant
            || !action.effects.is_empty()
            || action.from_states.is_empty()
            || !action
                .from_states
                .iter()
                .all(|state| current.status_values.contains(state))
            || action
                .from_states
                .iter()
                .any(|state| self.terminal_states.contains(state))
        {
            return false;
        }

        let Some(target) = action.to_state.as_ref() else {
            return true;
        };
        if action.from_states.iter().all(|source| source == target) {
            return true;
        }

        self.status_values.contains(target)
            && !current.status_values.contains(target)
            && !self.has_global_invariant
            && !self.invariant_trigger_states.contains(target)
    }
}

#[derive(PartialEq)]
struct ModelActionSemantics {
    name: String,
    from_states: Vec<String>,
    to_state: Option<String>,
    guard: automaton::ResolvedGuard,
    effects: Vec<automaton::ResolvedEffect>,
}

fn model_reachability_semantics(spec: &automaton::Automaton) -> ModelReachabilitySemantics {
    let states = spec
        .state
        .iter()
        .filter(|state| matches!(state.var_type.as_str(), "bool" | "counter" | "list" | "set"))
        .map(|state| {
            (
                state.name.clone(),
                (
                    state.var_type.clone(),
                    automaton::parse_var_initial_json(&state.var_type, &state.initial),
                ),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let actions = automaton::translate_actions(spec)
        .into_iter()
        .map(|action| ModelActionSemantics {
            name: action.name,
            from_states: action.from_states,
            to_state: action.to_state,
            guard: normalized_model_guard(action.guard),
            effects: action.effects,
        })
        .collect::<Vec<_>>();

    let mut status_values = spec.automaton.states.clone();
    status_values.sort();
    let invariant_trigger_states = spec
        .invariants
        .iter()
        .flat_map(|invariant| invariant.when.iter().cloned())
        .collect();
    let terminal_states = spec
        .invariants
        .iter()
        .filter(|invariant| {
            matches!(
                automaton::parse_assert_expr(&invariant.assert),
                Some(automaton::ParsedAssert::NoFurtherTransitions)
            )
        })
        .flat_map(|invariant| invariant.when.iter().cloned())
        .collect();
    ModelReachabilitySemantics {
        status_values,
        initial_status: spec.automaton.initial.clone(),
        state_initials: states,
        actions,
        invariant_trigger_states,
        terminal_states,
        has_global_invariant: spec
            .invariants
            .iter()
            .any(|invariant| invariant.when.is_empty()),
    }
}

fn normalized_model_guard(guard: automaton::ResolvedGuard) -> automaton::ResolvedGuard {
    match guard {
        automaton::ResolvedGuard::CrossEntityState {
            entity_type,
            entity_id_source,
            required_status,
            forbidden_status,
            required: _,
        } => automaton::ResolvedGuard::CrossEntityState {
            entity_type,
            entity_id_source,
            required_status,
            forbidden_status,
            required: false,
        },
        automaton::ResolvedGuard::And(guards) => {
            automaton::ResolvedGuard::And(guards.into_iter().map(normalized_model_guard).collect())
        }
        other => other,
    }
}
