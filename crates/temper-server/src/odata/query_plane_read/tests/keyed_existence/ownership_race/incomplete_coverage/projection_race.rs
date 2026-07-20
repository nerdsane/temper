use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;

use super::*;
use crate::entity_actor::EntityEvent;
use crate::storage::{
    BackendLabel, BoxedEventStore, EntityCatalogRow, QueryPlaneStore, QueryProjectionFieldsRow,
};
use temper_runtime::persistence::PersistenceError;

#[derive(Clone)]
struct TombstoneOnUpsertQueryPlane {
    inner: Arc<SimQueryPlane>,
    events: SimEventStore,
    persistence_id: String,
    fired: Arc<AtomicBool>,
}

impl TombstoneOnUpsertQueryPlane {
    fn tombstone(&self) -> PersistenceEnvelope {
        let timestamp = sim_now();
        let event = EntityEvent {
            action: "Delete".to_string(),
            from_status: "Draft".to_string(),
            to_status: "Deleted".to_string(),
            timestamp,
            params: serde_json::json!({}),
            idempotency_key: Some("projection-race-delete".to_string()),
        };
        PersistenceEnvelope {
            sequence_nr: 0,
            event_type: "Delete".to_string(),
            payload: serde_json::to_value(event).expect("serialize projection-race tombstone"),
            metadata: EventMetadata {
                event_id: sim_uuid(),
                causation_id: sim_uuid(),
                correlation_id: sim_uuid(),
                timestamp,
                actor_id: self.persistence_id.clone(),
            },
        }
    }
}

#[async_trait]
impl QueryPlaneStore for TombstoneOnUpsertQueryPlane {
    async fn upsert_projection(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        status: &str,
        fields: &serde_json::Value,
        state: &serde_json::Value,
        sequence_nr: u64,
    ) -> Result<(), PersistenceError> {
        if !self.fired.swap(true, Ordering::SeqCst) {
            EventStore::append_with_index_rows(
                &self.events,
                &self.persistence_id,
                sequence_nr,
                &[self.tombstone()],
                &[],
                &[],
                IndexReconciliation {
                    keys: true,
                    key_set_signature: Some(ORDER_KEY_SET_SIGNATURE.to_string()),
                    vectors: false,
                },
            )
            .await?;
            QueryPlaneStore::remove_projection(self.inner.as_ref(), tenant, entity_type, entity_id)
                .await?;
        }
        QueryPlaneStore::upsert_projection(
            self.inner.as_ref(),
            tenant,
            entity_type,
            entity_id,
            status,
            fields,
            state,
            sequence_nr,
        )
        .await
    }

    async fn remove_projection(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<(), PersistenceError> {
        QueryPlaneStore::remove_projection(self.inner.as_ref(), tenant, entity_type, entity_id)
            .await
    }

    async fn query_field_index(
        &self,
        tenant: &str,
        entity_type: &str,
        where_clause: &str,
        params: Vec<String>,
    ) -> Result<Option<Vec<String>>, PersistenceError> {
        QueryPlaneStore::query_field_index(
            self.inner.as_ref(),
            tenant,
            entity_type,
            where_clause,
            params,
        )
        .await
    }

    async fn load_projection_fields_many(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_ids: &[String],
        field_names: &[&str],
    ) -> Result<Option<Vec<QueryProjectionFieldsRow>>, PersistenceError> {
        QueryPlaneStore::load_projection_fields_many(
            self.inner.as_ref(),
            tenant,
            entity_type,
            entity_ids,
            field_names,
        )
        .await
    }

    async fn load_entity_catalog_rows(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_ids: &[String],
    ) -> Result<Option<Vec<EntityCatalogRow>>, PersistenceError> {
        QueryPlaneStore::load_entity_catalog_rows(
            self.inner.as_ref(),
            tenant,
            entity_type,
            entity_ids,
        )
        .await
    }

    async fn projected_entity_counts_by_tenant(
        &self,
    ) -> Result<Option<Vec<(String, u64)>>, PersistenceError> {
        QueryPlaneStore::projected_entity_counts_by_tenant(self.inner.as_ref()).await
    }
}

#[tokio::test]
async fn tombstone_between_replay_and_projection_repair_is_replayed_before_return() {
    let (_guard, _clock, _ids) = install_deterministic_context(261);
    let tenant = TenantId::default();
    let workspace = "ws-projection-repair-race";
    let path = "/live-before-repair";
    let entity_id = "ord-projection-repair-race";
    let persistence_id = format!("{tenant}:Order:{entity_id}");
    let events = SimEventStore::no_faults(261);
    EventStore::append_with_index_rows(
        &events,
        &persistence_id,
        0,
        &[super::source_transitions::complete_field_update(
            &persistence_id,
            entity_id,
            workspace,
            path,
            "projection-race-live",
        )],
        &[key_row(workspace, path)],
        &[],
        IndexReconciliation {
            keys: true,
            key_set_signature: Some(ORDER_KEY_SET_SIGNATURE.to_string()),
            vectors: false,
        },
    )
    .await
    .expect("seed projection-race live state");
    let catalog = Arc::new(SimQueryPlane::default());
    let racing_query_plane = Arc::new(TombstoneOnUpsertQueryPlane {
        inner: catalog.clone(),
        events: events.clone(),
        persistence_id,
        fired: Arc::new(AtomicBool::new(false)),
    });
    let mut state = build_order_state("projection-repair-tombstone-race");
    state.set_storage_stack(StorageStack::new(
        BackendLabel::Sim,
        BoxedEventStore::new(events),
        None,
        None,
        None,
        None,
        Some(racing_query_plane),
        None,
        None,
        None,
    ));

    let result = match super::source_transitions::read_path(&state, &tenant, workspace, path).await
    {
        Ok(result) => result,
        Err(_) => panic!("a tombstone racing repair must stabilize to a successful read"),
    };
    assert!(
        result.entities.is_empty(),
        "a tombstone racing repair must not return the stale live body"
    );
    let rows = QueryPlaneStore::load_entity_catalog_rows(
        catalog.as_ref(),
        tenant.as_str(),
        "Order",
        &[entity_id.to_string()],
    )
    .await
    .expect("load projection after repair race")
    .expect("sim catalog support");
    assert!(
        rows.is_empty(),
        "terminal retry must remove stale projection"
    );
}
