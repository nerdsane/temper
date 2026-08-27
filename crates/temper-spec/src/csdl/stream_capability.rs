//! Verification for the closed Temper stream-capability vocabulary.

use std::collections::BTreeMap;

use super::{Annotation, AnnotationValue, CsdlDocument, EntityType, NavigationProperty};

mod migration;
pub use migration::{
    VerifiedStreamMigrationProvenanceV1, stream_capability_set_digest_v1,
    verify_stream_migration_automata_v1,
};
mod types;
pub use types::{StreamCapabilityError, StreamCapabilityMutabilityV1, VerifiedStreamCapabilityV1};

const MUTABILITY_TERM: &str = "Temper.Vocab.Stream.Mutability";
const VERSION_ENTITY_TYPE_TERM: &str = "Temper.Vocab.Stream.VersionEntityType";
const VERSION_COLLECTION_TERM: &str = "Temper.Vocab.Stream.VersionCollection";
const AUTHORIZATION_PARENT_TERM: &str = "Temper.Vocab.Stream.AuthorizationParent";
const DESCRIPTOR_CONTRACT_TERM: &str = "Temper.Vocab.Stream.DescriptorContractVersion";

/// Verify every stream declaration and return capabilities in subject-type order.
pub fn verify_stream_capabilities_v1(
    document: &CsdlDocument,
) -> Result<Vec<VerifiedStreamCapabilityV1>, StreamCapabilityError> {
    let mut entities = BTreeMap::new();
    for schema in &document.schemas {
        for entity in &schema.entity_types {
            let qualified = format!("{}.{}", schema.namespace, entity.name);
            if entities.insert(qualified.clone(), entity).is_some() {
                return Err(StreamCapabilityError::DuplicateEntityType(qualified));
            }
        }
    }

    let mut capabilities = BTreeMap::new();
    for (qualified, entity) in &entities {
        let has_stream_terms = entity.has_stream || stream_annotations(entity).next().is_some();
        if !has_stream_terms {
            continue;
        }
        let mutability = required_mutability(qualified, entity)?;
        let version_type = string_annotation(qualified, entity, VERSION_ENTITY_TYPE_TERM)?;
        let version_navigation = path_annotation(qualified, entity, VERSION_COLLECTION_TERM)?;
        let parent_navigation = path_annotation(qualified, entity, AUTHORIZATION_PARENT_TERM)?;
        let descriptor_contract_v1_active = descriptor_contract_active(qualified, entity)?;
        let migration_provenance = migration::verified_migration_provenance(
            qualified,
            entity,
            parent_navigation.is_some(),
            descriptor_contract_v1_active,
        )?;

        let capability = match mutability {
            StreamCapabilityMutabilityV1::Mutable => {
                if !entity.has_stream {
                    return Err(StreamCapabilityError::MutableWithoutHasStream(
                        qualified.clone(),
                    ));
                }
                if parent_navigation.is_some() {
                    return Err(StreamCapabilityError::IncompatibleAnnotation {
                        entity_type: qualified.clone(),
                        term: AUTHORIZATION_PARENT_TERM,
                    });
                }
                if version_type.is_some() != version_navigation.is_some() {
                    return Err(StreamCapabilityError::IncompleteVersionContract(
                        qualified.clone(),
                    ));
                }
                if let (Some(target), Some(navigation)) =
                    (version_type.as_deref(), version_navigation.as_deref())
                {
                    require_entity(&entities, qualified, target)?;
                    require_navigation(qualified, entity, navigation, target, true)?;
                }
                VerifiedStreamCapabilityV1 {
                    subject_type: qualified.clone(),
                    mutability,
                    version_entity_type: version_type,
                    version_collection_navigation: version_navigation,
                    authorization_parent_navigation: None,
                    authorization_parent_type: None,
                    migration_provenance,
                    descriptor_contract_v1_active,
                }
            }
            StreamCapabilityMutabilityV1::Immutable => {
                if version_type.is_some() {
                    return Err(StreamCapabilityError::IncompatibleAnnotation {
                        entity_type: qualified.clone(),
                        term: VERSION_ENTITY_TYPE_TERM,
                    });
                }
                if version_navigation.is_some() {
                    return Err(StreamCapabilityError::IncompatibleAnnotation {
                        entity_type: qualified.clone(),
                        term: VERSION_COLLECTION_TERM,
                    });
                }
                let navigation = parent_navigation.ok_or_else(|| {
                    StreamCapabilityError::MissingAuthorizationParent(qualified.clone())
                })?;
                let parent = unique_navigation(qualified, entity, &navigation)?;
                let (parent_type, collection) = navigation_target(&parent.type_name);
                if collection || !entities.contains_key(parent_type) {
                    return Err(StreamCapabilityError::UnknownTargetType {
                        entity_type: qualified.clone(),
                        target_type: parent_type.to_string(),
                    });
                }
                validate_parent_constraints(
                    qualified,
                    entity,
                    &navigation,
                    parent,
                    entities[parent_type],
                )?;
                VerifiedStreamCapabilityV1 {
                    subject_type: qualified.clone(),
                    mutability,
                    version_entity_type: None,
                    version_collection_navigation: None,
                    authorization_parent_navigation: Some(navigation),
                    authorization_parent_type: Some(parent_type.to_string()),
                    migration_provenance,
                    descriptor_contract_v1_active,
                }
            }
        };
        capabilities.insert(qualified.clone(), capability);
    }

    for capability in capabilities.values() {
        let (Some(version_type), Some(_)) = (
            capability.version_entity_type.as_deref(),
            capability.version_collection_navigation.as_deref(),
        ) else {
            continue;
        };
        let Some(version) = capabilities.get(version_type) else {
            return Err(StreamCapabilityError::NonMutualVersionContract(
                capability.subject_type.clone(),
            ));
        };
        if version.mutability != StreamCapabilityMutabilityV1::Immutable
            || version.authorization_parent_type.as_deref()
                != Some(capability.subject_type.as_str())
        {
            return Err(StreamCapabilityError::NonMutualVersionContract(
                capability.subject_type.clone(),
            ));
        }
    }

    Ok(capabilities.into_values().collect())
}

