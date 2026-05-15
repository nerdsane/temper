use temper_runtime::ActorSystem;
use temper_runtime::persistence::EventStore;
use temper_runtime::tenant::TenantId;
use temper_store_turso::TursoEventStore;

use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::state::ServerState;
use temper_server::storage::StorageStack;
use temper_spec::csdl::parse_csdl;

const CSDL_XML: &str = include_str!("../../../test-fixtures/specs/model.csdl.xml");
const ORDER_IOA: &str = include_str!("../../../test-fixtures/specs/order.ioa.toml");
const PROJECTION_AWARE_ORDER_IOA: &str = r#"
[automaton]
name = "Order"
states = ["Draft"]
initial = "Draft"

[[state]]
name = "Title"
type = "string"
initial = ""

[[state]]
name = "progress_token"
type = "counter"
initial = "0"
query_indexed = false

[[action]]
name = "Touch"
from = ["Draft"]
to = "Draft"
effect = [{ type = "increment", var = "progress_token" }]
"#;

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

fn build_projection_aware_state_with_turso(
    system_name: &str,
    store: TursoEventStore,
) -> ServerState {
    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(CSDL_XML).expect("CSDL should parse");
    registry.register_tenant(
        "tenant-a",
        csdl,
        CSDL_XML.to_string(),
        &[("Order", PROJECTION_AWARE_ORDER_IOA)],
    );

    let mut state = ServerState::from_registry(ActorSystem::new(system_name), registry);
    state.set_storage_stack(StorageStack::from_turso(store));
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

#[tokio::test]
async fn query_projection_excludes_fields_marked_not_query_indexed() {
    let db_path = std::env::temp_dir().join(format!(
        "temper-query-projection-opt-out-{}.db",
        uuid::Uuid::new_v4()
    ));
    let db_url = format!("file:{}", db_path.display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso db");

    let tenant = TenantId::new("tenant-a");
    let entity_type = "Order";
    let entity_id = "ord-projection-opt-out";

    let state =
        build_projection_aware_state_with_turso("test-query-projection-opt-out", store.clone());
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
            "Touch",
            serde_json::json!({}),
            &AgentContext::default(),
        )
        .await
        .expect("dispatch Touch");
    assert!(
        response.success,
        "Touch should succeed for projection opt-out test"
    );

    let title_ids = wait_for_query_projection_ids(
        &store,
        tenant.as_str(),
        entity_type,
        "Title",
        "Projection Lifecycle",
        &[entity_id.to_string()],
    )
    .await;
    assert_eq!(title_ids, vec![entity_id.to_string()]);

    let progress_ids = store
        .query_field_index(
            tenant.as_str(),
            entity_type,
            "field_name = ?3 AND field_value = ?4",
            vec!["progress_token".to_string(), "1".to_string()],
        )
        .await
        .expect("query opt-out field");
    assert!(
        progress_ids.is_empty(),
        "fields marked query_indexed=false should not appear in the field index"
    );

    let _ = std::fs::remove_file(db_path);
}

#[tokio::test]
async fn replay_parity_verifier_detects_projection_drift() {
    let db_path = std::env::temp_dir().join(format!(
        "temper-query-projection-parity-{}.db",
        uuid::Uuid::new_v4()
    ));
    let db_url = format!("file:{}", db_path.display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso db");

    let tenant = TenantId::new("tenant-a");
    let entity_type = "Order";
    let active_id = "ord-parity-active";
    let deleted_id = "ord-parity-deleted";

    let state = build_state_with_turso("test-query-projection-parity", store.clone());
    state
        .get_or_create_tenant_entity(
            &tenant,
            entity_type,
            active_id,
            serde_json::json!({"Title": "Parity Good"}),
        )
        .await
        .expect("create active entity");
    let response = state
        .dispatch_tenant_action(
            &tenant,
            entity_type,
            active_id,
            "AddItem",
            serde_json::json!({"ProductId": "sku-3", "Quantity": 1}),
            &AgentContext::default(),
        )
        .await
        .expect("dispatch AddItem");
    assert!(
        response.success,
        "AddItem should succeed for parity verifier test"
    );

    state
        .get_or_create_tenant_entity(
            &tenant,
            entity_type,
            deleted_id,
            serde_json::json!({"Title": "Parity Deleted"}),
        )
        .await
        .expect("create entity that will be deleted");
    state
        .delete_tenant_entity(&tenant, entity_type, deleted_id)
        .await
        .expect("delete parity entity");

    let active_ids = wait_for_query_projection_ids(
        &store,
        tenant.as_str(),
        entity_type,
        "Title",
        "Parity Good",
        &[active_id.to_string()],
    )
    .await;
    assert_eq!(active_ids, vec![active_id.to_string()]);
    let counts = wait_for_projected_counts(&store, &[("tenant-a".to_string(), 1)]).await;
    assert_eq!(counts, vec![("tenant-a".to_string(), 1)]);

    let clean = state
        .verify_query_projection_replay_parity(&tenant)
        .await
        .expect("clean replay parity report");
    assert!(clean.is_clean(), "expected clean parity report: {clean:?}");
    assert_eq!(clean.checked, 1);
    assert_eq!(clean.matched, 1);
    assert_eq!(clean.deleted_absent, 0);

    let actor_state = state
        .get_tenant_entity_state(&tenant, entity_type, active_id)
        .await
        .expect("load active state");
    store
        .upsert_query_projection(
            tenant.as_str(),
            entity_type,
            active_id,
            &actor_state.state.status,
            &serde_json::json!({"Title": "Parity Drift"}),
            actor_state.state.sequence_nr,
        )
        .await
        .expect("inject projection drift");

    let drift = state
        .verify_query_projection_replay_parity(&tenant)
        .await
        .expect("drift replay parity report");
    assert!(!drift.is_clean(), "expected drift report: {drift:?}");
    assert_eq!(drift.checked, 1);
    assert_eq!(drift.drifted, 1);
    assert_eq!(drift.missing, 0);
    assert_eq!(drift.errors, 0);
    assert!(
        drift.drift_examples.iter().any(|example| {
            example.entity_type == entity_type
                && example.entity_id == active_id
                && example.drift_kind == "fields"
                && example.sequence_direction == "equal"
        }),
        "expected fields drift example for active entity: {drift:?}"
    );

    let _ = std::fs::remove_file(db_path);
}
