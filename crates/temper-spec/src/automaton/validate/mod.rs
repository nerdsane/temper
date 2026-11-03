//! Checks a parsed spec must pass before it is installed: declared states,
//! predicates and effects, state timeouts, triggers, webhooks, vectors and
//! the statechart labels.

mod carries;
mod groups;
mod triggers;
mod webhooks;

#[cfg(test)]
#[path = "acts_for_test.rs"]
mod acts_for_test;
#[cfg(test)]
#[path = "entry_rules_test.rs"]
mod entry_rules_test;

use super::parser::AutomatonParseError;
use super::types::*;

pub(super) fn validate(automaton: &Automaton) -> Result<(), AutomatonParseError> {
    // 1. Initial state must be in the states list.
    if !automaton
        .automaton
        .states
        .contains(&automaton.automaton.initial)
    {
        return Err(AutomatonParseError::Validation(format!(
            "initial state '{}' is not in states list",
            automaton.automaton.initial
        )));
    }

    // A principal record's status reaches Cedar as `principal.<name>`. Every
    // attribute the kernel puts on a principal starts lowercase (`id`, `role`,
    // `agentTypeVerified`, `actingFor<Type>`), so a name that starts uppercase
    // can never stand in for one of them.
    let name = &automaton.automaton.name;
    if automaton.automaton.principal_record && !name.starts_with(|c: char| c.is_ascii_uppercase()) {
        return Err(AutomatonParseError::Validation(format!(
            "principal_record type '{name}' must start with an uppercase letter: Cedar reads it \
             as principal.{name}, and every attribute the kernel sets on a principal starts \
             lowercase, so only such a name cannot stand in for one"
        )));
    }
    if let Some(field) = &automaton.automaton.principal_record_set {
        if !automaton.automaton.principal_record {
            return Err(AutomatonParseError::Validation(format!(
                "principal_record_set = '{field}' needs principal_record = true: only a \
                 principal record's fields reach the principal"
            )));
        }
        let declared = automaton.state.iter().find(|var| &var.name == field);
        if declared.map(|var| var.var_type) != Some(VarType::List) {
            return Err(AutomatonParseError::Validation(format!(
                "principal_record_set = '{field}' must name a list state variable of \
                 '{name}' (a [[state]] with type = \"list\"), which Cedar reads as the set \
                 principal.{name}_{field}"
            )));
        }
    }

    // 2. Runtime-owned fields cannot also be mutable spec variables or action
    //    params. That would create a second identity/lifecycle/context truth.
    for state_var in &automaton.state {
        if super::types::is_server_derived_field_name(&state_var.name) {
            return Err(AutomatonParseError::Validation(format!(
                "state variable '{}' uses a runtime-owned field name",
                state_var.name
            )));
        }
    }
    for action in &automaton.actions {
        for param in &action.params {
            if param.source().is_some()
                && (!automaton.automaton.strict_action_params
                    || param.param_type() != VarType::String)
            {
                return Err(AutomatonParseError::Validation(format!(
                    "action '{}' bound parameter '{}' requires strict_action_params and string type",
                    action.name,
                    param.name(),
                )));
            }
            if super::types::is_server_derived_field_name(param.name()) {
                return Err(AutomatonParseError::Validation(format!(
                    "action '{}' parameter '{}' uses a runtime-owned field name",
                    action.name,
                    param.name()
                )));
            }
        }
    }

    super::contracts::validate(automaton)?;
    validate_predicates(automaton)?;

    // 3. All `from` and `to` states in actions must be declared states.
    for action in &automaton.actions {
        for from in &action.from {
            if !automaton.automaton.states.contains(from) {
                return Err(AutomatonParseError::Validation(format!(
                    "action '{}' references undeclared from-state '{from}'",
                    action.name
                )));
            }
        }
        if let Some(to) = &action.to
            && !automaton.automaton.states.contains(to)
        {
            return Err(AutomatonParseError::Validation(format!(
                "action '{}' references undeclared to-state '{to}'",
                action.name
            )));
        }
    }

    let action_names: Vec<&str> = automaton.actions.iter().map(|a| a.name.as_str()).collect();

    // 5. Validate [[state_timeout]] declarations (ADR-0049).
    //    - `state` must be a declared state.
    //    - `on_timeout` must be a declared action.
    //    - each `reset_on` entry must be a declared action.
    //    - `after_seconds` must be > 0 (a zero delay is almost certainly a typo).
    //    - the same state must not be declared twice (ambiguous timer contract).
    let mut seen_states: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for st in &automaton.state_timeouts {
        if !automaton.automaton.states.contains(&st.state) {
            return Err(AutomatonParseError::Validation(format!(
                "state_timeout references undeclared state '{}'",
                st.state
            )));
        }
        if !seen_states.insert(st.state.as_str()) {
            return Err(AutomatonParseError::Validation(format!(
                "state_timeout declared twice for state '{}'",
                st.state
            )));
        }
        if st.after_seconds == 0 {
            return Err(AutomatonParseError::Validation(format!(
                "state_timeout for '{}' must have after_seconds > 0",
                st.state
            )));
        }
        if !action_names.contains(&st.on_timeout.as_str()) {
            return Err(AutomatonParseError::Validation(format!(
                "state_timeout for '{}' references unknown on_timeout action '{}'",
                st.state, st.on_timeout
            )));
        }
        for reset in &st.reset_on {
            if !action_names.contains(&reset.as_str()) {
                return Err(AutomatonParseError::Validation(format!(
                    "state_timeout for '{}' references unknown reset_on action '{reset}'",
                    st.state
                )));
            }
        }
    }

    // 6. Validate allow_indefinite_states entries are declared states
    //    (ADR-0050 support).
    for state in &automaton.automaton.allow_indefinite_states {
        if !automaton.automaton.states.contains(state) {
            return Err(AutomatonParseError::Validation(format!(
                "allow_indefinite_states references undeclared state '{state}'"
            )));
        }
    }

    // 7. Validate [[action.triggers]] declarations (ADR-0046).
    triggers::validate_action_triggers(automaton, &action_names)?;
    triggers::validate_timeout_args(automaton)?;
    webhooks::validate_webhooks(automaton, &action_names)?;

    // 7. Validate [[vector]] access-path declarations (ADR-0155).
    //    - `property` and `model_property` must be declared state variables.
    //    - `dims` must be > 0.
    //    - `metric` must be one of cosine | dot | l2.
    //    - names must be unique (each identifies one index partition + `decl=`).
    validate_vector_decls(automaton)?;
    carries::validate_carries(automaton)?;

    // 8. Statechart labels: `says` is one line; `[[group]]` boxes form a tree.
    groups::validate_says(automaton)?;
    groups::validate_groups(automaton)?;

    Ok(())
}

