//! Model builder: constructs a `TemperModel` directly from I/O Automaton specifications.
//!
//! Uses the shared translation layer in `temper-spec` for guard/effect translation,
//! then converts to verification-specific types. Runtime-only effects (dispatch,
//! schedule, spawn) are filtered out; parameter-driven effects are kept and
//! explored over every candidate parameter value. Cross-entity guards are kept
//! as abstract guards so single-entity checks do not silently treat them as
//! locally enabled.

use std::collections::{BTreeMap, BTreeSet};

use temper_spec::automaton::{
    Automaton, ResolvedEffect, parse_bool_initial, parse_counter_initial_usize, parse_list_initial,
    translate_actions,
};
use temper_spec::predicate::{Arg, Literal, ParamKind, VarKind};

use super::types::{
    LivenessKind, ModelEffect, ResolvedInvariant, ResolvedLiveness, ResolvedTransition, TemperModel,
};

/// Build a `TemperModel` from I/O Automaton TOML source.
///
/// This is the sole entry point. The IOA format has explicit guards and effects,
/// so the `Automaton` is translated directly — no intermediate representation.
///
/// Returns an error if the IOA TOML fails to parse.
pub fn build_model_from_ioa(ioa_toml: &str, max_counter: usize) -> Result<TemperModel, String> {
    let automaton = temper_spec::automaton::parse_automaton(ioa_toml)
        .map_err(|e| format!("failed to parse I/O Automaton TOML: {e}"))?;
    Ok(build_model_from_automaton(&automaton, max_counter))
}

/// Build a `TemperModel` directly from a parsed [`Automaton`].
pub fn build_model_from_automaton(automaton: &Automaton, max_counter: usize) -> TemperModel {
    let states = automaton.automaton.states.clone();
    let initial_status = automaton.automaton.initial.clone();

    // Extract initial values from [[state]] declarations.
    let mut initial_counters = BTreeMap::new();
    let mut initial_booleans = BTreeMap::new();
    let mut initial_lists = BTreeMap::new();
    let mut counter_bounds = BTreeMap::new();

    for sv in &automaton.state {
        match sv.var_type.as_str() {
            "counter" => {
                let init_val = parse_counter_initial_usize(&sv.initial);
                initial_counters.insert(sv.name.clone(), init_val);
                counter_bounds.insert(sv.name.clone(), max_counter);
            }
            "bool" => {
                let init_val = parse_bool_initial(&sv.initial);
                initial_booleans.insert(sv.name.clone(), init_val);
            }
            "list" | "set" => {
                initial_lists.insert(sv.name.clone(), parse_list_initial(&sv.initial));
            }
            _ => {
                // Keep verification robust against partially modeled types.
                // Semantic linting reports unsupported state variable types.
            }
        }
    }

    let transitions = resolve_transitions(automaton);
    // A counter written with a literal larger than the exploration bound is
    // explored up to that literal, so the write is not pruned away.
    for t in &transitions {
        for effect in &t.effects {
            if let ModelEffect::SetCounter {
                var,
                value: Arg::Lit(Literal::Int(n)),
            }
            | ModelEffect::AddCounter {
                var,
                value: Arg::Lit(Literal::Int(n)),
            } = effect
                && let (Some(bound), Ok(n)) = (counter_bounds.get_mut(var), usize::try_from(*n))
            {
                *bound = (*bound).max(n);
            }
        }
    }
    for (name, initial) in &initial_counters {
        if let Some(bound) = counter_bounds.get_mut(name) {
            *bound = (*bound).max(*initial);
        }
    }
    let invariants = resolve_invariants(automaton);
    let liveness = resolve_liveness(automaton);
    let var_kinds = automaton
        .state
        .iter()
        .map(|sv| (sv.name.clone(), VarKind::from_type(&sv.var_type)))
        .collect();

    TemperModel {
        states,
        transitions,
        invariants,
        liveness,
        terminal: automaton.automaton.terminal.clone(),
        var_kinds,
        initial_status,
        initial_counters,
        initial_booleans,
        initial_lists,
        counter_bounds,
        default_max_counter: max_counter,
        list_literals: list_literals(automaton),
    }
}

