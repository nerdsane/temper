//! Model builder: constructs a `TemperModel` directly from I/O Automaton specifications.
//!
//! Uses the shared translation layer in `temper-spec` for guard/effect translation,
//! then converts to verification-specific types. Runtime-only effects (dispatch,
//! schedule, spawn) are filtered out; parameter-driven effects are kept and
//! explored over every candidate parameter value. Cross-entity guards are kept
//! as abstract guards so single-entity checks do not silently treat them as
//! locally enabled.

use std::collections::{BTreeMap, BTreeSet};

use temper_spec::automaton::{ActionKind, Automaton, Initial, ResolvedEffect, translate_actions};
use temper_spec::predicate::{Arg, Expr, Literal, Operand, ParamKind, Set, VarKind};

mod fields;
mod strings;

use fields::read_fields;
use strings::{string_literals, string_params, tracked_strings};

use super::counters::leave_out_unread_counters;
use super::entry::resolve_entry_rules;
use super::types::{
    LivenessKind, ModelEffect, ModelString, ResolvedInvariant, ResolvedLiveness,
    ResolvedTransition, TemperModel,
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

/// Build a `TemperModel` directly from a parsed [`Automaton`], without the
/// counters nothing reads.
pub fn build_model_from_automaton(automaton: &Automaton, max_counter: usize) -> TemperModel {
    let mut model = build_model_tracking(automaton, max_counter, &BTreeSet::new());
    leave_out_unread_counters(automaton, &mut model);
    model
}

/// [`build_model_from_automaton`], also tracking the string variables in
/// `also_tracked`: those another entity's invariant reads through
/// `Type[id].field`, or a reaction copies into a tracked variable.
pub fn build_model_tracking(
    automaton: &Automaton,
    max_counter: usize,
    also_tracked: &BTreeSet<String>,
) -> TemperModel {
    let states = automaton.automaton.states.clone();
    let initial_status = automaton.automaton.initial.clone();

    // Extract initial values from [[state]] declarations.
    let mut initial_counters = BTreeMap::new();
    let mut initial_booleans = BTreeMap::new();
    let mut initial_lists: BTreeMap<String, Vec<ModelString>> = BTreeMap::new();
    let mut counter_bounds = BTreeMap::new();
    let read = read_fields(automaton, also_tracked);

    for sv in automaton.state.iter().filter(|sv| read.contains(&sv.name)) {
        match &sv.initial {
            Initial::Counter(n) => {
                initial_counters.insert(sv.name.clone(), *n);
                counter_bounds.insert(sv.name.clone(), max_counter);
            }
            Initial::Bool(b) => {
                initial_booleans.insert(sv.name.clone(), *b);
            }
            Initial::List(items) => {
                initial_lists.insert(
                    sv.name.clone(),
                    items.iter().cloned().map(Into::into).collect(),
                );
            }
            // Strings are initialized below; ints are not modeled.
            Initial::String(_) | Initial::Int(_) => {}
        }
    }

    let tracked_strings = tracked_strings(automaton, also_tracked);
    let initial_strings: BTreeMap<String, super::types::ModelString> = automaton
        .state
        .iter()
        .filter(|sv| tracked_strings.contains(&sv.name))
        .map(|sv| {
            let value = match &sv.initial {
                Initial::String(text) => text.clone(),
                _ => String::new(),
            };
            (sv.name.clone(), value.into())
        })
        .collect();
    let transitions = resolve_transitions(automaton, &tracked_strings, &read);
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
    // A list is explored at least as long as it starts.
    let mut list_bounds: BTreeMap<String, usize> = initial_lists
        .iter()
        .map(|(name, items)| (name.clone(), max_counter.max(items.len())))
        .collect();
    let invariants = resolve_invariants(automaton);
    let entry_rules = resolve_entry_rules(automaton);
    // A counter, or a list's length, is explored one past every literal a
    // guard or rule compares it with, so a rule about a count passing a
    // threshold is checked at the threshold, not passed because the count
    // never got there.
    let guards = transitions.iter().map(|t| &t.guard);
    for (name, literal) in compared_literals(guards.chain(rule_asserts(automaton))) {
        let bound = counter_bounds.get_mut(&name).or(list_bounds.get_mut(&name));
        if let Some(bound) = bound {
            *bound = (*bound).max(literal.saturating_add(1));
        }
    }
    let liveness = resolve_liveness(automaton);
    let var_kinds = automaton
        .state
        .iter()
        .map(|sv| (sv.name.clone(), sv.var_type.kind()))
        .collect();

    let mut string_inputs = tracked_strings.clone();
    for t in &transitions {
        string_inputs.extend(t.string_params.iter().map(|p| p.name.clone()));
    }
    TemperModel {
        states,
        transitions,
        invariants,
        entry_rules,
        liveness,
        terminal: automaton.automaton.terminal.clone(),
        var_kinds,
        initial_status,
        initial_counters,
        initial_booleans,
        initial_lists,
        string_literals: string_literals(automaton, &string_inputs),
        initial_strings,
        tracked_strings,
        counter_bounds,
        default_max_counter: max_counter,
        list_bounds,
        list_literals: list_literals(automaton),
        limit: None,
    }
}

/// The asserts of the spec's invariants and entry rules: what the model
/// checks beyond the guards.
fn rule_asserts(automaton: &Automaton) -> impl Iterator<Item = &Expr> {
    let invariants = automaton.invariants.iter().map(|inv| &inv.assert);
    invariants.chain(automaton.entry_rules.iter().map(|rule| &rule.assert))
}

/// `'x' in list` literals per list, across every guard and rule.
fn list_literals(automaton: &Automaton) -> BTreeMap<String, BTreeSet<String>> {
    let mut pairs = BTreeSet::new();
    for action in &automaton.actions {
        super::semantics::collect_list_contains_pairs(&action.guard, &mut pairs);
    }
    for assert in rule_asserts(automaton) {
        super::semantics::collect_list_contains_pairs(assert, &mut pairs);
    }
    let mut literals: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (list, value) in pairs {
        literals.entry(list).or_default().insert(value);
    }
    literals
}

/// Translate IOA actions into resolved transitions using the shared translation layer.
fn resolve_transitions(
    automaton: &Automaton,
    tracked_strings: &BTreeSet<String>,
    read: &BTreeSet<String>,
) -> Vec<ResolvedTransition> {
    // `translate_actions` keeps every action but outputs, in order.
    let declared = automaton
        .actions
        .iter()
        .filter(|a| a.kind != ActionKind::Output);
    translate_actions(automaton)
        .into_iter()
        .zip(declared)
        .map(|(a, action)| {
            let effects: Vec<ModelEffect> = a
                .effects
                .into_iter()
                .filter_map(convert_effect)
                .filter(|effect| holds_written(effect, read, tracked_strings))
                .collect();
            let mut params = effect_params(&effects);
            if !automaton.entry_rules.is_empty() {
                // A guard-only input still determines whether entry is legal.
                let reads = action.guard.params_read();
                for param in &action.params {
                    let constraints: Vec<_> = action
                        .constraints
                        .iter()
                        .filter(|c| c.param() == param.name())
                        .collect();
                    if !reads.iter().any(|name| name == param.name()) && constraints.is_empty() {
                        continue;
                    }
                    let kind = constraints
                        .iter()
                        .filter_map(|c| c.field())
                        .find_map(|field| automaton.state.iter().find(|s| s.name == field))
                        .map_or(param.param_type().kind(), |s| s.var_type.kind());
                    let kind = match kind {
                        VarKind::Str => ParamKind::Str,
                        VarKind::Bool => ParamKind::Bool,
                        VarKind::Counter => ParamKind::Count,
                        VarKind::Num | VarKind::List => continue,
                    };
                    params.entry(param.name().to_string()).or_insert(kind);
                }
            }
            let string_params = string_params(action, tracked_strings, &params);
            ResolvedTransition {
                name: a.name,
                from_states: a.from_states,
                to_state: a.to_state,
                guard: if automaton.entry_rules.is_empty() {
                    a.guard
                } else {
                    super::parameters::guard_with_constraints(action)
                },
                effects,
                params,
                string_params,
                constraints: action.constraints.clone(),
            }
        })
        .collect()
}

/// The largest non-negative integer literal each counter, or each list's
/// length, is compared with in `exprs` (`v < 4`, `4 <= v`, `v in [1, 4]`,
/// `len(l) < 4`), by the variable's name.
fn compared_literals<'a>(exprs: impl Iterator<Item = &'a Expr>) -> BTreeMap<String, usize> {
    fn note(var: &str, literal: &Literal, out: &mut BTreeMap<String, usize>) {
        if let Literal::Int(n) = literal
            && let Ok(n) = usize::try_from(*n)
        {
            let largest = out.entry(var.to_string()).or_default();
            *largest = (*largest).max(n);
        }
    }
    fn visit(expr: &Expr, out: &mut BTreeMap<String, usize>) {
        match expr {
            Expr::Not(inner) => visit(inner, out),
            Expr::And(parts) | Expr::Or(parts) => {
                parts.iter().for_each(|part| visit(part, out));
            }
            Expr::Implies(a, b) => {
                visit(a, out);
                visit(b, out);
            }
            Expr::Compare { lhs, rhs, .. } => {
                for (var, literal) in [(lhs, rhs), (rhs, lhs)] {
                    if let (Operand::Var(var) | Operand::Len(var), Operand::Lit(literal)) =
                        (var, literal)
                    {
                        note(var, literal, out);
                    }
                }
            }
            Expr::In {
                value: Operand::Var(var) | Operand::Len(var),
                set: Set::List(items),
                ..
            } => items.iter().for_each(|item| note(var, item, out)),
            _ => {}
        }
    }
    let mut out = BTreeMap::new();
    exprs.for_each(|expr| visit(expr, &mut out));
    out
}

