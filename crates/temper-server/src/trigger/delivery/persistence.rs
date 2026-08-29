//! Private delivery-journal persistence helpers.

use super::*;
use crate::storage::BoxedEventStore;
use temper_runtime::persistence::{EventMetadata, PersistenceEnvelope};
use temper_runtime::scheduler::{sim_now, sim_uuid};

/// Persistence ID of the private lifecycle journal for an intent.
pub fn delivery_journal_id(intent: &PersistedReactionIntent) -> String {
    format!(
        "{}:{REACTION_DELIVERY_ENTITY_TYPE}:{}",
        intent.tenant, intent.delivery_id
    )
}

/// Append one fenced lifecycle snapshot to the delivery's private journal.
pub async fn append_delivery_record(
    store: &BoxedEventStore,
    expected_sequence: u64,
    record: &ReactionDeliveryRecord,
) -> Result<u64, PersistenceError> {
    let append = delivery_record_append(expected_sequence, record)?;
    let results = store.append_batch(std::slice::from_ref(&append)).await?;
    Ok(results[0].sequence_nr)
}

pub(crate) fn delivery_record_append(
    expected_sequence: u64,
    record: &ReactionDeliveryRecord,
) -> Result<temper_runtime::persistence::PersistenceAppend, PersistenceError> {
    let payload = serde_json::to_value(record)
        .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
    let persistence_id = delivery_journal_id(&record.intent);
    let envelope = PersistenceEnvelope {
        sequence_nr: expected_sequence + 1,
        event_type: format!("ReactionDelivery::{:?}", record.status),
        payload,
        metadata: EventMetadata {
            event_id: sim_uuid(),
            causation_id: sim_uuid(),
            correlation_id: sim_uuid(),
            timestamp: sim_now(),
            actor_id: persistence_id.clone(),
            kernel: None,
        },
    };
    Ok(temper_runtime::persistence::PersistenceAppend {
        persistence_id,
        expected_sequence,
        events: vec![envelope],
        key_rows: Vec::new(),
        vector_rows: Vec::new(),
        reconcile_vectors: false,
    })
}

pub async fn load_delivery_record(
    store: &BoxedEventStore,
    intent: PersistedReactionIntent,
) -> Result<(ReactionDeliveryRecord, u64), PersistenceError> {
    let persistence_id = delivery_journal_id(&intent);
    let events = store.read_events(&persistence_id, 0).await?;
    let Some(latest) = events.last() else {
        return Ok((ReactionDeliveryRecord::pending(intent), 0));
    };
    let record: ReactionDeliveryRecord = serde_json::from_value(latest.payload.clone())
        .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
    if record.intent.delivery_id != intent.delivery_id || record.intent.tenant != intent.tenant {
        return Err(PersistenceError::Serialization(
            "delivery journal identity does not match source intent".to_string(),
        ));
    }
    Ok((record, latest.sequence_nr))
}

pub async fn initialize_delivery_record(
    store: &BoxedEventStore,
    intent: PersistedReactionIntent,
) -> Result<(), PersistenceError> {
    let (record, sequence) = load_delivery_record(store, intent).await?;
    if sequence != 0 {
        return Ok(());
    }
    match append_delivery_record(store, 0, &record).await {
        Ok(_) | Err(PersistenceError::ConcurrencyViolation { .. }) => Ok(()),
        Err(error) => Err(error),
    }
}

pub async fn list_delivery_records(
    store: &BoxedEventStore,
    tenant: &str,
    limit: usize,
) -> Result<Vec<(ReactionDeliveryRecord, u64)>, PersistenceError> {
    list_delivery_records_page(store, tenant, None, limit).await
}

pub async fn list_delivery_records_page(
    store: &BoxedEventStore,
    tenant: &str,
    after_delivery_id: Option<&str>,
    limit: usize,
) -> Result<Vec<(ReactionDeliveryRecord, u64)>, PersistenceError> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let mut records = Vec::new();
    let mut after = after_delivery_id.map(|delivery_id| {
        (
            REACTION_DELIVERY_ENTITY_TYPE.to_string(),
            delivery_id.to_string(),
        )
    });
    while records.len() < limit {
        let page = store
            .list_journal_ids_page(
                tenant,
                Some(REACTION_DELIVERY_ENTITY_TYPE),
                after
                    .as_ref()
                    .map(|(entity_type, entity_id)| (entity_type.as_str(), entity_id.as_str())),
                limit.saturating_sub(records.len()).max(1),
            )
            .await?;
        if page.is_empty() {
            break;
        }
        after = page.last().cloned();
        for (entity_type, entity_id) in page {
            if entity_type != REACTION_DELIVERY_ENTITY_TYPE {
                continue;
            }
            let persistence_id = format!("{tenant}:{entity_type}:{entity_id}");
            let events = store.read_latest_events(&persistence_id, 1).await?;
            if let Some(latest) = events.last() {
                let record: ReactionDeliveryRecord = serde_json::from_value(latest.payload.clone())
                    .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
                records.push((record, latest.sequence_nr));
                if records.len() >= limit {
                    break;
                }
            }
        }
    }
    Ok(records)
}

pub async fn find_delivery_record(
    store: &BoxedEventStore,
    tenant: &str,
    delivery_id: &str,
) -> Result<Option<(ReactionDeliveryRecord, u64)>, PersistenceError> {
    let persistence_id = format!("{tenant}:{REACTION_DELIVERY_ENTITY_TYPE}:{delivery_id}");
    let events = store.read_latest_events(&persistence_id, 1).await?;
    let Some(latest) = events.last() else {
        return Ok(None);
    };
    let record: ReactionDeliveryRecord = serde_json::from_value(latest.payload.clone())
        .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
    if record.intent.tenant != tenant || record.intent.delivery_id != delivery_id {
        return Err(PersistenceError::Serialization(
            "delivery journal identity does not match request".to_string(),
        ));
    }
    Ok(Some((record, latest.sequence_nr)))
}