/// Name- and type-check every predicate, and check `terminal` states exist.
fn validate_predicates(automaton: &Automaton) -> Result<(), AutomatonParseError> {
    use crate::predicate::{Scope, VarKind, check, check_effects};

    let vars: std::collections::BTreeMap<String, VarKind> = automaton
        .state
        .iter()
        .map(|sv| (sv.name.clone(), sv.var_type.kind()))
        .collect();
    let invalid =
        |slot: String, error: String| AutomatonParseError::Validation(format!("{slot}: {error}"));
    // A related entity's field is read only by the joint check of several
    // entity types; the runtime resolves related statuses, not fields. Do not
    // let a guard or a parameter constraint read one without a commit that
    // spans both records: the dispatch path would read the field before the
    // actor writes, and the related record can change in between, so the
    // write would rest on a stale read (an install pin set from a commit the
    // install later rolled back is that bug).
    let no_related_field = |expr: &crate::predicate::Expr, slot: String| {
        if expr.reads_related_field() {
            Err(invalid(
                slot,
                format!(
                    "`{expr}` reads a related entity's field, which only an \
                     [[invariant]] may do"
                ),
            ))
        } else {
            Ok(())
        }
    };
    // `params.p` is a parameter of the move being taken, so only an
    // action's guard reads one, and only a parameter its action declares.
    let no_params = |expr: &crate::predicate::Expr, slot: String| match expr.params_read().first() {
        Some(param) => Err(invalid(
            slot,
            format!("`params.{param}` is a parameter of a move; only an action's guard reads one"),
        )),
        None => Ok(()),
    };
    for action in &automaton.actions {
        check(&action.guard, Scope::State(&vars))
            .map_err(|e| invalid(format!("action '{}' guard", action.name), e))?;
        no_related_field(&action.guard, format!("action '{}' guard", action.name))?;
        if let Some(param) = action
            .guard
            .params_read()
            .into_iter()
            .find(|param| !action.params.iter().any(|p| p.name() == param))
        {
            return Err(invalid(
                format!("action '{}' guard", action.name),
                format!(
                    "`params.{param}` is not a parameter of '{}'; declare it in its params",
                    action.name
                ),
            ));
        }
        check_effects(&action.effect, &vars)
            .map_err(|e| invalid(format!("action '{}' effect", action.name), e))?;
        for effect in &action.effect {
            let (Effect::Schedule { action: target, .. }
            | Effect::ScheduleAt { action: target, .. }) = effect
            else {
                continue;
            };
            if !automaton.actions.iter().any(|a| &a.name == target) {
                return Err(invalid(
                    format!("action '{}' effect", action.name),
                    format!("'{effect}' schedules unknown action '{target}'"),
                ));
            }
        }
        for trigger in &action.triggers {
            if let Some(guard) = &trigger.guard {
                let slot = format!(
                    "trigger '{}' on action '{}' guard",
                    trigger.name, action.name
                );
                check(guard, Scope::Fields).map_err(|e| invalid(slot.clone(), e))?;
                no_params(guard, slot.clone())?;
                no_related_field(guard, slot)?;
            }
        }
    }
    for inv in &automaton.invariants {
        let slot = format!("invariant '{}'", inv.name);
        check(&inv.assert, Scope::State(&vars)).map_err(|e| invalid(slot.clone(), e))?;
        no_params(&inv.assert, slot.clone())?;
        if let Some(culprit) = crate::predicate::unverifiable(&inv.assert, &vars) {
            return Err(invalid(
                slot,
                format!(
                    "`{culprit}` reads a value the verification cascade cannot model \
                     (a number that is not a counter, an undeclared field, or null); \
                     state it as a [[field_invariant]], which is checked on writes"
                ),
            ));
        }
    }
    for rule in &automaton.entry_rules {
        let slot = format!("entry rule '{}'", rule.name);
        for place in std::iter::once(&rule.place).chain(&rule.from) {
            if !automaton.automaton.states.contains(place) {
                return Err(invalid(slot, format!("'{place}' is not a declared state")));
            }
        }
        check(&rule.assert, Scope::State(&vars)).map_err(|e| invalid(slot.clone(), e))?;
        no_params(&rule.assert, slot.clone())?;
        no_related_field(&rule.assert, slot.clone())?;
        // Entry checks share the assertion read contract, with nullable
        // statuses and local membership modeled by the shared move environment.
        if let Some(culprit) = crate::predicate::unverifiable_entry(&rule.assert, &vars) {
            return Err(invalid(
                slot,
                format!("`{culprit}` reads a value the verification cascade cannot model"),
            ));
        }
        // A rule no move is checked against proves nothing.
        let covered = |from: &String| {
            if rule.from.is_empty() {
                from != &rule.place
            } else {
                rule.from.contains(from)
            }
        };
        if !automaton.actions.iter().any(|a| {
            let sources = if a.from.is_empty() {
                &automaton.automaton.states
            } else {
                &a.from
            };
            a.kind != ActionKind::Output
                && a.to.as_ref() == Some(&rule.place)
                && sources.iter().any(covered)
        }) {
            return Err(invalid(
                slot,
                format!(
                    "no action moves into '{}' from a place it covers",
                    rule.place
                ),
            ));
        }
    }
    for live in &automaton.liveness {
        let slot = format!("liveness '{}'", live.name);
        let Some(eventually) = &live.eventually else {
            if !live.relies_on.is_empty() {
                return Err(invalid(slot, "`relies_on` needs `eventually`".into()));
            }
            if !live.can_reach.is_empty() {
                validate_can_reach(automaton, live).map_err(|e| invalid(slot, e))?;
            }
            continue;
        };
        if !live.from.is_empty()
            || !live.reaches.is_empty()
            || !live.can_reach.is_empty()
            || live.has_actions.is_some()
        {
            return Err(invalid(
                slot,
                "`eventually` takes no `from`, `reaches`, `can_reach` or `has_actions`".into(),
            ));
        }
        check(eventually, Scope::State(&vars)).map_err(|e| invalid(slot.clone(), e))?;
        if let Some(culprit) = crate::predicate::unverifiable(eventually, &vars) {
            return Err(invalid(
                slot,
                format!(
                    "`{culprit}` reads a value the verification cascade cannot model \
                     (a number that is not a counter, an undeclared field, or null)"
                ),
            ));
        }
        if let Some(unknown) = live
            .relies_on
            .iter()
            .find(|name| !automaton.actions.iter().any(|a| &a.name == *name))
        {
            return Err(invalid(
                slot,
                format!("`relies_on` names no action '{unknown}'"),
            ));
        }
    }
    for inv in &automaton.field_invariants {
        let slot = format!("field_invariant '{}'", inv.name);
        check(&inv.assert, Scope::Fields).map_err(|e| invalid(slot.clone(), e))?;
        no_params(&inv.assert, slot.clone())?;
        no_related_field(&inv.assert, slot)?;
    }
    for state in &automaton.automaton.terminal {
        if !automaton.automaton.states.contains(state) {
            return Err(AutomatonParseError::Validation(format!(
                "terminal references undeclared state '{state}'"
            )));
        }
    }
    Ok(())
}

