//! ADR-0178 durable state-timeout creation and restart recovery.

use std::time::{Duration, Instant};

use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::registry::SpecRegistry;
use temper_server::state::ServerState;
use temper_server::storage::{BoxedEventStore, StorageStack};
use temper_spec::csdl::parse_csdl;
use temper_store_turso::TursoEventStore;

const CSDL_XML: &str = include_str!("../../../test-fixtures/specs/model.csdl.xml");

const TICKET_WITH_TIMEOUT_IOA: &str = r#"
[automaton]
name = "Ticket"
states = ["Open", "InProgress", "Closed"]
initial = "Open"
allow_indefinite_states = ["InProgress", "Closed"]

[[action]]
name = "AssignAgent"
kind = "input"
from = ["Open"]
to = "InProgress"

[[action]]
name = "Heartbeat"
kind = "input"
from = ["Open"]
to = "Open"

[[action]]
name = "Close"
kind = "input"
from = ["Open"]
to = "Closed"

[[action]]
name = "Reopen"
kind = "input"
from = ["InProgress"]
to = "Open"

[[state_timeout]]
state = "Open"
after_seconds = 1
on_timeout = "AssignAgent"
reset_on = ["Heartbeat"]
params = { reason = "state deadline elapsed" }
"#;

const TIMEOUT_POLICY: &str = r#"
permit(
    principal is Agent,
    action == Action::"AssignAgent",
    resource is Ticket
) when {
    principal.agent_type == "timeout-scheduler"
};
permit(principal is Customer, action, resource) when {
    principal.id == "anonymous"
};
"#;

fn build_state_with_policy(system_name: &str, store: TursoEventStore, policy: &str) -> ServerState {
    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(CSDL_XML).expect("CSDL should parse");
    registry.register_tenant(
        "tenant-a",
        csdl,
        CSDL_XML.to_string(),
        &[("Ticket", TICKET_WITH_TIMEOUT_IOA)],
    );
    let mut state = ServerState::from_registry(ActorSystem::new(system_name), registry);
    state
        .authz
        .reload_tenant_policies("tenant-a", policy)
        .expect("timeout fixture policy should parse");
    state.set_storage_stack(StorageStack::from_turso(store));
    state
}

fn build_state(system_name: &str, store: TursoEventStore) -> ServerState {
    build_state_with_policy(system_name, store, TIMEOUT_POLICY)
}

async fn open_store(db_url: &str) -> TursoEventStore {
    TursoEventStore::new(db_url, None)
        .await
        .expect("open local Turso database")
}

async fn wait_for_status(
    state: &ServerState,
    tenant: &TenantId,
    entity_id: &str,
    expected: &str,
    deadline: Duration,
) -> String {
    let started = Instant::now(); // determinism-ok: integration-test wall deadline only
    loop {
        let current = state
            .get_tenant_entity_state(tenant, "Ticket", entity_id)
            .await
            .expect("entity should load")
            .state
            .status;
        if current == expected || started.elapsed() >= deadline {
            return current;
        }
        tokio::time::sleep(Duration::from_millis(50)).await; // determinism-ok: integration-test polling only
    }
}

async fn wait_for_delivery_status(
    store: &BoxedEventStore,
    tenant: &TenantId,
    expected: temper_server::trigger::delivery::ReactionDeliveryStatus,
    deadline: Duration,
) -> (
    temper_server::trigger::delivery::ReactionDeliveryRecord,
    u64,
) {
    let started = Instant::now(); // determinism-ok: integration-test wall deadline only
    loop {
        let records =
            temper_server::trigger::delivery::list_delivery_records(store, tenant.as_str(), 10)
                .await
                .expect("delivery records should load");
        if let Some(record) = records
            .into_iter()
            .find(|(record, _)| record.status == expected)
        {
            return record;
        }
        assert!(
            started.elapsed() < deadline,
            "delivery did not reach {expected:?} before bounded deadline"
        );
        tokio::time::sleep(Duration::from_millis(50)).await; // determinism-ok: integration-test polling only
    }
}

fn run_and_hard_kill_generation_a(db_url: &str, entity_id: &str) {
    let db_url = db_url.to_string();
    let entity_id = entity_id.to_string();
    std::thread::spawn(move || {
        // determinism-ok: test-only process-generation isolation
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("generation-A runtime");
        runtime.block_on(async move {
            let tenant = TenantId::new("tenant-a");
            let state = build_state("timeout-generation-a", open_store(&db_url).await);
            let created = state
                .get_or_create_tenant_entity(&tenant, "Ticket", &entity_id, serde_json::json!({}))
                .await
                .expect("create timed entity");
            assert_eq!(created.state.status, "Open");
        });
        runtime.shutdown_timeout(Duration::from_millis(100));
    })
    .join()
    .expect("generation A should stop cleanly");
}

#[path = "state_timeout_durable_recovery/creation.rs"]
mod creation;
#[path = "state_timeout_durable_recovery/lifecycle.rs"]
mod lifecycle;
