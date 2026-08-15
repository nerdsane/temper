use super::parser::AutomatonParseError;
use super::{Automaton, Effect, Guard, KeyDecl};

/// Maximum typed-reference targets a single prospective write may resolve.
pub const MAX_REFERENCE_TARGETS_PER_WRITE: usize = 16;

/// Validate the entity-local portion of ADR-0156 reference declarations.
pub(super) fn validate_reference_declarations(
    automaton: &Automaton,
) -> Result<(), AutomatonParseError> {
    let mut state_refs = std::collections::BTreeMap::new();
    for state in &automaton.state {
        if state.var_type == "ref" {
            let target = state
                .entity_type
                .as_deref()
                .filter(|target| !target.trim().is_empty())
                .ok_or_else(|| {
                    AutomatonParseError::Validation(format!(
                        "typed reference state variable '{}' must declare entity_type",
                        state.name
                    ))
                })?;
            let canonical = crate::to_snake_case(&state.name);
            if let Some((existing, _)) = state_refs.insert(canonical, (state.name.as_str(), target))
            {
                return Err(AutomatonParseError::Validation(format!(
                    "typed reference state variables '{}' and '{}' are alias-equivalent",
                    existing, state.name
                )));
            }
        } else if state.entity_type.is_some() {
            return Err(AutomatonParseError::Validation(format!(
                "state variable '{}' declares entity_type but type is '{}'",
                state.name, state.var_type
            )));
        }
    }
    for action in &automaton.actions {
        let mut param_refs = std::collections::BTreeMap::new();
        for param in &action.params {
            let canonical_param = crate::to_snake_case(param.name());
            if let Some((_, state_target)) = state_refs.get(&canonical_param)
                && param.param_type() != "ref"
            {
                return Err(AutomatonParseError::Validation(format!(
                    "action parameter '{}.{}' projects onto typed reference '{}' but has type '{}'",
                    action.name,
                    param.name(),
                    state_target,
                    param.param_type()
                )));
            }
            if param.param_type() == "ref" {
                let target = param
                    .entity_type()
                    .filter(|target| !target.trim().is_empty())
                    .ok_or_else(|| {
                        AutomatonParseError::Validation(format!(
                            "typed reference parameter '{}.{}' must declare entity_type",
                            action.name,
                            param.name()
                        ))
                    })?;
                if let Some((existing, _)) =
                    param_refs.insert(canonical_param.clone(), (param.name(), target))
                {
                    return Err(AutomatonParseError::Validation(format!(
                        "action parameters '{}.{}' and '{}.{}' are alias-equivalent",
                        action.name,
                        existing,
                        action.name,
                        param.name()
                    )));
                }
                if let Some((_, state_target)) = state_refs.get(&canonical_param)
                    && *state_target != target
                {
                    return Err(AutomatonParseError::Validation(format!(
                        "typed reference parameter '{}.{}' targets '{}' but state reference targets '{}'",
                        action.name,
                        param.name(),
                        target,
                        state_target
                    )));
                }
            } else if param.entity_type().is_some() {
                return Err(AutomatonParseError::Validation(format!(
                    "action parameter '{}.{}' declares entity_type but type is '{}'",
                    action.name,
                    param.name(),
                    param.param_type()
                )));
            }
        }
        for guard in &action.guard {
            validate_reference_guard(guard, &action.name, &state_refs, &param_refs)?;
        }
        let observed_references = state_refs.len().saturating_add(
            param_refs
                .keys()
                .filter(|name| !state_refs.contains_key(*name))
                .count(),
        );
        if observed_references > MAX_REFERENCE_TARGETS_PER_WRITE {
            return Err(AutomatonParseError::Validation(format!(
                "action '{}' may observe {} typed references, exceeding the per-write budget of {}",
                action.name, observed_references, MAX_REFERENCE_TARGETS_PER_WRITE
            )));
        }
    }
    if state_refs.len() > MAX_REFERENCE_TARGETS_PER_WRITE {
        return Err(AutomatonParseError::Validation(format!(
            "entity declares {} typed reference fields, exceeding the per-write budget of {}",
            state_refs.len(),
            MAX_REFERENCE_TARGETS_PER_WRITE
        )));
    }
    let identity_keys: Vec<&KeyDecl> = automaton.keys.iter().filter(|key| key.entity_id).collect();
    if identity_keys.len() > 1 {
        return Err(AutomatonParseError::Validation(
            "at most one [[key]] may declare entity_id = true".to_string(),
        ));
    }
    if let Some(key) = identity_keys.first() {
        if key.properties.is_empty() {
            return Err(AutomatonParseError::Validation(format!(
                "identity key '{}' must declare at least one property",
                key.name
            )));
        }
        for property in &key.properties {
            if !state_refs.contains_key(&crate::to_snake_case(property)) {
                return Err(AutomatonParseError::Validation(format!(
                    "identity key '{}' property '{}' must be an immutable typed reference",
                    key.name, property
                )));
            }
        }
    }
    for action in &automaton.actions {
        for effect in &action.effect {
            if let Effect::Spawn {
                store_id_in: Some(field),
                ..
            } = effect
                && state_refs.contains_key(&crate::to_snake_case(field))
            {
                return Err(AutomatonParseError::Validation(format!(
                    "action '{}' asynchronous spawn cannot store into typed reference '{}'",
                    action.name, field
                )));
            }
        }
    }
    Ok(())
}

fn validate_reference_guard(
    guard: &Guard,
    action: &str,
    state_refs: &std::collections::BTreeMap<String, (&str, &str)>,
    param_refs: &std::collections::BTreeMap<String, (&str, &str)>,
) -> Result<(), AutomatonParseError> {
    if let Guard::ReferenceEquals { reference, param } = guard {
        let Some((_, reference_target)) = state_refs.get(&crate::to_snake_case(reference)) else {
            return Err(AutomatonParseError::Validation(format!(
                "reference_equals on action '{action}' names non-reference state variable '{reference}'"
            )));
        };
        let Some((_, param_target)) = param_refs.get(&crate::to_snake_case(param)) else {
            return Err(AutomatonParseError::Validation(format!(
                "reference_equals on action '{action}' names non-reference parameter '{param}'"
            )));
        };
        if reference_target != param_target {
            return Err(AutomatonParseError::Validation(format!(
                "reference_equals on action '{action}' compares '{reference}' ({reference_target}) with '{param}' ({param_target})"
            )));
        }
    }
    Ok(())
}
