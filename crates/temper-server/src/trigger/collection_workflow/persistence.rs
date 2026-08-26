//! Atomic event-store persistence and bounded replay for the private ledger.

mod recovery;
mod source;

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
use recovery::{SourceEvidence, commit_or_reconcile};
pub(crate) use recovery::{
    list_collection_records_page, list_collection_workflow_ids_page, load_collection_record,
};
use source::active_workflow_append;
use source::{attach_active_workflow, ensure_source_journal};

/// Reserved source-event field containing normalized collection starts.
pub(crate) const COLLECTION_START_INTENTS_FIELD: &str = "_temper_collection_starts_v1";
/// Reserved source-event field containing normalized collection controls.
pub(crate) const COLLECTION_CONTROL_INTENTS_FIELD: &str = "_temper_collection_controls_v1";
/// Reserved replay field retaining the active workflow identity.
pub(crate) const ACTIVE_COLLECTION_WORKFLOW_FIELD: &str = "_temper_active_collection_workflow_v1";
/// Private synthetic entity type used for one workflow journal.
pub(crate) const COLLECTION_WORKFLOW_ENTITY_TYPE: &str = "_CollectionWorkflow";
/// Maximum private workflow snapshots inspected to recover one owned intent.
const MAX_COLLECTION_WORKFLOW_EVENTS: usize = 1_024;

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
    source_append: PersistenceAppend,
    intent: &CollectionStartIntentV1,
    record: &CollectionWorkflowRecordV1,
) -> Result<CollectionLedgerCommitOutcome, PersistenceError> {
    commit_collection_start_with_intents(store, source_append, intent, record, &[], &[]).await
}

pub(super) async fn commit_collection_start_with_intents(
    store: &BoxedEventStore,
    mut source_append: PersistenceAppend,
    intent: &CollectionStartIntentV1,
    record: &CollectionWorkflowRecordV1,
    intents: &[crate::trigger::delivery::PersistedReactionIntent],
    extra_appends: &[PersistenceAppend],
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
    let mut workflow_append = workflow_append(record, 0, "CollectionWorkflow::StartedV1")?;
    crate::trigger::delivery::attach_intents(&mut workflow_append.events[0].payload, intents)
        .map_err(PersistenceError::Serialization)?;
    let mut appends = Vec::with_capacity(2 + extra_appends.len());
    appends.push(source_append);
    appends.push(workflow_append);
    appends.extend_from_slice(extra_appends);
    appends.push(active_workflow_append(store, record).await?);
    commit_or_reconcile(store, &appends, SourceEvidence::Start(intent), record).await
}

/// Atomically commit a source control event and its fenced workflow snapshot.
pub(crate) async fn commit_collection_control(
    store: &BoxedEventStore,
    source_append: PersistenceAppend,
    intent: &CollectionControlIntentV1,
    expected_workflow_sequence: u64,
    record: &CollectionWorkflowRecordV1,
) -> Result<CollectionLedgerCommitOutcome, PersistenceError> {
    commit_collection_control_with_intents(
        store,
        source_append,
        intent,
        expected_workflow_sequence,
        record,
        &[],
        &[],
    )
    .await
}

pub(super) async fn commit_collection_control_with_intents(
    store: &BoxedEventStore,
    mut source_append: PersistenceAppend,
    intent: &CollectionControlIntentV1,
    expected_workflow_sequence: u64,
    record: &CollectionWorkflowRecordV1,
    intents: &[crate::trigger::delivery::PersistedReactionIntent],
    delivery_appends: &[PersistenceAppend],
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
        && record.control_schema_pin == intent.schema_pin
        && record.control_timeout_delivery_id == intent.timeout_delivery_id;
    let ignored_after_first = record.last_control_id.is_some()
        && record.requested_outcome.is_some()
        && record.last_control_id.as_deref() != Some(intent.control_id.as_str());
    let timeout_receipt_matches = match intent.requested_outcome {
        super::CollectionRequestedOutcome::Cancelled => intent.timeout_delivery_id.is_none(),
        super::CollectionRequestedOutcome::TimedOut => {
            let receipt =
                crate::trigger::delivery::extract_receipt(&source_append.events[0].payload)
                    .map_err(PersistenceError::Serialization)?;
            let binding = record.timeout_binding.as_ref();
            receipt.as_ref().is_some_and(|receipt| {
                Some(receipt.delivery_id.as_str()) == intent.timeout_delivery_id.as_deref()
                    && receipt.state_timeout_state.as_deref()
                        == binding.map(|binding| binding.state.as_str())
                    && receipt.schema_pin == intent.schema_pin
            })
        }
    };
    if intent.workflow_id != record.workflow_id
        || intent.control_id != expected_control_id
        || intent.control_epoch != record.control_epoch
        || (!first_control_matches && !ignored_after_first)
        || source_append.events.len() != 1
        || source_append.expected_sequence + 1 != intent.source_sequence
        || source_append.events[0].event_type != intent.source_action
        || !timeout_receipt_matches
    {
        return Err(PersistenceError::Serialization(
            "collection control intent does not match source or workflow record".to_string(),
        ));
    }
    attach_collection_control(&mut source_append.events[0].payload, intent)
        .map_err(PersistenceError::Serialization)?;
    let mut workflow_append = workflow_append(
        record,
        expected_workflow_sequence,
        "CollectionWorkflow::ControlledV1",
    )?;
    crate::trigger::delivery::attach_intents(&mut workflow_append.events[0].payload, intents)
        .map_err(PersistenceError::Serialization)?;
    let mut appends = Vec::with_capacity(2 + delivery_appends.len());
    appends.push(source_append);
    appends.push(workflow_append);
    appends.extend_from_slice(delivery_appends);
    commit_or_reconcile(store, &appends, SourceEvidence::Control(intent), record).await
}