/// Convert a state [`ResolvedEffect`] to the verification [`ModelEffect`];
/// runtime-only effects (dispatch, schedule, spawn) have no model.
fn convert_effect(effect: ResolvedEffect) -> Option<ModelEffect> {
    Some(match effect {
        ResolvedEffect::SetCounter { var, value } => ModelEffect::SetCounter { var, value },
        ResolvedEffect::AddCounter { var, value } => ModelEffect::AddCounter { var, value },
        ResolvedEffect::SubCounter { var, value } => ModelEffect::SubCounter { var, value },
        ResolvedEffect::SetBool { var, value } => ModelEffect::SetBool { var, value },
        ResolvedEffect::SetString { var, value } => ModelEffect::SetString { var, value },
        ResolvedEffect::ListAppend { var, value } => ModelEffect::ListAppend { var, value },
        ResolvedEffect::ListRemoveAt { var, index } => ModelEffect::ListRemoveAt { var, index },
        ResolvedEffect::ListRemove { var, value } => ModelEffect::ListRemove { var, value },
        ResolvedEffect::Dispatch(_)
        | ResolvedEffect::Schedule { .. }
        | ResolvedEffect::ScheduleAt { .. }
        | ResolvedEffect::Spawn { .. } => return None,
    })
}

/// Whether the model holds the field `effect` writes: a tracked string
/// (applying a write to any other is a no-op) or a field in `read`.
fn holds_written(
    effect: &ModelEffect,
    read: &BTreeSet<String>,
    tracked_strings: &BTreeSet<String>,
) -> bool {
    match effect {
        ModelEffect::SetString { var, .. } => tracked_strings.contains(var),
        ModelEffect::SetCounter { var, .. }
        | ModelEffect::AddCounter { var, .. }
        | ModelEffect::SubCounter { var, .. }
        | ModelEffect::SetBool { var, .. }
        | ModelEffect::ListAppend { var, .. }
        | ModelEffect::ListRemove { var, .. }
        | ModelEffect::ListRemoveAt { var, .. } => read.contains(var),
    }
}

