use std::collections::BTreeMap;

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

    model_reachability_semantics(current) != model_reachability_semantics(incoming)
}

#[derive(PartialEq)]
struct ModelReachabilitySemantics {
    status_values: Vec<String>,
    initial_status: String,
    state_initials: BTreeMap<String, (String, serde_json::Value)>,
    actions: Vec<ModelActionSemantics>,
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
        .map(|action| {
            let effects = action
                .effects
                .into_iter()
                .filter(automaton::ResolvedEffect::is_verifiable)
                .collect::<Vec<_>>();
            ModelActionSemantics {
                name: action.name,
                from_states: action.from_states,
                to_state: action.to_state,
                guard: normalized_model_guard(action.guard),
                effects,
            }
        })
        .collect::<Vec<_>>();

    let mut status_values = spec.automaton.states.clone();
    status_values.sort();
    ModelReachabilitySemantics {
        status_values,
        initial_status: spec.automaton.initial.clone(),
        state_initials: states,
        actions,
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
