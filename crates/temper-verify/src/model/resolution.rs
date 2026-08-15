//! Translation from parsed IOA declarations into verifier model metadata.

use temper_spec::automaton::{
    Automaton, ParsedAssert, ResolvedEffect, ResolvedGuard, parse_assert_expr, translate_actions,
};

use super::types::{
    InvariantKind, LivenessKind, ModelEffect, ModelGuard, ResolvedInvariant, ResolvedLiveness,
    ResolvedTransition,
};

pub(super) fn resolve_transitions(automaton: &Automaton) -> Vec<ResolvedTransition> {
    let reference_metadata = super::reference_contract::ReferenceModelMetadata::new(automaton);
    translate_actions(automaton)
        .into_iter()
        .map(|action| {
            let mut effects = action
                .effects
                .into_iter()
                .filter(|effect| effect.is_verifiable())
                .map(convert_effect)
                .collect::<Vec<_>>();
            reference_metadata.augment_effects(&action.name, &mut effects);
            ResolvedTransition {
                name: action.name,
                from_states: action.from_states,
                to_state: action.to_state,
                guard: convert_guard(action.guard),
                effects,
            }
        })
        .collect()
}

fn convert_guard(guard: ResolvedGuard) -> ModelGuard {
    match guard {
        ResolvedGuard::Always => ModelGuard::Always,
        ResolvedGuard::StateIn(values) => ModelGuard::StateIn(values),
        ResolvedGuard::CounterMin { var, min } => ModelGuard::CounterMin { var, min },
        ResolvedGuard::CounterMax { var, max } => ModelGuard::CounterMax { var, max },
        ResolvedGuard::BoolTrue(var) => ModelGuard::BoolTrue(var),
        ResolvedGuard::BoolFalse(var) => ModelGuard::BoolFalse(var),
        ResolvedGuard::ListContains { var, value } => ModelGuard::ListContains { var, value },
        ResolvedGuard::ListLengthMin { var, min } => ModelGuard::ListLengthMin { var, min },
        ResolvedGuard::CrossEntityState {
            entity_type,
            entity_id_source,
            required_status,
            forbidden_status,
            required: _,
        } => ModelGuard::CrossEntityState {
            entity_type,
            entity_id_source,
            required_status,
            forbidden_status,
        },
        ResolvedGuard::ReferenceEquals { reference, param } => {
            ModelGuard::ReferenceEquals { reference, param }
        }
        ResolvedGuard::And(guards) => {
            ModelGuard::And(guards.into_iter().map(convert_guard).collect())
        }
    }
}

fn convert_effect(effect: ResolvedEffect) -> ModelEffect {
    match effect {
        ResolvedEffect::IncrementCounter(var) => ModelEffect::IncrementCounter(var),
        ResolvedEffect::DecrementCounter(var) => ModelEffect::DecrementCounter(var),
        ResolvedEffect::SetBool { var, value } => ModelEffect::SetBool { var, value },
        ResolvedEffect::ListAppend(var) => ModelEffect::ListAppend(var),
        ResolvedEffect::ListRemoveAt(var) => ModelEffect::ListRemoveAt(var),
        ResolvedEffect::Emit(_)
        | ResolvedEffect::SetCounterFromParam { .. }
        | ResolvedEffect::Trigger(_)
        | ResolvedEffect::IncrementCounterByParam { .. }
        | ResolvedEffect::DecrementCounterByParam { .. }
        | ResolvedEffect::Schedule { .. }
        | ResolvedEffect::ScheduleAt { .. }
        | ResolvedEffect::Spawn { .. } => {
            unreachable!("runtime-only effect should have been filtered")
        }
    }
}

pub(super) fn resolve_invariants(automaton: &Automaton) -> Vec<ResolvedInvariant> {
    let mut result = vec![ResolvedInvariant {
        name: "TypeInvariant".to_string(),
        trigger_states: vec![],
        required_states: vec![],
        kind: InvariantKind::StatusInSet,
    }];
    let bool_names = automaton
        .state
        .iter()
        .filter(|state| state.var_type == "bool")
        .map(|state| state.name.as_str())
        .collect::<Vec<_>>();
    for invariant in &automaton.invariants {
        let expression = invariant.assert.trim();
        let kind = parse_assert_expr(expression)
            .and_then(|parsed| try_translate(&parsed, &bool_names))
            .unwrap_or_else(|| InvariantKind::Unverifiable {
                expression: expression.to_string(),
            });
        result.push(ResolvedInvariant {
            name: invariant.name.clone(),
            trigger_states: invariant.when.clone(),
            required_states: vec![],
            kind,
        });
    }
    result
}

fn try_translate(parsed: &ParsedAssert, bool_names: &[&str]) -> Option<InvariantKind> {
    match parsed {
        ParsedAssert::CounterPositive { var } => {
            Some(InvariantKind::CounterPositive { var: var.clone() })
        }
        ParsedAssert::NoFurtherTransitions => Some(InvariantKind::NoFurtherTransitions),
        ParsedAssert::NeverState { state } => Some(InvariantKind::NeverState {
            state: state.clone(),
        }),
        ParsedAssert::CounterCompare { var, op, value } => Some(InvariantKind::CounterCompare {
            var: var.clone(),
            op: op.clone(),
            value: *value,
        }),
        ParsedAssert::BoolRequired { var, expect } if bool_names.contains(&var.as_str()) => {
            Some(InvariantKind::BoolRequired {
                var: var.clone(),
                expect: *expect,
            })
        }
        ParsedAssert::BoolRequired { .. } | ParsedAssert::OrderingConstraint { .. } => None,
        ParsedAssert::And(parts) => parts
            .iter()
            .map(|part| try_translate(part, bool_names))
            .collect::<Option<Vec<_>>>()
            .map(InvariantKind::And),
        ParsedAssert::Or(parts) => parts
            .iter()
            .map(|part| try_translate(part, bool_names))
            .collect::<Option<Vec<_>>>()
            .map(InvariantKind::Or),
    }
}

pub(super) fn resolve_liveness(automaton: &Automaton) -> Vec<ResolvedLiveness> {
    automaton
        .liveness
        .iter()
        .map(|liveness| {
            let kind = if !liveness.reaches.is_empty() {
                LivenessKind::ReachesState {
                    from: liveness.from.clone(),
                    targets: liveness.reaches.clone(),
                }
            } else if liveness.has_actions == Some(true) {
                LivenessKind::NoDeadlock {
                    from: liveness.from.clone(),
                }
            } else {
                LivenessKind::ReachesState {
                    from: liveness.from.clone(),
                    targets: vec![],
                }
            };
            ResolvedLiveness {
                name: liveness.name.clone(),
                kind,
            }
        })
        .collect()
}