fn stream_annotations(entity: &EntityType) -> impl Iterator<Item = &Annotation> {
    entity.annotations.iter().filter(|annotation| {
        matches!(
            annotation.term.as_str(),
            MUTABILITY_TERM
                | VERSION_ENTITY_TYPE_TERM
                | VERSION_COLLECTION_TERM
                | AUTHORIZATION_PARENT_TERM
                | DESCRIPTOR_CONTRACT_TERM
        ) || migration::migration_annotations().contains(&annotation.term.as_str())
    })
}

fn descriptor_contract_active(
    entity_type: &str,
    entity: &EntityType,
) -> Result<bool, StreamCapabilityError> {
    let Some(annotation) = exact_annotation(entity_type, entity, DESCRIPTOR_CONTRACT_TERM)? else {
        return Ok(false);
    };
    match &annotation.value {
        AnnotationValue::Int(1) => Ok(true),
        AnnotationValue::Int(version) => {
            Err(StreamCapabilityError::UnsupportedDescriptorContract {
                entity_type: entity_type.to_string(),
                version: *version,
            })
        }
        _ => Err(StreamCapabilityError::InvalidAnnotationValue {
            entity_type: entity_type.to_string(),
            term: DESCRIPTOR_CONTRACT_TERM,
            expected: "Int=1",
        }),
    }
}

pub(super) fn exact_annotation<'a>(
    entity_type: &str,
    entity: &'a EntityType,
    term: &'static str,
) -> Result<Option<&'a Annotation>, StreamCapabilityError> {
    let matches = entity
        .annotations
        .iter()
        .filter(|annotation| annotation.term == term)
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [] => Ok(None),
        [annotation] => Ok(Some(*annotation)),
        _ => Err(StreamCapabilityError::DuplicateAnnotation {
            entity_type: entity_type.to_string(),
            term,
        }),
    }
}

fn required_mutability(
    entity_type: &str,
    entity: &EntityType,
) -> Result<StreamCapabilityMutabilityV1, StreamCapabilityError> {
    let annotation = exact_annotation(entity_type, entity, MUTABILITY_TERM)?
        .ok_or_else(|| StreamCapabilityError::MissingMutability(entity_type.to_string()))?;
    let AnnotationValue::String(value) = &annotation.value else {
        return Err(StreamCapabilityError::InvalidAnnotationValue {
            entity_type: entity_type.to_string(),
            term: MUTABILITY_TERM,
            expected: "String",
        });
    };
    match value.as_str() {
        "Mutable" => Ok(StreamCapabilityMutabilityV1::Mutable),
        "Immutable" => Ok(StreamCapabilityMutabilityV1::Immutable),
        _ => Err(StreamCapabilityError::UnknownMutability {
            entity_type: entity_type.to_string(),
            value: value.clone(),
        }),
    }
}