/// Find one collection-owned intent in the bounded private workflow history.
pub(crate) async fn find_collection_intent(
    store: &BoxedEventStore,
    record: &CollectionWorkflowRecordV1,
    delivery_id: &str,
) -> Result<Option<crate::trigger::delivery::PersistedReactionIntent>, PersistenceError> {
    let persistence_id = collection_workflow_journal_id(&record.tenant, &record.workflow_id);
    let events = store
        .read_events_limited(&persistence_id, 0, MAX_COLLECTION_WORKFLOW_EVENTS)
        .await?;
    for event in events.iter().rev() {
        let intents = crate::trigger::delivery::extract_intents(&event.payload)
            .map_err(PersistenceError::Serialization)?;
        if let Some(intent) = intents
            .into_iter()
            .find(|intent| intent.delivery_id == delivery_id)
        {
            return Ok(Some(intent));
        }
    }
    Ok(None)
}

/// Find a bounded set of collection-owned intents with one private-history scan.
pub(crate) async fn find_collection_intents(
    store: &BoxedEventStore,
    record: &CollectionWorkflowRecordV1,
    delivery_ids: &std::collections::BTreeSet<String>,
) -> Result<
    std::collections::BTreeMap<String, crate::trigger::delivery::PersistedReactionIntent>,
    PersistenceError,
> {
    let persistence_id = collection_workflow_journal_id(&record.tenant, &record.workflow_id);
    let events = store
        .read_events_limited(&persistence_id, 0, MAX_COLLECTION_WORKFLOW_EVENTS)
        .await?;
    let mut found = std::collections::BTreeMap::new();
    for event in events.iter().rev() {
        let intents = crate::trigger::delivery::extract_intents(&event.payload)
            .map_err(PersistenceError::Serialization)?;
        for intent in intents {
            if delivery_ids.contains(&intent.delivery_id) {
                found.entry(intent.delivery_id.clone()).or_insert(intent);
            }
        }
        if found.len() == delivery_ids.len() {
            break;
        }
    }
    Ok(found)
}

/// Recover the active workflow identity from its dedicated atomic pointer.
pub(super) async fn active_source_workflow_id(
    store: &BoxedEventStore,
    record: &CollectionWorkflowRecordV1,
) -> Result<Option<String>, PersistenceError> {
    source::load_active_workflow(store, record)
        .await
        .map(|active| active.map(|(workflow_id, _)| workflow_id))
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

/// Append a workflow snapshot with the durable delivery intents it created.
pub(crate) async fn append_collection_step_idempotent(
    store: &BoxedEventStore,
    expected_sequence: u64,
    event_type: &str,
    record: &CollectionWorkflowRecordV1,
    intents: &[crate::trigger::delivery::PersistedReactionIntent],
) -> Result<(CollectionMutationOutcome, u64), PersistenceError> {
    record.validate().map_err(PersistenceError::Serialization)?;
    let mut append = workflow_append(record, expected_sequence, event_type)?;
    crate::trigger::delivery::attach_intents(&mut append.events[0].payload, intents)
        .map_err(PersistenceError::Serialization)?;
    match store.append_batch(std::slice::from_ref(&append)).await {
        Ok(results) => Ok((CollectionMutationOutcome::Applied, results[0].sequence_nr)),
        Err(error) => {
            let events = store
                .read_events_limited(&append.persistence_id, expected_sequence, 1)
                .await?;
            let Some(event) = events.first() else {
                return Err(error);
            };
            let same_record = decode_record(&event.payload)? == *record;
            let same_intents = crate::trigger::delivery::extract_intents(&event.payload)
                .map_err(PersistenceError::Serialization)?
                == intents;
            if same_record && same_intents {
                Ok((CollectionMutationOutcome::Replayed, event.sequence_nr))
            } else {
                Err(error)
            }
        }
    }
}

/// Atomically persist a terminal delivery and its workflow aggregation.
pub(crate) async fn commit_collection_delivery_outcome(
    store: &BoxedEventStore,
    expected_delivery_sequence: u64,
    delivery: &crate::trigger::delivery::ReactionDeliveryRecord,
    expected_workflow_sequence: u64,
    record: &CollectionWorkflowRecordV1,
    continuation: &[crate::trigger::delivery::PersistedReactionIntent],
) -> Result<Vec<PersistenceAppendResult>, PersistenceError> {
    record.validate().map_err(PersistenceError::Serialization)?;
    if !delivery.status.is_terminal() || delivery.intent.collection.is_none() {
        return Err(PersistenceError::Serialization(
            "collection outcome requires a terminal bound delivery".to_string(),
        ));
    }
    let delivery_append =
        crate::trigger::delivery::delivery_record_append(expected_delivery_sequence, delivery)?;
    let mut workflow_append = workflow_append(
        record,
        expected_workflow_sequence,
        "CollectionWorkflow::DeliveryTerminalV1",
    )?;
    crate::trigger::delivery::attach_intents(&mut workflow_append.events[0].payload, continuation)
        .map_err(PersistenceError::Serialization)?;
    store
        .append_batch(&[delivery_append, workflow_append])
        .await
}

use super::CollectionMutationOutcome;

/// Build one validated private workflow journal append.
pub(crate) fn workflow_append(
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
        key_rows: Vec::new(),
        vector_rows: Vec::new(),
        reconcile_vectors: false,
    })
}

pub(super) fn decode_record(
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
