use std::sync::Arc;

use temper_runtime::ActorSystem;
use temper_runtime::persistence::EventStore;
use temper_runtime::tenant::TenantId;
use temper_store_sim::SimEventStore;
use temper_store_turso::TursoEventStore;

use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::state::ServerState;
use temper_server::storage::{BackendLabel, BoxedEventStore, QueryPlaneStore, StorageStack};
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
    state.set_storage_stack(StorageStack::from_turso(store));
    state
}

fn build_state_with_sim_events_and_turso_projection(
    system_name: &str,
    events: SimEventStore,
    projection: TursoEventStore,
) -> ServerState {
    let mut state = build_state_with_turso(system_name, projection.clone());
    state.set_storage_stack(StorageStack::new(
        BackendLabel::Sim,
        BoxedEventStore::new(events),
        None,
        None,
        None,
        None,
        Some(Arc::new(projection) as Arc<dyn QueryPlaneStore>),
        None,
        None,
        None,
    ));
    state
}

async fn wait_for_query_projection_ids(
    store: &TursoEventStore,
    tenant: &str,
    entity_type: &str,
    field_name: &str,
    field_value: &str,
    expected: &[String],
) -> Vec<String> {
    let mut last = Vec::new();
    for _ in 0..50 {
        last = store
            .query_field_index(
                tenant,
                entity_type,
                "field_name = ?3 AND field_value = ?4",
                vec![field_name.to_string(), field_value.to_string()],
            )
            .await
            .expect("query projection");
        if last == expected {
            return last;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    last
}

async fn wait_for_projected_counts(
    store: &TursoEventStore,
    expected: &[(String, u64)],
) -> Vec<(String, u64)> {
    let mut last = Vec::new();
    for _ in 0..50 {
        last = store
            .projected_entity_counts_by_tenant()
            .await
            .expect("projected entity counts");
        if last == expected {
            return last;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    last
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

    let ids = wait_for_query_projection_ids(
        &observer_store,
        tenant.as_str(),
        entity_type,
        "Title",
        "Projection Lifecycle",
        &[entity_id.to_string()],
    )
    .await;
    assert_eq!(ids, vec![entity_id.to_string()]);

    let counts = wait_for_projected_counts(&observer_store, &[("tenant-a".to_string(), 1)]).await;
    assert_eq!(counts, vec![("tenant-a".to_string(), 1)]);

    state
        .delete_tenant_entity(&tenant, entity_type, entity_id)
        .await
        .expect("delete entity");

    let ids = wait_for_query_projection_ids(
        &observer_store,
        tenant.as_str(),
        entity_type,
        "Title",
        "Projection Lifecycle",
        &[],
    )
    .await;
    assert!(
        ids.is_empty(),
        "deleted entities should be removed from the field index"
    );

    let counts = wait_for_projected_counts(&observer_store, &[]).await;
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
    restarted.populate_index_from_store(&tenant).await.unwrap();
    assert!(
        restarted.actor_registry.read().unwrap().is_empty(),
        "restart should begin cold before backfill"
    );

    restarted
        .populate_field_index_from_snapshots(&tenant)
        .await
        .expect("backfill query projections");

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

#[tokio::test]
async fn startup_backfill_replays_tail_after_snapshot_before_publishing() {
    let db_path = std::env::temp_dir().join(format!(
        "temper-query-projection-stale-snapshot-{}.db",
        uuid::Uuid::new_v4()
    ));
    let db_url = format!("file:{}", db_path.display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso db");

    let tenant = TenantId::new("tenant-a");
    let entity_type = "Order";
    let entity_id = "ord-stale-snapshot";
    let persistence_id = format!("{tenant}:{entity_type}:{entity_id}");

    let state = build_state_with_turso("test-query-projection-stale-seed", store.clone());
    state
        .get_or_create_tenant_entity(
            &tenant,
            entity_type,
            entity_id,
            serde_json::json!({"Title": "Must Stay Deleted"}),
        )
        .await
        .expect("create entity");
    let live = state
        .get_tenant_entity_state(&tenant, entity_type, entity_id)
        .await
        .expect("load live state")
        .state;
    store
        .save_snapshot(
            &persistence_id,
            live.sequence_nr,
            &serde_json::to_vec(&live).expect("serialize live snapshot"),
        )
        .await
        .expect("save deliberately stale snapshot");

    state
        .delete_tenant_entity(&tenant, entity_type, entity_id)
        .await
        .expect("append deletion after snapshot");

    store
        .upsert_query_projection_with_state(
            tenant.as_str(),
            entity_type,
            entity_id,
            &live.status,
            &live.fields,
            &serde_json::to_value(&live).expect("serialize stale projection state"),
            live.sequence_nr,
        )
        .await
        .expect("inject stale live projection");

    let restarted = build_state_with_turso("test-query-projection-stale-restart", store.clone());
    restarted
        .populate_field_index_from_snapshots(&tenant)
        .await
        .expect("backfill query projections");

    let ids = store
        .query_field_index(
            tenant.as_str(),
            entity_type,
            "field_name = ?3 AND field_value = ?4",
            vec!["Title".to_string(), "Must Stay Deleted".to_string()],
        )
        .await
        .expect("query projection after strict backfill");
    assert!(
        ids.is_empty(),
        "the deletion tail must win over the stale live snapshot"
    );
    assert!(
        store
            .projected_entity_counts_by_tenant()
            .await
            .expect("projected entity counts")
            .is_empty(),
        "a tombstoned entity must not remain in the projection catalog"
    );

    let _ = std::fs::remove_file(db_path);
}

#[tokio::test]
async fn startup_backfill_quarantines_stale_projection_when_recovery_fails() {
    let db_path = std::env::temp_dir().join(format!(
        "temper-query-projection-recovery-error-{}.db",
        uuid::Uuid::new_v4()
    ));
    let db_url = format!("file:{}", db_path.display());
    let projection = TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso projection db");
    let events = SimEventStore::no_faults(195);
    let tenant = TenantId::new("tenant-a");
    let entity_type = "Order";
    let entity_id = "ord-recovery-error";
    let persistence_id = format!("{tenant}:{entity_type}:{entity_id}");

    let writer = build_state_with_sim_events_and_turso_projection(
        "test-query-projection-recovery-error-writer",
        events.clone(),
        projection.clone(),
    );
    writer
        .get_or_create_tenant_entity(
            &tenant,
            entity_type,
            entity_id,
            serde_json::json!({"Title": "Uncertain Projection"}),
        )
        .await
        .expect("create durable entity");
    let live = writer
        .get_tenant_entity_state(&tenant, entity_type, entity_id)
        .await
        .expect("load live state")
        .state;
    projection
        .upsert_query_projection_with_state(
            tenant.as_str(),
            entity_type,
            entity_id,
            &live.status,
            &live.fields,
            &serde_json::to_value(&live).expect("serialize stale projection"),
            live.sequence_nr,
        )
        .await
        .expect("seed stale projection");

    events.fail_next_reads(&persistence_id, 1);
    let restarted = build_state_with_sim_events_and_turso_projection(
        "test-query-projection-recovery-error-reader",
        events,
        projection.clone(),
    );
    restarted
        .populate_field_index_from_snapshots(&tenant)
        .await
        .expect_err("unreadable journal must report incomplete backfill");

    assert!(
        projection
            .query_field_index(
                tenant.as_str(),
                entity_type,
                "field_name = ?3 AND field_value = ?4",
                vec!["Title".to_string(), "Uncertain Projection".to_string(),],
            )
            .await
            .expect("query projection after failed recovery")
            .is_empty(),
        "an unreadable journal must quarantine a pre-existing projection"
    );

    let _ = std::fs::remove_file(db_path);
}
