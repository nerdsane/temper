mod common;

use common::reaction_fixture::*;
use temper_runtime::persistence::{EventMetadata, PersistenceEnvelope};
use temper_runtime::scheduler::{sim_now, sim_uuid};
use temper_server::registry::SpecRegistry;
use temper_server::trigger::delivery::{
    DeliveryKind, ReactionDeliveryStatus, extract_intents, find_delivery_record,
};
use temper_spec::csdl::parse_csdl;

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

const TIMEOUT_ORDER_IOA: &str = r#"
[automaton]
name = "Order"
states = ["Draft", "Cancelled"]
initial = "Draft"
allow_indefinite_states = ["Cancelled"]

[[action]]
name = "CancelOrder"
kind = "input"
from = ["Draft"]
to = "Cancelled"

[[state_timeout]]
state = "Draft"
after_seconds = 3600
on_timeout = "CancelOrder"
"#;

fn build_timeout_state(tenant: &str) -> ServerState {
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        tenant,
        parse_csdl(CSDL_XML).expect("CSDL should parse"),
        CSDL_XML.to_string(),
        &[("Order", TIMEOUT_ORDER_IOA), ("Payment", PAYMENT_IOA)],
    );
    let state = ServerState::from_registry(ActorSystem::new("timeout-backend-parity"), registry);
    state
        .authz
        .reload_tenant_policies(tenant, "permit(principal, action, resource);")
        .expect("timeout parity policy should parse");
    state
}

async fn prove_durable_timeout_persistence_contract(
    tenant_name: &str,
    mut state: ServerState,
    stack: StorageStack,
    store: BoxedEventStore,
) {
    state.set_storage_stack(stack);
    let tenant = TenantId::new(tenant_name);
    state
        .get_or_create_tenant_entity(&tenant, "Order", "deadline-1", serde_json::json!({}))
        .await
        .expect("create timed entity");
    let source = store
        .read_events(&format!("{tenant_name}:Order:deadline-1"), 0)
        .await
        .expect("read timeout source journal");
    let intent = source
        .iter()
        .find(|event| event.event_type == "Created")
        .and_then(|event| extract_intents(&event.payload).ok())
        .and_then(|intents| intents.into_iter().next())
        .expect("timeout intent must be co-committed");
    assert_eq!(intent.kind, DeliveryKind::StateTimeout);
    let deadline = intent.not_before.expect("absolute timeout deadline");
    assert_eq!(
        deadline.signed_duration_since(intent.created_at),
        chrono::Duration::hours(1)
    );
    let (record, _) = find_delivery_record(&store, tenant_name, &intent.delivery_id)
        .await
        .expect("read timeout delivery journal")
        .expect("timeout lifecycle must be materialized");
    assert_eq!(record.status, ReactionDeliveryStatus::Pending);
    assert_eq!(record.next_attempt_at, Some(deadline));
}

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
        BoxedEventStore::new(store.clone()),
    )
    .await;
    prove_durable_timeout_persistence_contract(
        "timeout-turso-parity",
        build_timeout_state("timeout-turso-parity"),
        StorageStack::from_turso(store.clone()),
        BoxedEventStore::new(store.clone()),
    )
    .await;
}

#[tokio::test]
async fn sim_matches_durable_timeout_persistence_contract() {
    let store = SimEventStore::no_faults(418);
    prove_durable_timeout_persistence_contract(
        "timeout-sim-parity",
        build_timeout_state("timeout-sim-parity"),
        StorageStack::from_sim(store.clone(), None),
        BoxedEventStore::new(store.clone()),
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
        BoxedEventStore::new(store.clone()),
    )
    .await;
    let timeout_tenant = format!("timeout-postgres-{}", uuid::Uuid::new_v4());
    prove_durable_timeout_persistence_contract(
        &timeout_tenant,
        build_timeout_state(&timeout_tenant),
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
        BoxedEventStore::new(store.clone()),
    )
    .await;
    let timeout_tenant = format!("timeout-redis-{}", uuid::Uuid::new_v4());
    prove_durable_timeout_persistence_contract(
        &timeout_tenant,
        build_timeout_state(&timeout_tenant),
        StorageStack::from_redis(store.clone()),
        BoxedEventStore::new(store),
    )
    .await;
}