/// A `can_reach` rule is its own kind of liveness, and every place it names
/// is declared: a misspelt `from` would leave it checking nothing, and a
/// misspelt target would refuse every record.
fn validate_can_reach(automaton: &Automaton, live: &Liveness) -> Result<(), String> {
    if !live.reaches.is_empty() || live.has_actions.is_some() {
        return Err("`can_reach` takes no `reaches` or `has_actions`".into());
    }
    for (key, places) in [("from", &live.from), ("can_reach", &live.can_reach)] {
        if let Some(place) = places
            .iter()
            .find(|place| !automaton.automaton.states.contains(place))
        {
            return Err(format!("`{key}` names no place '{place}'"));
        }
    }
    Ok(())
}

/// Validate all `[[vector]]` access-path declarations per ADR-0155.
fn validate_vector_decls(automaton: &Automaton) -> Result<(), AutomatonParseError> {
    const METRICS: [&str; 3] = ["cosine", "dot", "l2"];
    let state_var_names: std::collections::BTreeSet<&str> =
        automaton.state.iter().map(|sv| sv.name.as_str()).collect();
    let mut seen_names: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for vec_decl in &automaton.vectors {
        if !seen_names.insert(vec_decl.name.as_str()) {
            return Err(AutomatonParseError::Validation(format!(
                "vector path '{}' declared twice",
                vec_decl.name
            )));
        }
        if !state_var_names.contains(vec_decl.property.as_str()) {
            return Err(AutomatonParseError::Validation(format!(
                "vector path '{}' references undeclared property state variable '{}'",
                vec_decl.name, vec_decl.property
            )));
        }
        if !state_var_names.contains(vec_decl.model_property.as_str()) {
            return Err(AutomatonParseError::Validation(format!(
                "vector path '{}' references undeclared model_property state variable '{}'",
                vec_decl.name, vec_decl.model_property
            )));
        }
        if vec_decl.dims == 0 {
            return Err(AutomatonParseError::Validation(format!(
                "vector path '{}' must declare dims > 0",
                vec_decl.name
            )));
        }
        if !METRICS.contains(&vec_decl.metric.as_str()) {
            return Err(AutomatonParseError::Validation(format!(
                "vector path '{}' has unknown metric '{}' (expected one of cosine, dot, l2)",
                vec_decl.name, vec_decl.metric
            )));
        }
    }
    Ok(())
}
