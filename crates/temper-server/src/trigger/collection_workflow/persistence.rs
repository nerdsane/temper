//! Atomic event-store persistence and bounded replay for the private ledger.

use temper_runtime::persistence::{
    EventMetadata, PersistenceAppend, PersistenceAppendResult, PersistenceEnvelope,
    PersistenceError,
};
use temper_runtime::scheduler::{sim_now, sim_uuid};
use temper_runtime::tenant::parse_persistence_id_parts;

use super::{
    COLLECTION_LEDGER_VERSION, CollectionControlIntentV1, CollectionStartIntentV1,
    CollectionWorkflowRecordV1, CollectionWorkflowStart, collection_control_id,
};
use crate::storage::BoxedEventStore;

/// Reserved source-event field containing normalized collection starts.
pub(crate) const COLLECTION_START_INTENTS_FIELD: &str = "_temper_collection_starts_v1";
/// Reserved source-event field containing normalized collection controls.
pub(crate) const COLLECTION_CONTROL_INTENTS_FIELD: &str = "_temper_collection_controls_v1";
/// Reserved replay field retaining the active workflow identity.
pub(crate) const ACTIVE_COLLECTION_WORKFLOW_FIELD: &str = "_temper_active_collection_workflow_v1";
/// Private synthetic entity type used for one workflow journal.
pub(crate) const COLLECTION_WORKFLOW_ENTITY_TYPE: &str = "_CollectionWorkflow";

/// Outcome of an atomic commit whose prior result may have been ambiguous.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CollectionLedgerCommitOutcome {
    Committed(Vec<PersistenceAppendResult>),
    Reconciled(Vec<PersistenceAppendResult>),
}

/// Attach one normalized start intent and active workflow ID to a source event.
pub(crate) fn attach_collection_start(
    payload: &mut serde_json::Value,
    intent: &CollectionStartIntentV1,
) -> Result<(), String> {
    ensure_supported_version(intent.version)?;
    let object = payload
        .as_object_mut()
        .ok_or_else(|| "entity event payload must be an object".to_string())?;
    let encoded = serde_json::to_value(intent).map_err(|error| error.to_string())?;
    match object.get_mut(COLLECTION_START_INTENTS_FIELD) {
        None => {
            object.insert(
                COLLECTION_START_INTENTS_FIELD.to_string(),
                serde_json::Value::Array(vec![encoded]),
            );
        }
        Some(serde_json::Value::Array(intents)) if intents.is_empty() => intents.push(encoded),
        Some(serde_json::Value::Array(intents)) if intents.len() == 1 && intents[0] == encoded => {}
        Some(_) => {
            return Err("collection start evidence must contain exactly one intent".to_string());
        }
    }
    attach_active_workflow(object, &intent.workflow_id)?;
    Ok(())
}

/// Decode normalized start intents from a replayed source event.
pub(crate) fn extract_collection_starts(
    payload: &serde_json::Value,
) -> Result<Vec<CollectionStartIntentV1>, String> {
    let Some(value) = payload.get(COLLECTION_START_INTENTS_FIELD) else {
        return Ok(Vec::new());
    };
    let intents: Vec<CollectionStartIntentV1> =
        serde_json::from_value(value.clone()).map_err(|error| error.to_string())?;
    if intents.len() > 1 {
        return Err("collection start evidence contains multiple intents".to_string());
    }
    for intent in &intents {
        ensure_supported_version(intent.version)?;
    }
    Ok(intents)
}

/// Attach one normalized control intent while retaining its workflow ID.
pub(crate) fn attach_collection_control(
    payload: &mut serde_json::Value,
    intent: &CollectionControlIntentV1,
) -> Result<(), String> {
    ensure_supported_version(intent.version)?;
    let object = payload
        .as_object_mut()
        .ok_or_else(|| "entity event payload must be an object".to_string())?;
    let encoded = serde_json::to_value(intent).map_err(|error| error.to_string())?;
    match object.get_mut(COLLECTION_CONTROL_INTENTS_FIELD) {
        None => {
            object.insert(
                COLLECTION_CONTROL_INTENTS_FIELD.to_string(),
                serde_json::Value::Array(vec![encoded]),
            );
        }
        Some(serde_json::Value::Array(intents)) if intents.is_empty() => intents.push(encoded),
        Some(serde_json::Value::Array(intents)) if intents.len() == 1 && intents[0] == encoded => {}
        Some(_) => {
            return Err("collection control evidence must contain exactly one intent".to_string());
        }
    }
    attach_active_workflow(object, &intent.workflow_id)?;
    Ok(())
}

