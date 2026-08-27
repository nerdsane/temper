use sha2::{Digest as _, Sha256};
use std::collections::BTreeMap;

use super::{
    AnnotationValue, EntityType, StreamCapabilityError, VerifiedStreamCapabilityV1,
    exact_annotation,
};
use serde::{Deserialize, Serialize};

use crate::automaton::Automaton;

pub(super) const PUBLICATION_ACTION_TERM: &str = "Temper.Vocab.Stream.MigrationPublicationAction";
pub(super) const CONTENT_HASH_PARAMETER_TERM: &str =
    "Temper.Vocab.Stream.MigrationContentHashParameter";
pub(super) const BYTE_LENGTH_PARAMETER_TERM: &str =
    "Temper.Vocab.Stream.MigrationByteLengthParameter";
pub(super) const CONTENT_TYPE_PARAMETER_TERM: &str =
    "Temper.Vocab.Stream.MigrationContentTypeParameter";
pub(super) const AUTHORIZATION_PARENT_PARAMETER_TERM: &str =
    "Temper.Vocab.Stream.MigrationAuthorizationParentParameter";
pub(super) const STORAGE_CONTRACT_VERSION_TERM: &str =
    "Temper.Vocab.Stream.MigrationStorageContractVersion";
pub(super) const STORAGE_KEY_PREFIX_TERM: &str = "Temper.Vocab.Stream.MigrationStorageKeyPrefix";

const STORAGE_KEY_PREFIX_BYTE_BUDGET: usize = 256;

/// Canonical, migration-only interpretation of one historical stream event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerifiedStreamMigrationProvenanceV1 {
    /// Exact historical event action that published the content.
    pub publication_action: String,
    /// Exact event parameter carrying the lowercase `sha256:` content digest.
    pub content_hash_parameter: String,
    /// Exact event parameter carrying the non-negative historical byte length.
    pub byte_length_parameter: String,
    /// Optional event parameter carrying the media type.
    pub content_type_parameter: Option<String>,
    /// Required parent identity parameter for parent-authorized streams.
    pub authorization_parent_parameter: Option<String>,
    /// Closed platform storage reconstruction contract. V1 is content addressed.
    pub storage_contract_version: u16,
    /// Bounded platform object-key prefix prepended to the content digest.
    pub storage_key_prefix: String,
}

pub(super) fn migration_annotations() -> [&'static str; 7] {
    [
        PUBLICATION_ACTION_TERM,
        CONTENT_HASH_PARAMETER_TERM,
        BYTE_LENGTH_PARAMETER_TERM,
        CONTENT_TYPE_PARAMETER_TERM,
        AUTHORIZATION_PARENT_PARAMETER_TERM,
        STORAGE_CONTRACT_VERSION_TERM,
        STORAGE_KEY_PREFIX_TERM,
    ]
}

pub(super) fn verified_migration_provenance(
    entity_type: &str,
    entity: &EntityType,
    requires_parent: bool,
    descriptor_contract_v1_active: bool,
) -> Result<Option<VerifiedStreamMigrationProvenanceV1>, StreamCapabilityError> {
    let has_any = migration_annotations().into_iter().any(|term| {
        entity
            .annotations
            .iter()
            .any(|annotation| annotation.term == term)
    });
    if !has_any {
        return if descriptor_contract_v1_active {
            Err(StreamCapabilityError::MissingMigrationProvenance(
                entity_type.to_string(),
            ))
        } else {
            Ok(None)
        };
    }

    let publication_action = required_string(entity_type, entity, PUBLICATION_ACTION_TERM)?;
    let content_hash_parameter = required_string(entity_type, entity, CONTENT_HASH_PARAMETER_TERM)?;
    let byte_length_parameter = required_string(entity_type, entity, BYTE_LENGTH_PARAMETER_TERM)?;
    let content_type_parameter = optional_string(entity_type, entity, CONTENT_TYPE_PARAMETER_TERM)?;
    let authorization_parent_parameter =
        optional_string(entity_type, entity, AUTHORIZATION_PARENT_PARAMETER_TERM)?;
    if requires_parent != authorization_parent_parameter.is_some() {
        return Err(StreamCapabilityError::MigrationParentBinding {
            entity_type: entity_type.to_string(),
            requires_parent,
        });
    }
    let storage_contract_version = required_version(entity_type, entity)?;
    let storage_key_prefix = required_string(entity_type, entity, STORAGE_KEY_PREFIX_TERM)?;
    if storage_key_prefix.len() > STORAGE_KEY_PREFIX_BYTE_BUDGET
        || storage_key_prefix.trim() != storage_key_prefix
        || storage_key_prefix.starts_with('/')
        || storage_key_prefix.contains("..")
    {
        return Err(StreamCapabilityError::InvalidStorageKeyPrefix(
            entity_type.to_string(),
        ));
    }

    Ok(Some(VerifiedStreamMigrationProvenanceV1 {
        publication_action,
        content_hash_parameter,
        byte_length_parameter,
        content_type_parameter,
        authorization_parent_parameter,
        storage_contract_version,
        storage_key_prefix,
    }))
}

