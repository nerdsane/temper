//! ARN-192 (a): a deleted entity must be absent from a listing after a restart on
//! every backend, not just Turso. The non-eager cold path
//! (`populate_index_from_store` → `list_entity_ids_lazy`) is what a restart uses, so
//! these tests rebuild a fresh `ServerState` over the same durable store and assert
//! the tombstoned entity is gone while a live sibling survives.

use temper_runtime::ActorSystem;
use temper_runtime::persistence::{EventMetadata, EventStore, PersistenceEnvelope};
use temper_runtime::scheduler::{sim_now, sim_uuid};
use temper_runtime::tenant::TenantId;

use temper_server::registry::SpecRegistry;
use temper_server::state::ServerState;
use temper_server::storage::StorageStack;
use temper_spec::csdl::parse_csdl;

const CSDL_XML: &str = include_str!("../../../test-fixtures/specs/model.csdl.xml");
const ORDER_IOA: &str = include_str!("../../../test-fixtures/specs/order.ioa.toml");

fn register(system_name: &str, tenant: &str) -> ServerState {
    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(CSDL_XML).expect("CSDL should parse");
    registry.register_tenant(tenant, csdl, CSDL_XML.to_string(), &[("Order", ORDER_IOA)]);
    ServerState::from_registry(ActorSystem::new(system_name), registry)
}

/// Sim backend: create one entity, delete it, restart into a fresh index, and prove
/// the deleted entity is absent from `list_entity_ids_lazy` while the live one is
/// present. Deterministic, always runs.
#[tokio::test]
async fn deleted_entity_absent_from_list_after_restart_sim() {
    let store = temper_store_sim::SimEventStore::no_faults(192);

    let tenant = TenantId::new("tenant-a");
    let entity_type = "Order";

    let mut state = register("arn192-sim-writer", "tenant-a");
    state.set_storage_stack(StorageStack::from_sim(store.clone(), None));

    for id in ["ord-live", "ord-doomed"] {
        state
            .get_or_create_tenant_entity(&tenant, entity_type, id, serde_json::json!({"Title": id}))
            .await
            .expect("create entity");
    }
    state
        .delete_tenant_entity(&tenant, entity_type, "ord-doomed")
        .await
        .expect("delete entity");

    // Fresh process: empty in-memory index over the same durable Sim store.
    let mut restarted = register("arn192-sim-reader", "tenant-a");
    restarted.set_storage_stack(StorageStack::from_sim(store, None));
    restarted.populate_index_from_store(&tenant).await.unwrap();

    let ids = restarted
        .list_entity_ids_lazy(&tenant, entity_type)
        .await
        .unwrap();
    assert!(
        ids.iter().any(|id| id == "ord-live"),
        "live entity must survive the restart, got {ids:?}"
    );
    assert!(
        !ids.iter().any(|id| id == "ord-doomed"),
        "deleted entity must be absent after restart on the Sim backend, got {ids:?}"
    );
}

/// A tail-read failure must not index the uncertain candidate or mark the type
/// hydrated. The next list retries classification and succeeds once the injected
/// failure is consumed.
#[tokio::test]
async fn latest_event_failure_keeps_index_unhydrated_and_retriable() {
    let store = temper_store_sim::SimEventStore::no_faults(193);
    let tenant = TenantId::new("tenant-failure");

    let mut writer = register("arn192-failure-writer", "tenant-failure");
    writer.set_storage_stack(StorageStack::from_sim(store.clone(), None));
    writer
        .get_or_create_tenant_entity(
            &tenant,
            "Order",
            "ord-uncertain",
            serde_json::json!({"Title": "uncertain"}),
        )
        .await
        .expect("create uncertain entity");

    store.fail_next_reads("tenant-failure:Order:ord-uncertain", 1);
    let mut restarted = register("arn192-failure-reader", "tenant-failure");
    restarted.set_storage_stack(StorageStack::from_sim(store, None));

    assert!(restarted.populate_index_from_store(&tenant).await.is_err());
    assert!(!restarted.entity_exists(&tenant, "Order", "ord-uncertain"));

    let retried = restarted
        .list_entity_ids_lazy(&tenant, "Order")
        .await
        .unwrap();
    assert_eq!(retried, vec!["ord-uncertain".to_string()]);
}

#[tokio::test]
async fn eager_hydration_failure_does_not_publish_partial_watermarks() {
    let store = temper_store_sim::SimEventStore::no_faults(194);
    let tenant = TenantId::new("tenant-eager-failure");

    let mut writer = register("arn192-eager-failure-writer", tenant.as_str());
    writer.set_storage_stack(StorageStack::from_sim(store.clone(), None));
    writer
        .get_or_create_tenant_entity(
            &tenant,
            "Order",
            "ord-loaded-first",
            serde_json::json!({"Title": "loaded before failure"}),
        )
        .await
        .expect("create durable order");
    store
        .append(
            "tenant-eager-failure:Zed:unregistered",
            0,
            &[PersistenceEnvelope {
                sequence_nr: 0,
                event_type: "Created".to_string(),
                payload: serde_json::json!({}),
                metadata: EventMetadata {
                    event_id: sim_uuid(),
                    causation_id: sim_uuid(),
                    correlation_id: sim_uuid(),
                    timestamp: sim_now(),
                    actor_id: "eager-hydration-test".to_string(),
                },
            }],
        )
        .await
        .expect("append unregistered stream that cannot spawn");

    let mut restarted = register("arn192-eager-failure-reader", tenant.as_str());
    restarted.set_storage_stack(StorageStack::from_sim(store, None));
    assert!(restarted.hydrate_from_store(&tenant).await.is_err());
    assert!(
        restarted.entity_index_hydrated.read().unwrap().is_empty(),
        "a failure after loading one actor must leave every affected type retriable"
    );
    assert!(
        restarted.entity_exists(&tenant, "Order", "ord-loaded-first"),
        "precondition: eager hydration reached the registered entity before failing"
    );
}

/// Redis backend: same property against a live service.
#[tokio::test]
#[ignore = "requires REDIS_URL and a live Redis service"]
async fn deleted_entity_absent_from_list_after_restart_redis() {
    let url = std::env::var("REDIS_URL").expect("REDIS_URL for ignored Redis integration test");

    let entity_type = "Order";
    // Unique tenant so repeated runs against a shared Redis never collide.
    let tenant_name = format!("arn192-{}", uuid::Uuid::new_v4());
    let tenant = TenantId::new(&tenant_name);

    let store = temper_store_redis::RedisEventStore::new(&url)
        .await
        .expect("connect to Redis");

    let mut state = register("arn192-redis-writer", &tenant_name);
    state.set_storage_stack(StorageStack::from_redis(store.clone()));

    for id in ["ord-live", "ord-doomed"] {
        state
            .get_or_create_tenant_entity(&tenant, entity_type, id, serde_json::json!({"Title": id}))
            .await
            .expect("create entity");
    }
    state
        .delete_tenant_entity(&tenant, entity_type, "ord-doomed")
        .await
        .expect("delete entity");

    let mut restarted = register("arn192-redis-reader", &tenant_name);
    restarted.set_storage_stack(StorageStack::from_redis(store));
    restarted.populate_index_from_store(&tenant).await.unwrap();

    let ids = restarted
        .list_entity_ids_lazy(&tenant, entity_type)
        .await
        .unwrap();
    assert!(
        ids.iter().any(|id| id == "ord-live"),
        "live entity must survive the restart, got {ids:?}"
    );
    assert!(
        !ids.iter().any(|id| id == "ord-doomed"),
        "deleted entity must be absent after restart on the Redis backend, got {ids:?}"
    );
}
