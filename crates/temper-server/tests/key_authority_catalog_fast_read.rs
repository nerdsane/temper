//! Regression for the catalog fast-read flag crossing an authoritative key boundary.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use temper_runtime::ActorSystem;
use temper_runtime::persistence::{
    EventMetadata, EventStore, PersistenceEnvelope, PersistenceError,
};
use temper_runtime::scheduler::{install_deterministic_context, sim_now, sim_uuid};
use temper_runtime::tenant::TenantId;
use temper_server::entity_actor::EntityEvent;
use temper_server::registry::SpecRegistry;
use temper_server::storage::{
    BackendLabel, BoxedEventStore, EntityCatalogRow, QueryPlaneStore, QueryProjectionFieldsRow,
    StorageStack,
};
use temper_server::{ServerState, build_router};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;
use tower::ServiceExt;

const CSDL_XML: &str = include_str!("../../../test-fixtures/specs/model.csdl.xml");
const ORDER_IOA: &str = include_str!("../../../test-fixtures/specs/order.ioa.toml");

#[derive(Default)]
struct StaleCatalog {
    rows: Mutex<BTreeMap<String, EntityCatalogRow>>,
}

#[async_trait]
impl QueryPlaneStore for StaleCatalog {
    async fn upsert_projection(
        &self,
        _tenant: &str,
        _entity_type: &str,
        entity_id: &str,
        status: &str,
        fields: &serde_json::Value,
        state: &serde_json::Value,
        sequence_nr: u64,
    ) -> Result<(), PersistenceError> {
        self.rows.lock().expect("catalog lock").insert(
            entity_id.to_string(),
            EntityCatalogRow {
                entity_id: entity_id.to_string(),
                status: status.to_string(),
                fields: fields.clone(),
                state: Some(state.clone()),
                sequence_nr,
            },
        );
        Ok(())
    }

    async fn remove_projection(
        &self,
        _tenant: &str,
        _entity_type: &str,
        entity_id: &str,
    ) -> Result<(), PersistenceError> {
        self.rows.lock().expect("catalog lock").remove(entity_id);
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
        let rows = self.rows.lock().expect("catalog lock");
        Ok(Some(
            entity_ids
                .iter()
                .filter_map(|id| rows.get(id).cloned())
                .collect(),
        ))
    }

    async fn projected_entity_counts_by_tenant(
        &self,
    ) -> Result<Option<Vec<(String, u64)>>, PersistenceError> {
        Ok(None)
    }
}

fn state_with_store(name: &str, events: &SimEventStore, catalog: Arc<StaleCatalog>) -> ServerState {
    let csdl = parse_csdl(CSDL_XML).expect("CSDL parse");
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        csdl,
        CSDL_XML.to_string(),
        &[("Order", ORDER_IOA)],
    );
    let mut state = ServerState::from_registry(ActorSystem::new(name), registry);
    state.set_storage_stack(StorageStack::new(
        BackendLabel::Sim,
        BoxedEventStore::new(events.clone()),
        None,
        None,
        None,
        None,
        Some(catalog),
        None,
        None,
        None,
    ));
    state
}

/// This integration-test binary contains one test, so setting the process-local
/// flag cannot leak into another test. An authoritative keyed candidate must still
/// materialize from its journal/actor when the catalog is stale.
#[tokio::test]
async fn catalog_fast_read_cannot_override_authoritative_key_materialization() {
    unsafe {
        std::env::set_var("TEMPER_ODATA_CATALOG_FAST_READ", "true");
    }
    let (_guard, _clock, _ids) = install_deterministic_context(240);
    let events = SimEventStore::no_faults(240);
    let catalog = Arc::new(StaleCatalog::default());
    let tenant = TenantId::default();
    let entity_id = "legacy-deleted";
    let state = state_with_store("arn238-fast-read-seed", &events, catalog.clone());

    state
        .get_or_create_tenant_entity(
            &tenant,
            "Order",
            entity_id,
            serde_json::json!({"WorkspaceId": "ws", "Path": "/stale"}),
        )
        .await
        .expect("create keyed entity");
    state.populate_key_index_from_snapshots(&tenant).await;

    let persistence_id = format!("{tenant}:Order:{entity_id}");
    let sequence_nr = events
        .read_events(&persistence_id, 0)
        .await
        .expect("read history")
        .last()
        .expect("created event")
        .sequence_nr;
    let timestamp = sim_now();
    let tombstone = EntityEvent {
        action: "Deleted".to_string(),
        from_status: "Draft".to_string(),
        to_status: "Deleted".to_string(),
        timestamp,
        params: serde_json::json!({}),
        idempotency_key: None,
    };
    events
        .append(
            &persistence_id,
            sequence_nr,
            &[PersistenceEnvelope {
                sequence_nr: sequence_nr + 1,
                event_type: "Deleted".to_string(),
                payload: serde_json::to_value(tombstone).expect("serialize tombstone"),
                metadata: EventMetadata {
                    event_id: sim_uuid(),
                    causation_id: sim_uuid(),
                    correlation_id: sim_uuid(),
                    timestamp,
                    actor_id: persistence_id.clone(),
                },
            }],
        )
        .await
        .expect("append legacy event-only tombstone");
    drop(state);

    let restarted = state_with_store("arn238-fast-read-restart", &events, catalog);
    let response = build_router(restarted)
        .oneshot(
            Request::builder()
                .uri("/tdata/Orders?$filter=WorkspaceId%20eq%20%27ws%27%20and%20Path%20eq%20%27%2Fstale%27")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1_000_000)
        .await
        .expect("response body");
    let json: serde_json::Value = serde_json::from_slice(&body).expect("OData JSON");
    assert_eq!(
        json["value"],
        serde_json::json!([]),
        "the durable tombstone must win even when catalog fast-read is enabled"
    );
}
