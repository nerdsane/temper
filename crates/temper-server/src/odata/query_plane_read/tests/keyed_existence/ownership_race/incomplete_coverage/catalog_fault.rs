use std::sync::Arc;

use async_trait::async_trait;

use super::*;
use crate::odata::read_support::{
    AuthoritativeMaterializationError, CatalogMaterializationPolicy,
    materialize_entity_set_entities,
};
use crate::storage::{EntityCatalogRow, QueryPlaneStore, QueryProjectionFieldsRow, StorageStack};

struct CatalogReadFault {
    durable_row: EntityCatalogRow,
}

#[async_trait]
impl QueryPlaneStore for CatalogReadFault {
    async fn upsert_projection(
        &self,
        _tenant: &str,
        _entity_type: &str,
        _entity_id: &str,
        _status: &str,
        _fields: &serde_json::Value,
        _state: &serde_json::Value,
        _sequence_nr: u64,
    ) -> Result<(), PersistenceError> {
        Ok(())
    }

    async fn remove_projection(
        &self,
        _tenant: &str,
        _entity_type: &str,
        _entity_id: &str,
    ) -> Result<(), PersistenceError> {
        Ok(())
    }

    async fn query_field_index(
        &self,
        _tenant: &str,
        _entity_type: &str,
        _where_clause: &str,
        _params: Vec<String>,
    ) -> Result<Option<Vec<String>>, PersistenceError> {
        Ok(None)
    }

    async fn load_projection_fields_many(
        &self,
        _tenant: &str,
        _entity_type: &str,
        _entity_ids: &[String],
        _field_names: &[&str],
    ) -> Result<Option<Vec<QueryProjectionFieldsRow>>, PersistenceError> {
        Ok(None)
    }

    async fn load_entity_catalog_rows(
        &self,
        _tenant: &str,
        _entity_type: &str,
        entity_ids: &[String],
    ) -> Result<Option<Vec<EntityCatalogRow>>, PersistenceError> {
        assert!(
            entity_ids.contains(&self.durable_row.entity_id),
            "the durable catalog-only candidate must be requested"
        );
        Err(PersistenceError::Storage(
            "injected catalog read fault".to_string(),
        ))
    }

    async fn projected_entity_counts_by_tenant(
        &self,
    ) -> Result<Option<Vec<(String, u64)>>, PersistenceError> {
        Ok(None)
    }
}

#[tokio::test]
async fn catalog_fault_cannot_turn_a_catalog_only_candidate_into_actor_state() {
    let (_guard, _clock, _ids) = install_deterministic_context(264);
    let tenant = TenantId::default();
    let entity_id = "ord-catalog-read-fault";
    let persistence_id = format!("{tenant}:Order:{entity_id}");
    let events = SimEventStore::no_faults(264);
    let query_plane = Arc::new(CatalogReadFault {
        durable_row: EntityCatalogRow {
            entity_id: entity_id.to_string(),
            status: "Draft".to_string(),
            fields: serde_json::json!({
                "Id": entity_id,
                "WorkspaceId": "ws-catalog-fault",
                "Path": "/catalog-only",
            }),
            state: None,
            sequence_nr: 1,
        },
    });
    let mut state = build_order_state("catalog-only-read-fault");
    state.set_storage_stack(StorageStack::new(
        BackendLabel::Sim,
        BoxedEventStore::new(events.clone()),
        None,
        None,
        None,
        None,
        Some(query_plane),
        None,
        None,
        None,
    ));

    let materialized = materialize_entity_set_entities(
        &state,
        &tenant,
        "Order",
        "Orders",
        &[entity_id.to_string()],
        CatalogMaterializationPolicy::JournalAbsentOnly,
        None,
    )
    .await;

    assert!(materialized.entities.is_empty());
    assert_eq!(
        materialized.error,
        Some(AuthoritativeMaterializationError::JournalUnstable),
        "a catalog read failure must remain retryable, not fall through to a fabricated actor body"
    );
    assert_eq!(
        EventStore::journal_boundary(&events, &persistence_id)
            .await
            .expect("journal remains readable")
            .latest_sequence,
        0,
        "the compatibility fallback must not write a first journal generation"
    );
}

#[tokio::test]
async fn catalog_fault_does_not_disable_journal_backed_materialization() {
    let (_guard, _clock, _ids) = install_deterministic_context(266);
    let tenant = TenantId::default();
    let workspace = "ws-journal-with-catalog-fault";
    let journal_path = "/journal-authority";
    let entity_id = "ord-journal-with-catalog-fault";
    let persistence_id = format!("{tenant}:Order:{entity_id}");
    let events = SimEventStore::no_faults(266);

    EventStore::append_with_index_rows(
        &events,
        &persistence_id,
        0,
        &[super::source_transitions::complete_field_update(
            &persistence_id,
            entity_id,
            workspace,
            journal_path,
            "journal-with-catalog-fault",
        )],
        &[key_row(workspace, journal_path)],
        &[],
        IndexReconciliation {
            keys: true,
            key_set_signature: Some(ORDER_KEY_SET_SIGNATURE.to_string()),
            vectors: false,
        },
    )
    .await
    .expect("seed authoritative journal generation");

    let query_plane = Arc::new(CatalogReadFault {
        durable_row: EntityCatalogRow {
            entity_id: entity_id.to_string(),
            status: "Draft".to_string(),
            fields: serde_json::json!({
                "Id": entity_id,
                "WorkspaceId": workspace,
                "Path": "/stale-catalog",
            }),
            state: None,
            sequence_nr: 1,
        },
    });
    let mut state = build_order_state("journal-backed-catalog-fault");
    state.set_storage_stack(StorageStack::new(
        BackendLabel::Sim,
        BoxedEventStore::new(events),
        None,
        None,
        None,
        None,
        Some(query_plane),
        None,
        None,
        None,
    ));

    let materialized = materialize_entity_set_entities(
        &state,
        &tenant,
        "Order",
        "Orders",
        &[entity_id.to_string()],
        CatalogMaterializationPolicy::JournalAbsentOnly,
        None,
    )
    .await;

    assert_eq!(materialized.error, None);
    assert_eq!(materialized.entities.len(), 1);
    assert_eq!(materialized.entities[0]["fields"]["Path"], journal_path);
}