/// `'x' in list` literals per list, across every guard and invariant.
fn list_literals(automaton: &Automaton) -> BTreeMap<String, BTreeSet<String>> {
    let mut pairs = BTreeSet::new();
    for action in &automaton.actions {
        super::semantics::collect_list_contains_pairs(&action.guard, &mut pairs);
    }
    for inv in &automaton.invariants {
        super::semantics::collect_list_contains_pairs(&inv.assert, &mut pairs);
    }
    let mut literals: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (list, value) in pairs {
        literals.entry(list).or_default().insert(value);
    }
    literals
}

/// Translate IOA actions into resolved transitions using the shared translation layer.
fn resolve_transitions(automaton: &Automaton) -> Vec<ResolvedTransition> {
    translate_actions(automaton)
        .into_iter()
        .map(|a| {
            let effects: Vec<ModelEffect> =
                a.effects.into_iter().filter_map(convert_effect).collect();
            let params = effect_params(&effects);
            ResolvedTransition {
                name: a.name,
                from_states: a.from_states,
                to_state: a.to_state,
                guard: a.guard,
                effects,
                params,
            }
        })
        .collect()
}

/// Convert a state [`ResolvedEffect`] to the verification [`ModelEffect`];
/// runtime-only effects (dispatch, schedule, spawn) have no model.
fn convert_effect(effect: ResolvedEffect) -> Option<ModelEffect> {
    Some(match effect {
        ResolvedEffect::SetCounter { var, value } => ModelEffect::SetCounter { var, value },
        ResolvedEffect::AddCounter { var, value } => ModelEffect::AddCounter { var, value },
        ResolvedEffect::SubCounter { var, value } => ModelEffect::SubCounter { var, value },
        ResolvedEffect::SetBool { var, value } => ModelEffect::SetBool { var, value },
        ResolvedEffect::ListAppend { var, value } => ModelEffect::ListAppend { var, value },
        ResolvedEffect::ListRemoveAt { var, index } => ModelEffect::ListRemoveAt { var, index },
        ResolvedEffect::Dispatch(_)
        | ResolvedEffect::Schedule { .. }
        | ResolvedEffect::ScheduleAt { .. }
        | ResolvedEffect::Spawn { .. } => return None,
    })
}

/// The kind of every `params.p` the effects read (checked consistent at load).
fn effect_params(effects: &[ModelEffect]) -> BTreeMap<String, ParamKind> {
    let mut params = BTreeMap::new();
    for effect in effects {
        let (arg, kind) = match effect {
            ModelEffect::SetCounter { value, .. }
            | ModelEffect::AddCounter { value, .. }
            | ModelEffect::SubCounter { value, .. } => (value, ParamKind::Count),
            ModelEffect::SetBool { value, .. } => (value, ParamKind::Bool),
            ModelEffect::ListAppend { value, .. } => (value, ParamKind::Str),
            ModelEffect::ListRemoveAt { index, .. } => (index, ParamKind::Count),
        };
        if let Arg::Param(name) = arg {
            params.insert(name.clone(), kind);
        }
    }
    params
}

/// Translate IOA invariants into resolved invariants. Status membership
/// (`TypeInvariant`) is checked by every backend separately.
fn resolve_invariants(automaton: &Automaton) -> Vec<ResolvedInvariant> {
    automaton
        .invariants
        .iter()
        .map(|inv| ResolvedInvariant {
            name: inv.name.clone(),
            assert: inv.assert.clone(),
        })
        .collect()
}