/// Compute the stable digest of a verified stream-capability set.
pub fn stream_capability_set_digest_v1(
    capabilities: &[VerifiedStreamCapabilityV1],
) -> Result<String, String> {
    let mut canonical = capabilities.to_vec();
    canonical.sort_by(|left, right| left.subject_type.cmp(&right.subject_type));
    let bytes = serde_json::to_vec(&canonical)
        .map_err(|error| format!("failed to encode verified stream capabilities: {error}"))?;
    Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
}

/// Cross-check every migration mapping against the corresponding IOA action.
pub fn verify_stream_migration_automata_v1(
    capabilities: &[VerifiedStreamCapabilityV1],
    automata: &BTreeMap<String, Automaton>,
) -> Result<(), String> {
    for capability in capabilities {
        let Some(provenance) = capability.migration_provenance.as_ref() else {
            continue;
        };
        let short_name = capability
            .subject_type
            .rsplit('.')
            .next()
            .unwrap_or_default();
        let automaton = automata
            .get(&capability.subject_type)
            .or_else(|| automata.get(short_name))
            .ok_or_else(|| {
                format!(
                    "stream migration subject '{}' has no IOA automaton",
                    capability.subject_type
                )
            })?;
        let actions = automaton
            .actions
            .iter()
            .filter(|action| action.name == provenance.publication_action)
            .collect::<Vec<_>>();
        let [action] = actions.as_slice() else {
            return Err(format!(
                "stream migration subject '{}' must declare exactly one IOA action '{}'",
                capability.subject_type, provenance.publication_action
            ));
        };
        for parameter in [
            Some(provenance.content_hash_parameter.as_str()),
            Some(provenance.byte_length_parameter.as_str()),
            provenance.content_type_parameter.as_deref(),
            provenance.authorization_parent_parameter.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            if !action
                .params
                .iter()
                .any(|candidate| candidate.name() == parameter)
            {
                return Err(format!(
                    "stream migration action '{}.{}' has no mapped parameter '{}'",
                    capability.subject_type, provenance.publication_action, parameter
                ));
            }
        }
    }
    Ok(())
}

fn required_string(
    entity_type: &str,
    entity: &EntityType,
    term: &'static str,
) -> Result<String, StreamCapabilityError> {
    optional_string(entity_type, entity, term)?.ok_or_else(|| {
        StreamCapabilityError::IncompleteMigrationProvenance {
            entity_type: entity_type.to_string(),
            term,
        }
    })
}

fn optional_string(
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

fn required_version(entity_type: &str, entity: &EntityType) -> Result<u16, StreamCapabilityError> {
    let annotation = exact_annotation(entity_type, entity, STORAGE_CONTRACT_VERSION_TERM)?
        .ok_or_else(|| StreamCapabilityError::IncompleteMigrationProvenance {
            entity_type: entity_type.to_string(),
            term: STORAGE_CONTRACT_VERSION_TERM,
        })?;
    match annotation.value {
        AnnotationValue::Int(1) => Ok(1),
        AnnotationValue::Int(version) => {
            Err(StreamCapabilityError::UnsupportedMigrationStorageContract {
                entity_type: entity_type.to_string(),
                version,
            })
        }
        _ => Err(StreamCapabilityError::InvalidAnnotationValue {
            entity_type: entity_type.to_string(),
            term: STORAGE_CONTRACT_VERSION_TERM,
            expected: "Int=1",
        }),
    }
}
