//! Verified historical publication decoding and replay evidence.

use super::*;

pub(super) fn validate_verified_capability(
    descriptor: &StreamDescriptorV1,
    capability: &VerifiedStreamCapabilityV1,
) -> Result<(), String> {
    if capability.subject_type.rsplit('.').next() != Some(descriptor.subject().entity_type()) {
        return Err("stream descriptor subject differs from verified migration capability".into());
    }
    let mutability_matches = matches!(
        (capability.mutability, descriptor.mutability()),
        (
            StreamCapabilityMutabilityV1::Mutable,
            StreamMutability::Mutable
        ) | (
            StreamCapabilityMutabilityV1::Immutable,
            StreamMutability::Immutable
        )
    );
    let parent_matches = match (
        capability.authorization_parent_type.as_deref(),
        descriptor.authorization_parent(),
    ) {
        (None, None) => true,
        (Some(expected), Some(actual)) => expected.rsplit('.').next() == Some(actual.entity_type()),
        _ => false,
    };
    if !mutability_matches || !parent_matches {
        return Err(
            "stream descriptor relation or mutability differs from verified migration capability"
                .into(),
        );
    }
    Ok(())
}

pub(super) struct HistoricalStreamFacts {
    pub(super) content_hash: String,
    pub(super) byte_length: u64,
    pub(super) content_type: Option<String>,
    pub(super) authorization_parent: Option<StreamEntityRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct BackfillProvenanceEvidenceV1 {
    pub(super) provenance: VerifiedStreamMigrationProvenanceV1,
    pub(super) authorization_parent_type: Option<String>,
}

pub(super) fn historical_stream_facts(
    envelope: &PersistenceEnvelope,
    provenance: &VerifiedStreamMigrationProvenanceV1,
    entity_type: &str,
    authorization_parent_type: Option<&str>,
) -> Result<HistoricalStreamFacts, String> {
    let event: EntityEvent = serde_json::from_value(envelope.payload.clone())
        .map_err(|error| format!("historical stream event is invalid: {error}"))?;
    if event.action != envelope.event_type {
        return Err("historical stream event action differs from its envelope".into());
    }
    let content_hash = event
        .params
        .get(&provenance.content_hash_parameter)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "historical stream event has no mapped content hash".to_string())?
        .to_string();
    let byte_length = event
        .params
        .get(&provenance.byte_length_parameter)
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| {
            "historical stream event has no non-negative mapped byte length".to_string()
        })?;
    let content_type = provenance
        .content_type_parameter
        .as_ref()
        .map(|parameter| {
            event
                .params
                .get(parameter)
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| {
                    format!(
                        "historical stream event has no mapped content type parameter '{parameter}'"
                    )
                })
        })
        .transpose()?;
    let authorization_parent = match (
        provenance.authorization_parent_parameter.as_ref(),
        authorization_parent_type,
    ) {
        (None, None) => None,
        (Some(parameter), Some(parent_type)) => Some(
            StreamEntityRef::new(
                parent_type.rsplit('.').next().unwrap_or(parent_type),
                event
                    .params
                    .get(parameter)
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| {
                        format!(
                            "historical stream event has no mapped parent parameter '{parameter}'"
                        )
                    })?,
            )
            .map_err(|error| error.to_string())?,
        ),
        _ => {
            return Err(format!(
                "stream migration provenance for '{entity_type}' is incomplete"
            ));
        }
    };
    Ok(HistoricalStreamFacts {
        content_hash,
        byte_length,
        content_type,
        authorization_parent,
    })
}