/// Translate IOA liveness properties into resolved liveness.
fn resolve_liveness(automaton: &Automaton) -> Vec<ResolvedLiveness> {
    automaton
        .liveness
        .iter()
        .map(|l| {
            let kind = if !l.reaches.is_empty() {
                LivenessKind::ReachesState {
                    from: l.from.clone(),
                    targets: l.reaches.clone(),
                }
            } else if l.has_actions == Some(true) {
                LivenessKind::NoDeadlock {
                    from: l.from.clone(),
                }
            } else {
                // Default: treat as reachability with empty targets (trivially true)
                LivenessKind::ReachesState {
                    from: l.from.clone(),
                    targets: vec![],
                }
            };
            ResolvedLiveness {
                name: l.name.clone(),
                kind,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use stateright::Model;

    const ORDER_IOA: &str = include_str!("../../../../test-fixtures/specs/order.ioa.toml");

    fn build_order_model() -> TemperModel {
        build_model_from_ioa(ORDER_IOA, 2).unwrap()
    }

    #[test]
    fn test_build_model_has_correct_states() {
        let model = build_order_model();
        assert_eq!(model.states.len(), 10);
        assert!(model.states.contains(&"Draft".to_string()));
        assert!(model.states.contains(&"Submitted".to_string()));
        assert!(model.states.contains(&"Confirmed".to_string()));
        assert!(model.states.contains(&"Refunded".to_string()));
    }

    #[test]
    fn test_build_model_initial_state_is_draft() {
        let model = build_order_model();
        let init = model.init_states();
        assert_eq!(init.len(), 1);
        assert_eq!(init[0].status, "Draft");
        assert_eq!(*init[0].counters.get("items").unwrap_or(&99), 0);
    }

    #[test]
    fn test_draft_actions_include_add_item() {
        let model = build_order_model();
        let state = super::super::types::TemperModelState {
            status: "Draft".to_string(),
            counters: BTreeMap::from([("items".to_string(), 0)]),
            booleans: BTreeMap::from([("has_address".to_string(), false)]),
            lists: BTreeMap::new(),
        };
        let mut actions = Vec::new();
        model.actions(&state, &mut actions);
        let names: Vec<&str> = actions.iter().map(|a| a.name.as_str()).collect();
        assert!(
            names.contains(&"AddItem"),
            "Draft state should allow AddItem, got: {names:?}"
        );
    }

    #[test]
    fn test_submitted_does_not_allow_add_item() {
        let model = build_order_model();
        let state = super::super::types::TemperModelState {
            status: "Submitted".to_string(),
            counters: BTreeMap::from([("items".to_string(), 1)]),
            booleans: BTreeMap::from([("has_address".to_string(), true)]),
            lists: BTreeMap::new(),
        };
        let mut actions = Vec::new();
        model.actions(&state, &mut actions);
        let names: Vec<&str> = actions.iter().map(|a| a.name.as_str()).collect();
        assert!(
            !names.contains(&"AddItem"),
            "Submitted state should NOT allow AddItem, got: {names:?}"
        );
    }

    #[test]
    fn test_draft_to_submitted_transition() {
        let model = build_order_model();
        let state = super::super::types::TemperModelState {
            status: "Draft".to_string(),
            counters: BTreeMap::from([("items".to_string(), 1)]),
            booleans: BTreeMap::from([("has_address".to_string(), false)]),
            lists: BTreeMap::new(),
        };
        let action = super::super::types::TemperModelAction {
            name: "SubmitOrder".to_string(),
            target_state: Some("Submitted".to_string()),
            params: BTreeMap::new(),
        };
        let next = model.next_state(&state, action);
        assert!(next.is_some());
        let next = next.unwrap();
        assert_eq!(next.status, "Submitted");
        assert_eq!(*next.counters.get("items").unwrap(), 1);
    }

    #[test]
    fn test_add_item_increments_count() {
        let model = build_order_model();
        let state = super::super::types::TemperModelState {
            status: "Draft".to_string(),
            counters: BTreeMap::from([("items".to_string(), 0)]),
            booleans: BTreeMap::from([("has_address".to_string(), false)]),
            lists: BTreeMap::new(),
        };
        let action = super::super::types::TemperModelAction {
            name: "AddItem".to_string(),
            target_state: None,
            params: BTreeMap::new(),
        };
        let next = model.next_state(&state, action).unwrap();
        assert_eq!(*next.counters.get("items").unwrap(), 1);
        assert_eq!(next.status, "Draft");
    }

    #[test]
    fn test_properties_are_generated() {
        let model = build_order_model();
        let props = model.properties();
        assert!(!props.is_empty(), "Model should have at least one property");
    }

    #[test]
    fn test_counter_invariant_resolved() {
        let model = build_order_model();
        assert!(
            model
                .invariants
                .iter()
                .any(|i| i.assert.to_string().ends_with("=> items > 0")),
            "Should have an `items > 0` invariant"
        );
    }

    #[test]
    fn test_terminal_states_resolved() {
        let model = build_order_model();
        assert!(!model.terminal.is_empty(), "Should have terminal states");
    }

    #[test]
    fn test_cross_entity_guard_is_preserved_as_abstract_guard() {
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
        let model = build_model_from_ioa(spec, 2).unwrap();
        let guard = &model
            .transitions
            .iter()
            .find(|transition| transition.name == "Proceed")
            .expect("Proceed transition")
            .guard;
        assert_eq!(guard.to_string(), "Child[child_id].status in ['Done']");
        let state = model.init_states().remove(0);
        assert!(
            super::super::semantics::guard_may_hold(guard, &model.var_kinds, &state),
            "a related-entity guard may hold (free boolean)"
        );
        assert!(
            !super::super::semantics::evaluate_guard(guard, &model.var_kinds, &state),
            "a related-entity guard is not locally enabled"
        );
    }

    #[test]
    fn debug_resolved_transitions() {
        let model = build_model_from_ioa(ORDER_IOA, 2).unwrap();
        for t in &model.transitions {
            eprintln!(
                "{}: from={:?} to={:?} guard={:?} effects={:?}",
                t.name, t.from_states, t.to_state, t.guard, t.effects
            );
        }
    }

    // --- Compound invariant tests ---------------------------------------

    const COMPOUND_IOA: &str = r#"
[automaton]
name = "Release"
states = ["Planning", "Testing", "Shipped"]
initial = "Planning"

[[state]]
name = "migrations_ok"
type = "bool"
initial = "false"

[[state]]
name = "typecheck_ok"
type = "bool"
initial = "false"

[[state]]
name = "unit_tests_ok"
type = "bool"
initial = "false"

[[action]]
name = "EnterTesting"
kind = "input"
from = ["Planning"]
to = "Testing"

[[action]]
name = "Ship"
kind = "internal"
from = ["Testing"]
to = "Shipped"

[[invariant]]
name = "TestingRequiresAllGates"
assert = "status in ['Testing', 'Shipped'] => migrations_ok && typecheck_ok && unit_tests_ok"

[[invariant]]
name = "EitherReviewer"
assert = "status in ['Shipped'] => migrations_ok || typecheck_ok"
"#;

    #[test]
    fn test_compound_invariants_keep_their_structure() {
        let model = build_model_from_ioa(COMPOUND_IOA, 2).unwrap();
        let text = |name: &str| {
            model
                .invariants
                .iter()
                .find(|i| i.name == name)
                .map(|i| i.assert.to_string())
                .unwrap()
        };
        assert_eq!(
            text("TestingRequiresAllGates"),
            "status in ['Testing', 'Shipped'] => migrations_ok && typecheck_ok && unit_tests_ok"
        );
        assert_eq!(
            text("EitherReviewer"),
            "status in ['Shipped'] => migrations_ok || typecheck_ok"
        );
    }

    const COMPOUND_UNDECLARED_IOA: &str = r#"
[automaton]
name = "Release"
states = ["Planning", "Shipped"]
initial = "Planning"

[[state]]
name = "migrations_ok"
type = "bool"
initial = "false"

[[action]]
name = "Ship"
kind = "internal"
from = ["Planning"]
to = "Shipped"

[[invariant]]
name = "MixedDeclaredUndeclared"
assert = "status in ['Shipped'] => migrations_ok && undeclared_flag"
"#;

    #[test]
    fn test_invariant_over_undeclared_variable_fails_to_load() {
        let err = build_model_from_ioa(COMPOUND_UNDECLARED_IOA, 2)
            .err()
            .expect("undeclared variable must be rejected");
        assert!(
            err.contains("unknown state variable 'undeclared_flag'"),
            "{err}"
        );
    }
}