/// Decode normalized control intents from a replayed source event.
pub(crate) fn extract_collection_controls(
    payload: &serde_json::Value,
) -> Result<Vec<CollectionControlIntentV1>, String> {
    let Some(value) = payload.get(COLLECTION_CONTROL_INTENTS_FIELD) else {
        return Ok(Vec::new());
    };
    let intents: Vec<CollectionControlIntentV1> =
        serde_json::from_value(value.clone()).map_err(|error| error.to_string())?;
    if intents.len() > 1 {
        return Err("collection control evidence contains multiple intents".to_string());
    }
    for intent in &intents {
        ensure_supported_version(intent.version)?;
    }
    Ok(intents)
}

/// Persistence ID of the private lifecycle journal.
pub(crate) fn collection_workflow_journal_id(tenant: &str, workflow_id: &str) -> String {
    format!("{tenant}:{COLLECTION_WORKFLOW_ENTITY_TYPE}:{workflow_id}")
}

/// Atomically commit a source start event and the initial `Running` snapshot.
pub(crate) async fn commit_collection_start(
    store: &BoxedEventStore,
    mut source_append: PersistenceAppend,
    intent: &CollectionStartIntentV1,
    record: &CollectionWorkflowRecordV1,
) -> Result<CollectionLedgerCommitOutcome, PersistenceError> {
    record.validate().map_err(PersistenceError::Serialization)?;
    ensure_source_journal(&source_append.persistence_id, record)?;
    ensure_supported_version(intent.version).map_err(PersistenceError::Serialization)?;
    let expected_start = CollectionWorkflowStart {
        tenant: record.tenant.clone(),
        source_entity_type: record.source_entity_type.clone(),
        source_entity_id: record.source_entity_id.clone(),
        declaration_name: record.declaration_name.clone(),
        source_action: record.source_action.clone(),
        source_sequence: record.source_sequence,
        schema_digest: record.schema_digest.clone(),
        schema_pin: record.schema_pin.clone(),
        authority: record.original_authority.clone(),
        roster: record.sealed_roster.clone(),
        budgets: record.budgets,
    };
    if intent.workflow_id != record.workflow_id || intent.start != expected_start {
        return Err(PersistenceError::Serialization(
            "collection start intent does not match workflow record".to_string(),
        ));
    }
    if source_append.events.len() != 1
        || source_append.expected_sequence + 1 != record.source_sequence
        || source_append.events[0].event_type != record.source_action
    {
        return Err(PersistenceError::Serialization(
            "collection start requires exactly one matching source event".to_string(),
        ));
    }
    attach_collection_start(&mut source_append.events[0].payload, intent)
        .map_err(PersistenceError::Serialization)?;
    let workflow_append = workflow_append(record, 0, "CollectionWorkflow::StartedV1")?;
    commit_or_reconcile(
        store,
        &[source_append, workflow_append],
        SourceEvidence::Start(intent),
        record,
    )
    .await
}

/// Atomically commit a source control event and its fenced workflow snapshot.
pub(crate) async fn commit_collection_control(
    store: &BoxedEventStore,
    mut source_append: PersistenceAppend,
    intent: &CollectionControlIntentV1,
    expected_workflow_sequence: u64,
    record: &CollectionWorkflowRecordV1,
) -> Result<CollectionLedgerCommitOutcome, PersistenceError> {
    record.validate().map_err(PersistenceError::Serialization)?;
    ensure_source_journal(&source_append.persistence_id, record)?;
    ensure_supported_version(intent.version).map_err(PersistenceError::Serialization)?;
    let expected_control_id = collection_control_id(
        &record.workflow_id,
        &intent.source_action,
        intent.source_sequence,
        intent.requested_outcome.identity_component(),
    );
    let first_control_matches = record.last_control_id.as_deref()
        == Some(intent.control_id.as_str())
        && record.requested_outcome == Some(intent.requested_outcome)
        && record.control_source_action.as_deref() == Some(intent.source_action.as_str())
        && record.control_source_sequence == Some(intent.source_sequence)
        && record.control_authority.as_ref() == Some(&intent.authority)
        && record.control_schema_pin == intent.schema_pin;
    let ignored_after_first = record.last_control_id.is_some()
        && record.requested_outcome.is_some()
        && record.last_control_id.as_deref() != Some(intent.control_id.as_str());
    if intent.workflow_id != record.workflow_id
        || intent.control_id != expected_control_id
        || intent.control_epoch != record.control_epoch
        || (!first_control_matches && !ignored_after_first)
        || source_append.events.len() != 1
        || source_append.expected_sequence + 1 != intent.source_sequence
        || source_append.events[0].event_type != intent.source_action
    {
        return Err(PersistenceError::Serialization(
            "collection control intent does not match source or workflow record".to_string(),
        ));
    }
    attach_collection_control(&mut source_append.events[0].payload, intent)
        .map_err(PersistenceError::Serialization)?;
    let workflow_append = workflow_append(
        record,
        expected_workflow_sequence,
        "CollectionWorkflow::ControlledV1",
    )?;
    commit_or_reconcile(
        store,
        &[source_append, workflow_append],
        SourceEvidence::Control(intent),
        record,
    )
    .await
}

