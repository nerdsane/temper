//! Validate semantic input contracts before publishing a tenant deployment.

use std::collections::BTreeSet;

use temper_spec::automaton::{Automaton, Guard};
use temper_spec::csdl::CsdlDocument;

use super::{EntitySpec, RegistryError};

pub(super) fn prepare_automata(
    tenant: &str,
    csdl: &CsdlDocument,
    sources: &[(&str, &str)],
) -> Result<Vec<Automaton>, RegistryError> {
    sources
        .iter()
        .map(|(entity_type, source)| {
            let automaton = temper_spec::automaton::parse_automaton(source).map_err(|error| {
                RegistryError::IoaParse {
                    tenant: tenant.to_string(),
                    entity_type: entity_type.to_string(),
                    source: error.to_string(),
                }
            })?;
            validate_bindings(csdl, entity_type, &automaton).map_err(|error| {
                RegistryError::IoaParse {
                    tenant: tenant.to_string(),
                    entity_type: entity_type.to_string(),
                    source: error,
                }
            })?;
            Ok(automaton)
        })
        .collect()
}

pub(super) fn validate_retained_automata(
    tenant: &str,
    csdl: &CsdlDocument,
    existing: &std::collections::BTreeMap<String, EntitySpec>,
    incoming: &[(&str, &str)],
) -> Result<(), RegistryError> {
    let replaced: BTreeSet<&str> = incoming.iter().map(|(name, _)| *name).collect();
    for (entity_type, spec) in existing {
        if !replaced.contains(entity_type.as_str()) {
            validate_bindings(csdl, entity_type, &spec.automaton).map_err(|source| {
                RegistryError::IoaParse {
                    tenant: tenant.to_string(),
                    entity_type: entity_type.clone(),
                    source,
                }
            })?;
        }
    }
    Ok(())
}

fn validate_bindings(
    csdl: &CsdlDocument,
    entity_type: &str,
    automaton: &Automaton,
) -> Result<(), String> {
    let mut fields: BTreeSet<String> = ["Id", "id", "Status", "status"]
        .into_iter()
        .map(str::to_string)
        .collect();
    fields.extend(automaton.state.iter().map(|state| state.name.clone()));
    for schema in &csdl.schemas {
        if let Some(entity) = schema.entity_type(entity_type) {
            fields.extend(
                entity
                    .properties
                    .iter()
                    .map(|property| property.name.clone()),
            );
        }
    }
    // These are authorization attributes, stripped from persisted entity data.
    // Declaring a CSDL property cannot make them available as model context.
    fields.remove("has_spec");
    fields.remove("HasSpec");
    for action in &automaton.actions {
        let mut params: BTreeSet<String> = action
            .params
            .iter()
            .map(|param| param.name().to_string())
            .collect();
        for schema in &csdl.schemas {
            for csdl_action in &schema.actions {
                if csdl_action.name != action.name
                    || csdl_action
                        .binding_type()
                        .is_none_or(|binding| binding.rsplit('.').next() != Some(entity_type))
                {
                    continue;
                }
                params.extend(
                    csdl_action
                        .parameters
                        .iter()
                        .skip(1)
                        .map(|param| param.name.clone()),
                );
            }
        }
        for guard in &action.guard {
            if let Guard::SystemOne(guard) = guard {
                guard
                    .validate_bindings(&fields, &params)
                    .map_err(|error| format!("action '{}': {error}", action.name))?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
