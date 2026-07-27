use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::ServerState;
use temper_server::StorageStack;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_spec::csdl::parse_csdl;
use temper_store_turso::TursoEventStore;

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

fn build_state(system_name: &str, store: TursoEventStore, ioa: &str) -> ServerState {
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "tenant-a",
        parse_csdl(CSDL_XML).expect("CSDL should parse"),
        CSDL_XML.to_string(),
        &[("Order", ioa)],
    );
    let mut state = ServerState::from_registry(ActorSystem::new(system_name), registry);
    state.set_storage_stack(StorageStack::from_turso(store));
    state
}

async fn wait_for_ids(
    store: &TursoEventStore,
    field_name: &str,
    field_value: &str,
    expected: &[String],
) -> Vec<String> {
    let mut last = Vec::new();
    for _ in 0..50 {
        last = store
            .query_field_index(
                "tenant-a",
                "Order",
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

async fn wait_for_counts(
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
async fn query_projection_excludes_fields_marked_not_query_indexed() {
    let db_path = std::env::temp_dir().join(format!(
        "temper-query-projection-opt-out-{}.db",
        uuid::Uuid::new_v4()
    ));
    let store = TursoEventStore::new(&format!("file:{}", db_path.display()), None)
        .await
        .expect("create local turso db");
    let tenant = TenantId::new("tenant-a");
    let entity_id = "ord-projection-opt-out";
    let state = build_state(
        "test-query-projection-opt-out",
        store.clone(),
        PROJECTION_AWARE_ORDER_IOA,
    );
    state
        .get_or_create_tenant_entity(
            &tenant,
            "Order",
            entity_id,
            serde_json::json!({"Title": "Projection Lifecycle"}),
        )
        .await
        .expect("create entity");
    let response = state
        .dispatch_tenant_action(
            &tenant,
            "Order",
            entity_id,
            "Touch",
            serde_json::json!({}),
            &AgentContext::default(),
        )
        .await
        .expect("dispatch Touch");
    assert!(response.success);

    assert_eq!(
        wait_for_ids(
            &store,
            "Title",
            "Projection Lifecycle",
            &[entity_id.to_string()]
        )
        .await,
        vec![entity_id.to_string()]
    );
    assert!(
        store
            .query_field_index(
                tenant.as_str(),
                "Order",
                "field_name = ?3 AND field_value = ?4",
                vec!["progress_token".to_string(), "1".to_string()],
            )
            .await
            .expect("query opt-out field")
            .is_empty()
    );
    let _ = std::fs::remove_file(db_path);
}

#[tokio::test]
async fn replay_parity_verifier_detects_projection_drift() {
    let db_path = std::env::temp_dir().join(format!(
        "temper-query-projection-parity-{}.db",
        uuid::Uuid::new_v4()
    ));
    let store = TursoEventStore::new(&format!("file:{}", db_path.display()), None)
        .await
        .expect("create local turso db");
    let tenant = TenantId::new("tenant-a");
    let active_id = "ord-parity-active";
    let deleted_id = "ord-parity-deleted";
    let state = build_state("test-query-projection-parity", store.clone(), ORDER_IOA);
    state
        .get_or_create_tenant_entity(
            &tenant,
            "Order",
            active_id,
            serde_json::json!({"Title": "Parity Good"}),
        )
        .await
        .expect("create active entity");
    assert!(
        state
            .dispatch_tenant_action(
                &tenant,
                "Order",
                active_id,
                "AddItem",
                serde_json::json!({"ProductId": "sku-3", "Quantity": 1}),
                &AgentContext::default(),
            )
            .await
            .expect("dispatch AddItem")
            .success
    );
    state
        .get_or_create_tenant_entity(
            &tenant,
            "Order",
            deleted_id,
            serde_json::json!({"Title": "Parity Deleted"}),
        )
        .await
        .expect("create entity that will be deleted");
    state
        .delete_tenant_entity(&tenant, "Order", deleted_id)
        .await
        .expect("delete parity entity");

    assert_eq!(
        wait_for_ids(&store, "Title", "Parity Good", &[active_id.to_string()]).await,
        vec![active_id.to_string()]
    );
    assert_eq!(
        wait_for_counts(&store, &[("tenant-a".to_string(), 1)]).await,
        vec![("tenant-a".to_string(), 1)]
    );

    let clean = state
        .verify_query_projection_replay_parity(&tenant)
        .await
        .expect("clean replay parity report");
    assert!(clean.is_clean(), "expected clean parity report: {clean:?}");
    assert_eq!(clean.checked, 2);
    assert_eq!(clean.matched, 1);
    assert_eq!(clean.deleted_absent, 1);

    let actor_state = state
        .get_tenant_entity_state(&tenant, "Order", active_id)
        .await
        .expect("load active state");
    let mut catalog_state = serde_json::to_value(&actor_state.state).expect("serialize state");
    if let Some(obj) = catalog_state.as_object_mut() {
        obj.insert("events".to_string(), serde_json::json!([]));
    }
    store
        .upsert_query_projection_with_state(
            tenant.as_str(),
            "Order",
            active_id,
            &actor_state.state.status,
            &serde_json::json!({"Title": "Parity Drift"}),
            &catalog_state,
            actor_state.state.sequence_nr,
        )
        .await
        .expect("inject projection drift");

    let drift = state
        .verify_query_projection_replay_parity(&tenant)
        .await
        .expect("drift replay parity report");
    assert!(!drift.is_clean(), "expected drift report: {drift:?}");
    assert_eq!(drift.checked, 2);
    assert_eq!(drift.drifted, 1);
    assert_eq!(drift.deleted_absent, 1);
    assert_eq!(drift.missing, 0);
    assert_eq!(drift.errors, 0);
    assert!(drift.drift_examples.iter().any(|example| {
        example.entity_type == "Order"
            && example.entity_id == active_id
            && example.drift_kind == "fields"
            && example.sequence_direction == "equal"
    }));
    let _ = std::fs::remove_file(db_path);
}
