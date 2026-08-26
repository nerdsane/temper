use std::collections::BTreeSet;

use temper_spec::bundle::IoaSourceInput;
use temper_spec::csdl::EntityType;
use temper_wasm_sdk::data::{ManifestPropertyV1, ManifestValueSourceV1};

use super::ModuleSdkCodegenError;

pub(super) fn assign_entity_property_sources(
    entity_type: &str,
    entity: &EntityType,
    ioa_sources: &[IoaSourceInput],
    properties: &mut [ManifestPropertyV1],
) -> Result<(), ModuleSdkCodegenError> {
    if entity.key_properties.len() != 1 {
        return Err(ModuleSdkCodegenError::UnsupportedEntityKey {
            entity_type: entity_type.into(),
            key_properties: entity.key_properties.clone(),
        });
    }
    let key = &entity.key_properties[0];
    let key_property = properties
        .iter_mut()
        .find(|property| property.canonical_name == *key)
        .ok_or_else(|| ModuleSdkCodegenError::MissingSymbol {
            entity_type: entity_type.into(),
            symbol: format!("entity key property '{key}'"),
        })?;
    key_property.source = ManifestValueSourceV1::EntityId;

    let source = ioa_sources
        .iter()
        .find(|source| source.entity_type == entity_type)
        .ok_or_else(|| ModuleSdkCodegenError::MissingIoaSource(entity_type.into()))?;
    let automaton = temper_spec::automaton::parse_automaton(&source.source).map_err(|error| {
        ModuleSdkCodegenError::InvalidIoaSource {
            entity_type: entity_type.into(),
            message: error.to_string(),
        }
    })?;
    let short_name = entity_type.rsplit('.').next().unwrap_or(entity_type);
    if automaton.automaton.name != short_name {
        return Err(ModuleSdkCodegenError::InvalidIoaSource {
            entity_type: entity_type.into(),
            message: format!(
                "automaton '{}' does not match entity short name '{short_name}'",
                automaton.automaton.name
            ),
        });
    }

    let lifecycle_states = automaton
        .automaton
        .states
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let mut candidates = properties
        .iter()
        .filter(|property| property.source == ManifestValueSourceV1::StoredField)
        .filter(|property| {
            property_domain_accepts_states(property, &lifecycle_states)
                && (property.default_value.as_ref().is_some_and(|value| {
                    value.as_str() == Some(automaton.automaton.initial.as_str())
                }) || enum_domain_exactly_matches(property, &lifecycle_states))
        })
        .map(|property| property.canonical_name.clone())
        .collect::<Vec<_>>();
    candidates.sort();

    let lifecycle_property = match candidates.as_slice() {
        [candidate] => candidate,
        [] => {
            return Err(ModuleSdkCodegenError::MissingLifecycleProperty {
                entity_type: entity_type.into(),
                initial_state: automaton.automaton.initial,
            });
        }
        _ => {
            return Err(ModuleSdkCodegenError::AmbiguousLifecycleProperty {
                entity_type: entity_type.into(),
                initial_state: automaton.automaton.initial,
                candidates,
            });
        }
    };
    let lifecycle_property = properties
        .iter_mut()
        .find(|property| property.canonical_name == *lifecycle_property)
        .expect("resolved lifecycle candidate must remain in manifest properties");
    if let Some(default) = lifecycle_property.default_value.as_ref()
        && default.as_str() != Some(automaton.automaton.initial.as_str())
    {
        return Err(ModuleSdkCodegenError::LifecycleDefaultMismatch {
            entity_type: entity_type.into(),
            property: lifecycle_property.canonical_name.clone(),
            initial_state: automaton.automaton.initial,
        });
    }
    lifecycle_property.source = ManifestValueSourceV1::LifecycleStatus;
    Ok(())
}

fn property_domain_accepts_states(property: &ManifestPropertyV1, states: &BTreeSet<&str>) -> bool {
    if property.type_name == "Edm.String" {
        return true;
    }
    let members = property
        .enum_members
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    !members.is_empty() && states.is_subset(&members)
}

fn enum_domain_exactly_matches(property: &ManifestPropertyV1, states: &BTreeSet<&str>) -> bool {
    let members = property
        .enum_members
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    !members.is_empty() && members == *states
}
