mod common;

use common::reaction_fixture::*;
use temper_runtime::persistence::{EventMetadata, PersistenceEnvelope};
use temper_runtime::scheduler::{sim_now, sim_uuid};
use temper_server::trigger::delivery::{
    ReactionDeliveryStatus, extract_intents, find_delivery_record,
};

const REACTIONS: &str = r#"
[[reaction]]
name = "order_confirmed_authorizes_payment"
[reaction.when]
entity_type = "Order"
action = "ConfirmOrder"
to_state = "Confirmed"
[reaction.then]
entity_type = "Payment"
action = "AuthorizePayment"
[reaction.resolve_target]
type = "same_id"
"#;

async fn prove_durable_reaction_contract(
    tenant_name: &str,
    mut state: ServerState,
    stack: StorageStack,
    store: BoxedEventStore,
) {
    state.set_storage_stack(stack);
    let tenant = TenantId::new(tenant_name);
    dispatch(
        &state,
        &tenant,
        "Order",
        "o1",
        "AddItem",
        serde_json::json!({}),
    )
    .await;
    dispatch(
        &state,
        &tenant,
        "Order",
        "o1",
        "SubmitOrder",
        serde_json::json!({}),
    )
    .await;
    dispatch(
        &state,
        &tenant,
        "Order",
        "o1",
        "ConfirmOrder",
        serde_json::json!({}),
    )
    .await;

    let source = store
        .read_events(&format!("{tenant_name}:Order:o1"), 0)
        .await
        .expect("read source journal");
    let intent = source
        .iter()
        .find(|event| event.event_type == "ConfirmOrder")
        .and_then(|event| extract_intents(&event.payload).ok())
        .and_then(|mut intents| intents.pop())
        .expect("source event and normalized intent must be co-committed");
    let (record, _) = find_delivery_record(&store, tenant_name, &intent.delivery_id)
        .await
        .expect("read delivery journal")
        .expect("delivery journal must exist");
    assert_eq!(record.status, ReactionDeliveryStatus::Succeeded);

    for (entity_type, entity_id) in [
        ("Alpha", "a"),
        ("_ReactionDelivery", "d1"),
        ("_ReactionDelivery", "d2"),
        ("Zeta", "z"),
    ] {
        let persistence_id = format!("{tenant_name}:{entity_type}:{entity_id}");
        store
            .append(
                &persistence_id,
                0,
                &[PersistenceEnvelope {
                    sequence_nr: 1,
                    event_type: "Seed".to_string(),
                    payload: serde_json::json!({}),
                    metadata: EventMetadata {
                        event_id: sim_uuid(),
                        causation_id: sim_uuid(),
                        correlation_id: sim_uuid(),
                        timestamp: sim_now(),
                        actor_id: persistence_id.clone(),
                    },
                }],
            )
            .await
            .expect("seed paging journal");
    }
    assert_eq!(
        store
            .list_journal_ids_page(
                tenant_name,
                Some("_ReactionDelivery"),
                Some(("Alpha", "zzz")),
                2,
            )
            .await
            .expect("page after earlier type"),
        vec![
            ("_ReactionDelivery".to_string(), "d1".to_string()),
            ("_ReactionDelivery".to_string(), "d2".to_string()),
        ]
    );
    assert_eq!(
        store
            .list_journal_ids_page(
                tenant_name,
                Some("_ReactionDelivery"),
                Some(("_ReactionDelivery", "d1")),
                1,
            )
            .await
            .expect("page within scoped type"),
        vec![("_ReactionDelivery".to_string(), "d2".to_string())]
    );
    assert!(
        store
            .list_journal_ids_page(
                tenant_name,
                Some("_ReactionDelivery"),
                Some(("zzzz", "")),
                10,
            )
            .await
            .expect("page after later type")
            .is_empty()
    );
}

#[tokio::test]
async fn turso_matches_durable_reaction_contract() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_url = format!("file:{}", dir.path().join("reactions.db").display());
    let store = temper_store_turso::TursoEventStore::new(&db_url, None)
        .await
        .expect("create Turso store");
    prove_durable_reaction_contract(
        "reaction-turso-parity",
        build_state("reaction-turso-parity", REACTIONS),
        StorageStack::from_turso(store.clone()),
        BoxedEventStore::new(store),
    )
    .await;
}

#[tokio::test]
async fn turso_journal_paging_retains_deleted_sources() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_url = format!("file:{}", dir.path().join("deleted.db").display());
    let store = temper_store_turso::TursoEventStore::new(&db_url, None)
        .await
        .expect("create Turso store");
    let boxed = BoxedEventStore::new(store);
    let persistence_id = "reaction-deleted:Order:deleted-source";
    boxed
        .append(
            persistence_id,
            0,
            &[PersistenceEnvelope {
                sequence_nr: 1,
                event_type: "Deleted".to_string(),
                payload: serde_json::json!({}),
                metadata: EventMetadata {
                    event_id: sim_uuid(),
                    causation_id: sim_uuid(),
                    correlation_id: sim_uuid(),
                    timestamp: sim_now(),
                    actor_id: persistence_id.to_string(),
                },
            }],
        )
        .await
        .expect("persist deleted source");
    assert_eq!(
        boxed
            .list_journal_ids_page("reaction-deleted", None, None, 1)
            .await
            .expect("page durable journals"),
        vec![("Order".to_string(), "deleted-source".to_string())]
    );
}

#[tokio::test]
async fn postgres_matches_durable_reaction_contract_when_available() {
    let Ok(database_url) = std::env::var("DATABASE_URL") else {
        assert_ne!(
            std::env::var("TEMPER_REQUIRE_BACKEND_PARITY").as_deref(),
            Ok("1"),
            "DATABASE_URL is required by the backend parity CI gate"
        );
        return;
    };
    let pool = sqlx::PgPool::connect(&database_url)
        .await
        .expect("connect Postgres");
    temper_store_postgres::migration::run_migrations(&pool)
        .await
        .expect("run Postgres migrations");
    let tenant_name = format!("reaction-postgres-{}", uuid::Uuid::new_v4());
    let store = temper_store_postgres::PostgresEventStore::new(pool);
    prove_durable_reaction_contract(
        &tenant_name,
        build_state(&tenant_name, REACTIONS),
        StorageStack::from_postgres(store.clone()),
        BoxedEventStore::new(store),
    )
    .await;
}

#[tokio::test]
async fn redis_matches_durable_reaction_contract_when_available() {
    let Ok(redis_url) = std::env::var("REDIS_URL") else {
        assert_ne!(
            std::env::var("TEMPER_REQUIRE_BACKEND_PARITY").as_deref(),
            Ok("1"),
            "REDIS_URL is required by the backend parity CI gate"
        );
        return;
    };
    let tenant_name = format!("reaction-redis-{}", uuid::Uuid::new_v4());
    let store = temper_store_redis::RedisEventStore::new(&redis_url)
        .await
        .expect("connect Redis");
    prove_durable_reaction_contract(
        &tenant_name,
        build_state(&tenant_name, REACTIONS),
        StorageStack::from_redis(store.clone()),
        BoxedEventStore::new(store),
    )
    .await;
}
