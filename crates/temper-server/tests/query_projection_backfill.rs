use std::sync::Arc;

use temper_runtime::ActorSystem;
use temper_runtime::persistence::EventStore;
use temper_runtime::tenant::TenantId;
use temper_store_turso::TursoEventStore;

use temper_server::event_store::ServerEventStore;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::state::ServerState;
use temper_spec::csdl::parse_csdl;

const CSDL_XML: &str = include_str!("../../../test-fixtures/specs/model.csdl.xml");
const ORDER_IOA: &str = include_str!("../../../test-fixtures/specs/order.ioa.toml");

fn build_state_with_turso(system_name: &str, store: TursoEventStore) -> ServerState {
    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(CSDL_XML).expect("CSDL should parse");
    registry.register_tenant(
        "tenant-a",
        csdl,
        CSDL_XML.to_string(),
        &[("Order", ORDER_IOA)],
    );

    let mut state = ServerState::from_registry(ActorSystem::new(system_name), registry);
    state.event_store = Some(Arc::new(ServerEventStore::Turso(store)));
    state
}

#[tokio::test]
async fn live_transitions_update_and_delete_query_projection() {
    let db_path = std::env::temp_dir().join(format!(
        "temper-query-projection-live-{}.db",
        uuid::Uuid::new_v4()
    ));
    let db_url = format!("file:{}", db_path.display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso db");
    let observer_store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create observer turso db");

    let tenant = TenantId::new("tenant-a");
    let entity_type = "Order";
    let entity_id = "ord-live-projection";

    let state = build_state_with_turso("test-live-query-projection", store.clone());
    state
        .get_or_create_tenant_entity(
            &tenant,
            entity_type,
            entity_id,
            serde_json::json!({"Title": "Projection Lifecycle"}),
        )
        .await
        .expect("create entity");
    let response = state
        .dispatch_tenant_action(
            &tenant,
            entity_type,
            entity_id,
            "AddItem",
            serde_json::json!({"ProductId": "sku-1", "Quantity": 1}),
            &AgentContext::default(),
        )
        .await
        .expect("dispatch AddItem");
    assert!(
        response.success,
        "AddItem should succeed for live projection test"
    );

    let ids = observer_store
        .query_field_index(
            tenant.as_str(),
            entity_type,
            "field_name = ?3 AND field_value = ?4",
            vec!["Title".to_string(), "Projection Lifecycle".to_string()],
        )
        .await
        .expect("query projection");
    assert_eq!(ids, vec![entity_id.to_string()]);

    let counts = observer_store
        .projected_entity_counts_by_tenant()
        .await
        .expect("projected entity counts");
    assert_eq!(counts, vec![("tenant-a".to_string(), 1)]);

    state
        .delete_tenant_entity(&tenant, entity_type, entity_id)
        .await
        .expect("delete entity");

    let ids = observer_store
        .query_field_index(
            tenant.as_str(),
            entity_type,
            "field_name = ?3 AND field_value = ?4",
            vec!["Title".to_string(), "Projection Lifecycle".to_string()],
        )
        .await
        .expect("query projection after delete");
    assert!(
        ids.is_empty(),
        "deleted entities should be removed from the field index"
    );

    let counts = observer_store
        .projected_entity_counts_by_tenant()
        .await
        .expect("projected entity counts after delete");
    assert!(
        counts.is_empty(),
        "deleted entities should leave the catalog"
    );

    let _ = std::fs::remove_file(db_path);
}

#[tokio::test]
async fn startup_backfill_rebuilds_query_projection_without_hydrating_actors() {
    let db_path = std::env::temp_dir().join(format!(
        "temper-query-projection-backfill-{}.db",
        uuid::Uuid::new_v4()
    ));
    let db_url = format!("file:{}", db_path.display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso db");

    let tenant = TenantId::new("tenant-a");
    let entity_type = "Order";
    let entity_id = "ord-backfill-projection";
    let persistence_id = format!("{tenant}:{entity_type}:{entity_id}");

    let state = build_state_with_turso("test-query-projection-seed", store.clone());
    state
        .get_or_create_tenant_entity(
            &tenant,
            entity_type,
            entity_id,
            serde_json::json!({"Title": "Rebuild Me", "Owner": "alice"}),
        )
        .await
        .expect("create entity");
    let response = state
        .dispatch_tenant_action(
            &tenant,
            entity_type,
            entity_id,
            "AddItem",
            serde_json::json!({"ProductId": "sku-2", "Quantity": 1}),
            &AgentContext::default(),
        )
        .await
        .expect("dispatch AddItem");
    assert!(
        response.success,
        "AddItem should succeed for backfill projection test"
    );

    let snapshot = store
        .load_snapshot(&persistence_id)
        .await
        .expect("load snapshot");
    assert!(
        snapshot.is_none(),
        "fixture should exercise the no-snapshot replay path"
    );

    store
        .remove_query_projection(tenant.as_str(), entity_type, entity_id)
        .await
        .expect("clear existing query projection");

    let restarted = build_state_with_turso("test-query-projection-restart", store.clone());
    restarted.populate_index_from_store(&tenant).await;
    assert!(
        restarted.actor_registry.read().unwrap().is_empty(),
        "restart should begin cold before backfill"
    );

    restarted.populate_field_index_from_snapshots(&tenant).await;

    assert!(
        restarted.actor_registry.read().unwrap().is_empty(),
        "backfill should not hydrate actors just to rebuild projections"
    );

    let ids = store
        .query_field_index(
            tenant.as_str(),
            entity_type,
            "field_name = ?3 AND field_value = ?4",
            vec!["Title".to_string(), "Rebuild Me".to_string()],
        )
        .await
        .expect("query rebuilt field index");
    assert_eq!(ids, vec![entity_id.to_string()]);

    let counts = store
        .projected_entity_counts_by_tenant()
        .await
        .expect("projected entity counts after backfill");
    assert_eq!(counts, vec![("tenant-a".to_string(), 1)]);

    let _ = std::fs::remove_file(db_path);
}
