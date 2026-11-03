//! Checks on `[[carry]]` sections.

use crate::predicate::{Scope, VarKind, check};

use super::super::parser::AutomatonParseError;
use super::super::types::{ActionKind, Automaton, CARRY_PREFIX, ParameterSource};

/// Each carry ends in a state of this version, reads only the record its
/// guard is about, and sets declared state variables to values of their
/// type. No action may take a carry's name.
pub(super) fn validate_carries(automaton: &Automaton) -> Result<(), AutomatonParseError> {
    let invalid =
        |slot: &str, error: String| AutomatonParseError::Validation(format!("{slot}: {error}"));
    if let Some(action) = automaton
        .actions
        .iter()
        .find(|action| action.name.starts_with(CARRY_PREFIX))
    {
        return Err(invalid(
            &format!("action '{}'", action.name),
            format!("names starting with '{CARRY_PREFIX}' belong to [[carry]] moves"),
        ));
    }
    for (index, carry) in automaton.carries.iter().enumerate() {
        let slot = format!("carry #{} (from '{}')", index + 1, carry.from);
        if !automaton
            .automaton
            .states
            .iter()
            .any(|state| state == carry.target())
        {
            return Err(invalid(
                &slot,
                format!(
                    "carried records would end in '{}', which is not a state of this version; \
                     set `to` to one",
                    carry.target()
                ),
            ));
        }
        check(&carry.guard, Scope::Fields).map_err(|e| invalid(&slot, e))?;
        if carry.guard.reads_related_field() {
            return Err(invalid(
                &slot,
                format!(
                    "`{}` reads a related entity's field, which only an [[invariant]] may do",
                    carry.guard
                ),
            ));
        }
        for (name, value) in &carry.set {
            let Some(var) = automaton.state.iter().find(|var| &var.name == name) else {
                return Err(invalid(
                    &slot,
                    format!("sets '{name}', which this version does not declare"),
                ));
            };
            let fits = match var.var_type.kind() {
                VarKind::Counter => value.as_u64().is_some(),
                VarKind::Bool => value.is_boolean(),
                VarKind::Str => value.is_string(),
                VarKind::Num => value.as_i64().is_some(),
                VarKind::List => false,
            };
            if !fits {
                return Err(invalid(
                    &slot,
                    format!("cannot set '{name}' ({:?}) to {value}", var.var_type),
                ));
            }
        }
        if let Some(then) = &carry.then {
            validate_then(automaton, then, carry.target()).map_err(|e| invalid(&slot, e))?;
        }
        if let Some(field) = &carry.acts_for {
            if carry.then.is_none() {
                return Err(invalid(
                    &slot,
                    "`as` names who a then move is made for; set `then`".to_string(),
                ));
            }
            validate_acts_for(automaton, field).map_err(|e| invalid(&slot, e))?;
        }
    }
    Ok(())
}

/// A carry's `as` names a string field that only the kernel writes, from the
/// identity of the caller who made a move on the record. The platform acts
/// for whoever the field names, so a field anyone else can write would let
/// them choose that user: in a spec that is not strict, a create body or a
/// field update writes any declared field; a plain action parameter of that
/// name writes it from the caller's body; and a carry's `set` writes it from
/// the spec.
fn validate_acts_for(automaton: &Automaton, field: &str) -> Result<(), String> {
    if !automaton
        .state
        .iter()
        .any(|var| var.name == field && var.var_type.kind() == VarKind::Str)
    {
        return Err(format!(
            "`as` names '{field}', which is not a string field of this version"
        ));
    }
    if !automaton.automaton.strict_action_params {
        return Err(format!(
            "`as = \"{field}\"` needs `strict_action_params = true`: otherwise a create body or \
             a field update can write '{field}' and name any user"
        ));
    }
    let plain = automaton.actions.iter().find(|action| {
        action.params.iter().any(|param| {
            param.name() == field && param.source() != Some(ParameterSource::AuthenticatedSubject)
        })
    });
    if let Some(action) = plain {
        return Err(format!(
            "`as = \"{field}\"`: action '{}' takes '{field}' from its caller; give that param \
             `source = \"authenticated_subject\"` so only the caller's own identity is written",
            action.name
        ));
    }
    if automaton
        .carries
        .iter()
        .any(|carry| carry.set.contains_key(field))
    {
        return Err(format!(
            "`as = \"{field}\"`: a [[carry]] sets '{field}', which would choose the user the then \
             move is made for"
        ));
    }
    Ok(())
}

/// A carry's `then` names a move of this version that starts from the state
/// the carry ends in.
fn validate_then(automaton: &Automaton, then: &str, target: &str) -> Result<(), String> {
    let Some(action) = automaton.actions.iter().find(|action| action.name == then) else {
        return Err(format!(
            "then names '{then}', which is not an action of this version"
        ));
    };
    if action.kind == ActionKind::Output {
        return Err(format!(
            "then names '{then}', an output action, which moves nothing"
        ));
    }
    let starts_there = if action.from.is_empty() {
        action.kind.enabled_everywhere()
    } else {
        action.from.iter().any(|state| state == target)
    };
    if !starts_there {
        return Err(format!(
            "then names '{then}', which does not start from '{target}', where carried records end"
        ));
    }
    Ok(())
}
