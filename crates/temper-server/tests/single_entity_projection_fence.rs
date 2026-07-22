//! Single-entity OData reads must close against durable entity sources.

mod common;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use temper_runtime::ActorSystem;
use temper_runtime::persistence::{EventStore, PersistenceError};
use temper_runtime::tenant::TenantId;
use temper_server::registry::SpecRegistry;
use temper_server::storage::{
    BackendLabel, BoxedEventStore, EntityCatalogRow, QueryPlaneStore, QueryProjectionFieldsRow,
};
use temper_server::{ServerState, StorageStack, build_router};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;
use tower::ServiceExt;

type SnapshotWrite = (SimEventStore, String, u64, Vec<u8>);

struct StaleCatalog {
    row: EntityCatalogRow,
    snapshot_on_load: Mutex<Option<SnapshotWrite>>,
}

#[async_trait]
impl QueryPlaneStore for StaleCatalog {
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
        entity_type: &str,
        entity_ids: &[String],
    ) -> Result<Option<Vec<EntityCatalogRow>>, PersistenceError> {
        let snapshot_write = self
            .snapshot_on_load
            .lock()
            .expect("snapshot injection lock poisoned")
            .take();
        if let Some((store, persistence_id, sequence_nr, snapshot)) = snapshot_write {
            EventStore::save_snapshot(&store, &persistence_id, sequence_nr, &snapshot).await?;
        }
        if entity_type == "Order" && entity_ids.contains(&self.row.entity_id) {
            return Ok(Some(vec![self.row.clone()]));
        }
        Ok(Some(Vec::new()))
    }

    async fn projected_entity_counts_by_tenant(
        &self,
    ) -> Result<Option<Vec<(String, u64)>>, PersistenceError> {
        Ok(None)
    }
}

fn build_stale_catalog_state(
    tenant: &TenantId,
    store: SimEventStore,
    stale_catalog: Arc<StaleCatalog>,
    system_name: &str,
) -> ServerState {
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        tenant.as_str(),
        parse_csdl(common::CSDL_XML).expect("parse CSDL"),
        common::CSDL_XML.to_string(),
        &[("Order", common::ORDER_IOA)],
    );
    let mut state = ServerState::from_registry(ActorSystem::new(system_name), registry);
    state.set_storage_stack(StorageStack::new(
        BackendLabel::Sim,
        BoxedEventStore::new(store),
        None,
        None,
        None,
        None,
        Some(stale_catalog as Arc<dyn QueryPlaneStore>),
        None,
        None,
        None,
    ));
    state
}

#[tokio::test]
async fn direct_key_get_does_not_resurrect_durable_tombstone_from_stale_catalog() {
    let tenant = TenantId::default();
    let entity_id = "ord-direct-tombstone";
    let persistence_id = format!("{tenant}:Order:{entity_id}");
    let store = SimEventStore::no_faults(9_238);
    let tombstone = serde_json::json!({
        "entity_type": "Order",
        "entity_id": entity_id,
        "status": "Deleted",
        "item_count": 0,
        "fields": {
            "Id": entity_id,
            "Status": "Deleted",
            "WorkspaceId": null,
            "Path": null,
        },
        "sequence_nr": 5,
    });
    EventStore::save_snapshot(
        &store,
        &persistence_id,
        5,
        &serde_json::to_vec(&tombstone).expect("serialize durable tombstone"),
    )
    .await
    .expect("persist durable tombstone");

    let stale_fields = serde_json::json!({
        "Id": entity_id,
        "Status": "Created",
        "WorkspaceId": "ws-stale",
        "Path": "/stale",
    });
    let stale_catalog = Arc::new(StaleCatalog {
        row: EntityCatalogRow {
            entity_id: entity_id.to_string(),
            status: "Created".to_string(),
            fields: stale_fields.clone(),
            state: Some(serde_json::json!({
                "entity_type": "Order",
                "entity_id": entity_id,
                "status": "Created",
                "fields": stale_fields,
                "sequence_nr": 4,
            })),
            sequence_nr: 4,
        },
        snapshot_on_load: Mutex::new(None),
    });
    let state =
        build_stale_catalog_state(&tenant, store, stale_catalog, "direct-key-projection-fence");

    let response = build_router(state)
        .oneshot(
            Request::get(format!("/tdata/Orders('{entity_id}')"))
                .body(Body::empty())
                .expect("build request"),
        )
        .await
        .expect("read response");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn direct_key_get_rechecks_durable_absence_after_catalog_materialization() {
    let tenant = TenantId::default();
    let entity_id = "ord-direct-source-transition";
    let persistence_id = format!("{tenant}:Order:{entity_id}");
    let store = SimEventStore::no_faults(9_239);
    let tombstone = serde_json::to_vec(&serde_json::json!({
        "entity_type": "Order",
        "entity_id": entity_id,
        "status": "Deleted",
        "item_count": 0,
        "fields": {
            "Id": entity_id,
            "Status": "Deleted",
            "WorkspaceId": null,
            "Path": null,
        },
        "sequence_nr": 5,
    }))
    .expect("serialize injected tombstone");
    let stale_fields = serde_json::json!({
        "Id": entity_id,
        "Status": "Created",
        "WorkspaceId": "ws-stale",
        "Path": "/stale",
    });
    let stale_catalog = Arc::new(StaleCatalog {
        row: EntityCatalogRow {
            entity_id: entity_id.to_string(),
            status: "Created".to_string(),
            fields: stale_fields.clone(),
            state: Some(serde_json::json!({
                "entity_type": "Order",
                "entity_id": entity_id,
                "status": "Created",
                "fields": stale_fields,
                "sequence_nr": 4,
            })),
            sequence_nr: 4,
        },
        snapshot_on_load: Mutex::new(Some((store.clone(), persistence_id, 5, tombstone))),
    });
    let state = build_stale_catalog_state(
        &tenant,
        store,
        stale_catalog,
        "direct-key-source-transition",
    );

    let response = build_router(state)
        .oneshot(
            Request::get(format!("/tdata/Orders('{entity_id}')"))
                .body(Body::empty())
                .expect("build request"),
        )
        .await
        .expect("read response");

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}