/// Append one lifecycle snapshot, accepting an identical concurrent append.
pub(crate) async fn append_collection_record_idempotent(
    store: &BoxedEventStore,
    expected_sequence: u64,
    event_type: &str,
    record: &CollectionWorkflowRecordV1,
) -> Result<(CollectionMutationOutcome, u64), PersistenceError> {
    record.validate().map_err(PersistenceError::Serialization)?;
    let append = workflow_append(record, expected_sequence, event_type)?;
    match store.append_batch(std::slice::from_ref(&append)).await {
        Ok(results) => Ok((CollectionMutationOutcome::Applied, results[0].sequence_nr)),
        Err(error) => {
            let events = store
                .read_events_limited(&append.persistence_id, expected_sequence, 1)
                .await?;
            let Some(event) = events.first() else {
                return Err(error);
            };
            if decode_record(&event.payload)? == *record {
                Ok((CollectionMutationOutcome::Replayed, event.sequence_nr))
            } else {
                Err(error)
            }
        }
    }
}

use super::CollectionMutationOutcome;

/// Load and validate the latest workflow snapshot with a one-event bound.
pub(crate) async fn load_collection_record(
    store: &BoxedEventStore,
    tenant: &str,
    workflow_id: &str,
) -> Result<Option<(CollectionWorkflowRecordV1, u64)>, PersistenceError> {
    let persistence_id = collection_workflow_journal_id(tenant, workflow_id);
    let events = store.read_latest_events(&persistence_id, 1).await?;
    let Some(event) = events.last() else {
        return Ok(None);
    };
    let record = decode_record(&event.payload)?;
    if record.tenant != tenant || record.workflow_id != workflow_id {
        return Err(PersistenceError::Serialization(
            "collection journal identity does not match payload".to_string(),
        ));
    }
    Ok(Some((record, event.sequence_nr)))
}

/// Read one bounded keyset page of private workflow snapshots.
pub(crate) async fn list_collection_records_page(
    store: &BoxedEventStore,
    tenant: &str,
    after_workflow_id: Option<&str>,
    limit: usize,
) -> Result<Vec<(CollectionWorkflowRecordV1, u64)>, PersistenceError> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let after = after_workflow_id.map(|id| (COLLECTION_WORKFLOW_ENTITY_TYPE, id));
    let ids = store
        .list_journal_ids_page(tenant, Some(COLLECTION_WORKFLOW_ENTITY_TYPE), after, limit)
        .await?;
    let mut records = Vec::with_capacity(ids.len());
    for (_, workflow_id) in ids {
        let Some(record) = load_collection_record(store, tenant, &workflow_id).await? else {
            return Err(PersistenceError::Storage(
                "indexed collection journal has no lifecycle event".to_string(),
            ));
        };
        records.push(record);
    }
    Ok(records)
}