fn string_annotation(
    entity_type: &str,
    entity: &EntityType,
    term: &'static str,
) -> Result<Option<String>, StreamCapabilityError> {
    let Some(annotation) = exact_annotation(entity_type, entity, term)? else {
        return Ok(None);
    };
    match &annotation.value {
        AnnotationValue::String(value) if !value.is_empty() => Ok(Some(value.clone())),
        _ => Err(StreamCapabilityError::InvalidAnnotationValue {
            entity_type: entity_type.to_string(),
            term,
            expected: "non-empty String",
        }),
    }
}

fn path_annotation(
    entity_type: &str,
    entity: &EntityType,
    term: &'static str,
) -> Result<Option<String>, StreamCapabilityError> {
    let Some(annotation) = exact_annotation(entity_type, entity, term)? else {
        return Ok(None);
    };
    match &annotation.value {
        AnnotationValue::NavigationPropertyPath(value)
            if !value.is_empty() && !value.contains('/') =>
        {
            Ok(Some(value.clone()))
        }
        _ => Err(StreamCapabilityError::InvalidAnnotationValue {
            entity_type: entity_type.to_string(),
            term,
            expected: "single-segment NavigationPropertyPath",
        }),
    }
}

fn require_entity<'a>(
    entities: &'a BTreeMap<String, &EntityType>,
    entity_type: &str,
    target_type: &str,
) -> Result<&'a EntityType, StreamCapabilityError> {
    entities
        .get(target_type)
        .copied()
        .ok_or_else(|| StreamCapabilityError::UnknownTargetType {
            entity_type: entity_type.to_string(),
            target_type: target_type.to_string(),
        })
}

fn unique_navigation<'a>(
    entity_type: &str,
    entity: &'a EntityType,
    navigation: &str,
) -> Result<&'a NavigationProperty, StreamCapabilityError> {
    let matches = entity
        .navigation_properties
        .iter()
        .filter(|candidate| candidate.name == navigation)
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [navigation] => Ok(*navigation),
        _ => Err(StreamCapabilityError::InvalidNavigation {
            entity_type: entity_type.to_string(),
            navigation: navigation.to_string(),
        }),
    }
}

fn require_navigation(
    entity_type: &str,
    entity: &EntityType,
    navigation: &str,
    expected_target: &str,
    expected_collection: bool,
) -> Result<(), StreamCapabilityError> {
    let navigation_value = unique_navigation(entity_type, entity, navigation)?;
    let (target, collection) = navigation_target(&navigation_value.type_name);
    if target != expected_target || collection != expected_collection {
        return Err(StreamCapabilityError::NavigationTargetMismatch {
            entity_type: entity_type.to_string(),
            navigation: navigation.to_string(),
            expected_target: expected_target.to_string(),
            expected_collection,
        });
    }
    Ok(())
}

fn navigation_target(type_name: &str) -> (&str, bool) {
    type_name
        .strip_prefix("Collection(")
        .and_then(|target| target.strip_suffix(')'))
        .map_or((type_name, false), |target| (target, true))
}

fn validate_parent_constraints(
    entity_type: &str,
    child: &EntityType,
    navigation_name: &str,
    navigation: &NavigationProperty,
    parent: &EntityType,
) -> Result<(), StreamCapabilityError> {
    let valid = !navigation.referential_constraints.is_empty()
        && navigation.referential_constraints.iter().all(|constraint| {
            child
                .properties
                .iter()
                .any(|property| property.name == constraint.property)
                && parent
                    .properties
                    .iter()
                    .any(|property| property.name == constraint.referenced_property)
                && parent
                    .key_properties
                    .contains(&constraint.referenced_property)
        });
    if !valid {
        return Err(StreamCapabilityError::InvalidReferentialConstraint {
            entity_type: entity_type.to_string(),
            navigation: navigation_name.to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
#[path = "stream_capability/tests.rs"]
mod tests;