pub(crate) async fn validate_backfill_replay_provenance(
    store: &BoxedEventStore,
    persistence_id: &str,
    descriptor: &StreamDescriptorV1,
) -> Result<(), String> {
    let content_sequence = descriptor.content_event_sequence();
    let descriptor_sequence = descriptor.descriptor_event_sequence();
    let content_events = store
        .read_events_limited(persistence_id, content_sequence.saturating_sub(1), 1)
        .await
        .map_err(|error| error.to_string())?;
    let [content_event] = content_events.as_slice() else {
        return Err("backfill content provenance event is unavailable".into());
    };
    if content_event.sequence_nr != content_sequence {
        return Err("backfill content provenance sequence is inconsistent".into());
    }
    let descriptor_events = store
        .read_events_limited(persistence_id, descriptor_sequence.saturating_sub(1), 1)
        .await
        .map_err(|error| error.to_string())?;
    let [descriptor_event] = descriptor_events.as_slice() else {
        return Err("backfill descriptor event is unavailable".into());
    };
    let backfill_event: EntityEvent = serde_json::from_value(descriptor_event.payload.clone())
        .map_err(|error| format!("backfill descriptor event is invalid: {error}"))?;
    let evidence: BackfillProvenanceEvidenceV1 = serde_json::from_value(
        backfill_event
            .params
            .get("migration_provenance")
            .cloned()
            .ok_or_else(|| {
                "backfill descriptor has no verified migration provenance".to_string()
            })?,
    )
    .map_err(|error| format!("backfill migration provenance is invalid: {error}"))?;
    let expected_event_type = evidence.provenance.publication_action.as_str();
    if content_event.event_type != expected_event_type {
        return Err("backfill content provenance has the wrong event type".into());
    }
    let facts = historical_stream_facts(
        content_event,
        &evidence.provenance,
        descriptor.subject().entity_type(),
        evidence.authorization_parent_type.as_deref(),
    )?;
    if facts.content_hash != descriptor.content_hash()
        || facts.byte_length != descriptor.byte_length()
        || facts.content_type.as_deref() != descriptor.content_type()
        || facts.authorization_parent.as_ref() != descriptor.authorization_parent()
    {
        return Err("backfill descriptor differs from historical event provenance".into());
    }
    let intermediate_count = descriptor_sequence
        .checked_sub(content_sequence)
        .and_then(|distance| distance.checked_sub(1))
        .ok_or_else(|| "backfill descriptor sequence ordering is invalid".to_string())?;
    let intermediate_count = usize::try_from(intermediate_count)
        .map_err(|_| "backfill provenance exceeds platform size".to_string())?;
    if intermediate_count > BACKFILL_HISTORY_EVENT_BUDGET {
        return Err("backfill provenance exceeds its replay budget".into());
    }
    let intermediate = store
        .read_events_limited(persistence_id, content_sequence, intermediate_count)
        .await
        .map_err(|error| error.to_string())?;
    if intermediate.len() != intermediate_count
        || intermediate.iter().enumerate().any(|(offset, event)| {
            u64::try_from(offset)
                .ok()
                .and_then(|offset| content_sequence.checked_add(offset))
                .and_then(|sequence| sequence.checked_add(1))
                != Some(event.sequence_nr)
                || event.event_type == expected_event_type
        })
    {
        return Err("backfill provenance does not cover the latest content publication".into());
    }
    Ok(())
}

pub(super) fn legacy_temper_fs_provenance(
    entity_type: &str,
) -> Result<VerifiedStreamMigrationProvenanceV1, String> {
    let (publication_action, authorization_parent_parameter) = match entity_type {
        "File" => ("StreamUpdated", None),
        "FileVersion" => ("Create", Some("file_id".into())),
        _ => {
            return Err(
                "legacy backfill supports only TemperFS; use the governed operation".into(),
            );
        }
    };
    Ok(VerifiedStreamMigrationProvenanceV1 {
        publication_action: publication_action.into(),
        content_hash_parameter: "content_hash".into(),
        byte_length_parameter: "size_bytes".into(),
        content_type_parameter: Some("mime_type".into()),
        authorization_parent_parameter,
        storage_contract_version: 1,
        storage_key_prefix: "temper-fs/".into(),
    })
}

pub(super) fn legacy_temper_fs_parent_type(entity_type: &str) -> Option<&'static str> {
    (entity_type == "FileVersion").then_some("Temper.FS.File")
}