/// The kind of every `params.p` the effects read (checked consistent at load).
pub(crate) fn effect_params(effects: &[ModelEffect]) -> BTreeMap<String, ParamKind> {
    let mut params = BTreeMap::new();
    for effect in effects {
        let (arg, kind) = match effect {
            ModelEffect::SetCounter { value, .. }
            | ModelEffect::AddCounter { value, .. }
            | ModelEffect::SubCounter { value, .. } => (value, ParamKind::Count),
            ModelEffect::SetBool { value, .. } => (value, ParamKind::Bool),
            ModelEffect::SetString { .. } => continue,
            ModelEffect::ListAppend { value, .. } | ModelEffect::ListRemove { value, .. } => {
                (value, ParamKind::Str)
            }
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

/// Translate IOA liveness properties into resolved liveness. An `eventually`
/// property is checked on the joint model instead.
fn resolve_liveness(automaton: &Automaton) -> Vec<ResolvedLiveness> {
    automaton
        .liveness
        .iter()
        .filter(|l| l.eventually.is_none())
        .map(|l| {
            let kind = if !l.reaches.is_empty() {
                LivenessKind::ReachesState {
                    from: l.from.clone(),
                    targets: l.reaches.clone(),
                }
            } else if !l.can_reach.is_empty() {
                LivenessKind::CanReach {
                    from: l.from.clone(),
                    targets: l.can_reach.clone(),
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
#[path = "builder_test.rs"]
mod tests;