enum SourceEvidence<'a> {
    Start(&'a CollectionStartIntentV1),
    Control(&'a CollectionControlIntentV1),
}

async fn commit_or_reconcile(
    store: &BoxedEventStore,
    appends: &[PersistenceAppend],
    evidence: SourceEvidence<'_>,
    record: &CollectionWorkflowRecordV1,
) -> Result<CollectionLedgerCommitOutcome, PersistenceError> {
    match store.append_batch(appends).await {
        Ok(results) => Ok(CollectionLedgerCommitOutcome::Committed(results)),
        Err(error) => {
            let source = &appends[0];
            let committed_source = store
                .read_events_limited(&source.persistence_id, source.expected_sequence, 1)
                .await?
                .into_iter()
                .next();
            let Some(source_event) = committed_source else {
                return Err(error);
            };
            let source_matches = match evidence {
                SourceEvidence::Start(intent) => extract_collection_starts(&source_event.payload)
                    .map_err(PersistenceError::Serialization)?
                    .iter()
                    .any(|found| found == intent),
                SourceEvidence::Control(intent) => {
                    extract_collection_controls(&source_event.payload)
                        .map_err(PersistenceError::Serialization)?
                        .iter()
                        .any(|found| found == intent)
                }
            };
            let workflow_events = store
                .read_events_limited(&appends[1].persistence_id, appends[1].expected_sequence, 1)
                .await?;
            let workflow_event = workflow_events.first();
            let workflow_matches = workflow_event
                .map(|event| decode_record(&event.payload))
                .transpose()?
                .is_some_and(|found| found == *record);
            if source_matches && workflow_matches {
                Ok(CollectionLedgerCommitOutcome::Reconciled(vec![
                    PersistenceAppendResult {
                        persistence_id: source.persistence_id.clone(),
                        sequence_nr: source_event.sequence_nr,
                    },
                    PersistenceAppendResult {
                        persistence_id: appends[1].persistence_id.clone(),
                        sequence_nr: workflow_event.map_or(0, |event| event.sequence_nr),
                    },
                ]))
            } else {
                Err(error)
            }
        }
    }
}

fn workflow_append(
    record: &CollectionWorkflowRecordV1,
    expected_sequence: u64,
    event_type: &str,
) -> Result<PersistenceAppend, PersistenceError> {
    let persistence_id = collection_workflow_journal_id(&record.tenant, &record.workflow_id);
    let payload = serde_json::to_value(record)
        .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
    Ok(PersistenceAppend {
        persistence_id: persistence_id.clone(),
        expected_sequence,
        events: vec![PersistenceEnvelope {
            sequence_nr: expected_sequence + 1,
            event_type: event_type.to_string(),
            payload,
            metadata: EventMetadata {
                event_id: sim_uuid(),
                causation_id: sim_uuid(),
                correlation_id: sim_uuid(),
                timestamp: sim_now(),
                actor_id: persistence_id,
            },
        }],
    })
}

fn decode_record(
    payload: &serde_json::Value,
) -> Result<CollectionWorkflowRecordV1, PersistenceError> {
    let version = payload
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| {
            PersistenceError::Serialization(
                "collection workflow record has no numeric version".to_string(),
            )
        })?;
    ensure_supported_version(version).map_err(PersistenceError::Serialization)?;
    let record: CollectionWorkflowRecordV1 = serde_json::from_value(payload.clone())
        .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
    record.validate().map_err(PersistenceError::Serialization)?;
    Ok(record)
}

fn ensure_supported_version(version: impl Into<u64>) -> Result<(), String> {
    let version = version.into();
    if version != u64::from(COLLECTION_LEDGER_VERSION) {
        return Err(format!("unsupported collection ledger version {version}"));
    }
    Ok(())
}

fn attach_active_workflow(
    object: &mut serde_json::Map<String, serde_json::Value>,
    workflow_id: &str,
) -> Result<(), String> {
    match object.get(ACTIVE_COLLECTION_WORKFLOW_FIELD) {
        None => {
            object.insert(
                ACTIVE_COLLECTION_WORKFLOW_FIELD.to_string(),
                serde_json::Value::String(workflow_id.to_string()),
            );
            Ok(())
        }
        Some(serde_json::Value::String(existing)) if existing == workflow_id => Ok(()),
        Some(_) => Err("active collection workflow evidence is contradictory".to_string()),
    }
}

fn ensure_source_journal(
    persistence_id: &str,
    record: &CollectionWorkflowRecordV1,
) -> Result<(), PersistenceError> {
    let (tenant, entity_type, entity_id) =
        parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Serialization)?;
    if tenant != record.tenant
        || entity_type != record.source_entity_type
        || entity_id != record.source_entity_id
    {
        return Err(PersistenceError::Serialization(
            "collection evidence persistence ID does not match the source identity".to_string(),
        ));
    }
    Ok(())
}
