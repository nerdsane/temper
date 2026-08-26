//! Source-journal identity and active-workflow evidence helpers.

use temper_runtime::persistence::{
    EventMetadata, PersistenceAppend, PersistenceEnvelope, PersistenceError,
};
use temper_runtime::scheduler::{sim_now, sim_uuid};
use temper_runtime::tenant::parse_persistence_id_parts;

use super::ACTIVE_COLLECTION_WORKFLOW_FIELD;
use crate::storage::BoxedEventStore;
use crate::trigger::collection_workflow::CollectionWorkflowRecordV1;

const ACTIVE_WORKFLOW_ENTITY_TYPE: &str = "_CollectionWorkflowSource";

fn source_journal_entity_id(record: &CollectionWorkflowRecordV1) -> String {
    record.schema_pin.as_ref().map_or_else(
        || record.source_entity_id.clone(),
        |pin| {
            temper_runtime::persistence::schema_deployment::scoped_journal_entity_id(
                &record.source_entity_id,
                &pin.execution,
            )
        },
    )
}

fn active_workflow_journal_id(record: &CollectionWorkflowRecordV1) -> String {
    format!(
        "{}:{}:{}:{}:{}",
        record.tenant,
        ACTIVE_WORKFLOW_ENTITY_TYPE,
        record.source_entity_type,
        source_journal_entity_id(record),
        record.declaration_name
    )
}

pub(super) async fn load_active_workflow(
    store: &BoxedEventStore,
    record: &CollectionWorkflowRecordV1,
) -> Result<Option<(String, u64)>, PersistenceError> {
    let events = store
        .read_latest_events(&active_workflow_journal_id(record), 1)
        .await?;
    let Some(event) = events.last() else {
        return Ok(None);
    };
    let workflow_id = event
        .payload
        .get(ACTIVE_COLLECTION_WORKFLOW_FIELD)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            PersistenceError::Serialization(
                "active collection workflow pointer is malformed".to_string(),
            )
        })?;
    Ok(Some((workflow_id.to_string(), event.sequence_nr)))
}

pub(super) async fn active_workflow_append(
    store: &BoxedEventStore,
    record: &CollectionWorkflowRecordV1,
) -> Result<PersistenceAppend, PersistenceError> {
    let expected_sequence = load_active_workflow(store, record)
        .await?
        .map_or(0, |(_, sequence)| sequence);
    let persistence_id = active_workflow_journal_id(record);
    Ok(PersistenceAppend {
        persistence_id: persistence_id.clone(),
        expected_sequence,
        events: vec![PersistenceEnvelope {
            sequence_nr: expected_sequence + 1,
            event_type: "CollectionWorkflowSource::ActivatedV1".to_string(),
            payload: serde_json::json!({
                ACTIVE_COLLECTION_WORKFLOW_FIELD: record.workflow_id,
            }),
            metadata: EventMetadata {
                event_id: sim_uuid(),
                causation_id: sim_uuid(),
                correlation_id: sim_uuid(),
                timestamp: sim_now(),
                actor_id: persistence_id,
                kernel: None,
            },
        }],
        key_rows: Vec::new(),
        vector_rows: Vec::new(),
        reconcile_vectors: false,
    })
}

pub(super) fn attach_active_workflow(
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

pub(super) fn ensure_source_journal(
    persistence_id: &str,
    record: &CollectionWorkflowRecordV1,
) -> Result<(), PersistenceError> {
    let (tenant, entity_type, entity_id) =
        parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Serialization)?;
    let expected_entity_id = source_journal_entity_id(record);
    if tenant != record.tenant
        || entity_type != record.source_entity_type
        || entity_id != expected_entity_id
    {
        return Err(PersistenceError::Serialization(
            "collection evidence persistence ID does not match the source identity".to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trigger::collection_workflow::{CollectionWorkflowBudgets, CollectionWorkflowStart};
    use temper_runtime::persistence::schema_deployment::{
        SchemaEventPin, SchemaExecutionPin, SchemaScope, SchemaScopeKind,
    };

    #[tokio::test]
    async fn scoped_active_pointer_uses_canonical_source_identity() {
        let execution = SchemaExecutionPin {
            scope: SchemaScope {
                kind: SchemaScopeKind::Task,
                id: "task-42".to_string(),
            },
            bundle_digest: format!("sha256:{}", "a".repeat(64)),
        };
        let (_, record) = CollectionWorkflowRecordV1::start(CollectionWorkflowStart {
            tenant: "scoped-pointer".to_string(),
            source_entity_type: "Batch".to_string(),
            source_entity_id: "batch-1".to_string(),
            declaration_name: "checks".to_string(),
            source_action: "StartChecks".to_string(),
            source_sequence: 1,
            schema_digest: execution.bundle_digest.clone(),
            schema_pin: Some(SchemaEventPin {
                execution: execution.clone(),
                action_digest: format!("sha256:{}", "b".repeat(64)),
            }),
            authority: serde_json::json!({"principal": "test"}),
            roster: vec!["a".to_string()],
            budgets: CollectionWorkflowBudgets {
                max_members: 1,
                max_concurrency: 1,
                max_attempts: 1,
            },
        })
        .unwrap();
        let store = BoxedEventStore::new(temper_store_sim::SimEventStore::no_faults(42));
        let append = active_workflow_append(&store, &record).await.unwrap();
        let scoped_id = temper_runtime::persistence::schema_deployment::scoped_journal_entity_id(
            "batch-1", &execution,
        );
        assert!(append.persistence_id.contains(&scoped_id));
        store.append_batch(&[append]).await.unwrap();
        assert_eq!(
            load_active_workflow(&store, &record).await.unwrap(),
            Some((record.workflow_id, 1))
        );
    }
}
