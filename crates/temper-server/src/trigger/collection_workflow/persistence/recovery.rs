//! Bounded workflow replay and ambiguous-commit reconciliation.

use temper_runtime::persistence::{PersistenceAppend, PersistenceAppendResult, PersistenceError};

use super::{
    COLLECTION_WORKFLOW_ENTITY_TYPE, CollectionLedgerCommitOutcome, collection_workflow_journal_id,
    decode_record, extract_collection_controls, extract_collection_starts,
};
use crate::storage::BoxedEventStore;
use crate::trigger::collection_workflow::{
    CollectionControlIntentV1, CollectionStartIntentV1, CollectionWorkflowRecordV1,
};

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

/// Read one bounded keyset page of tenant-scoped workflow identities.
///
/// Observe uses this lower-level form so it can inspect no more than its
/// authorization scan budget while still fetching one identity lookahead to
/// decide whether a continuation exists. Payloads remain behind the tenant
/// storage boundary until the caller explicitly loads them.
pub(crate) async fn list_collection_workflow_ids_page(
    store: &BoxedEventStore,
    tenant: &str,
    after_workflow_id: Option<&str>,
    limit: usize,
) -> Result<Vec<String>, PersistenceError> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let after = after_workflow_id.map(|id| (COLLECTION_WORKFLOW_ENTITY_TYPE, id));
    store
        .list_journal_ids_page(tenant, Some(COLLECTION_WORKFLOW_ENTITY_TYPE), after, limit)
        .await
        .map(|ids| ids.into_iter().map(|(_, id)| id).collect())
}

pub(super) enum SourceEvidence<'a> {
    Start(&'a CollectionStartIntentV1),
    Control(&'a CollectionControlIntentV1),
}

pub(super) async fn commit_or_reconcile(
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
