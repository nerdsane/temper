//! Deterministic restart regressions for durable `[[state_timeout]]` behavior.

use temper_runtime::ActorSystem;
use temper_runtime::persistence::{EventMetadata, EventStore, PersistenceEnvelope};
use temper_runtime::scheduler::{install_deterministic_context, sim_now, sim_uuid};
use temper_runtime::tenant::TenantId;
use temper_server::entity_actor::EntityEvent;
use temper_server::registry::SpecRegistry;
use temper_server::{ServerState, StorageStack};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;

const CSDL_XML: &str = include_str!("../../../test-fixtures/specs/model.csdl.xml");

const TIMED_TASK_IOA: &str = r#"
[automaton]
name = "TimedTask"
states = ["Idle", "Running", "TimedOut"]
initial = "Idle"
allow_indefinite_states = ["Idle", "TimedOut"]

[[action]]
name = "Start"
kind = "input"
from = ["Idle"]
to = "Running"

[[action]]
name = "TimeoutFail"
kind = "internal"
from = ["Running"]
to = "TimedOut"

[[state_timeout]]
state = "Running"
after_seconds = 60
on_timeout = "TimeoutFail"
"#;

fn persisted_event(
    persistence_id: &str,
    sequence_nr: u64,
    event: EntityEvent,
) -> PersistenceEnvelope {
    PersistenceEnvelope {
        sequence_nr,
        event_type: event.action.clone(),
        payload: serde_json::to_value(&event).expect("event serialization"),
        metadata: EventMetadata {
            event_id: sim_uuid(),
            causation_id: sim_uuid(),
            correlation_id: sim_uuid(),
            timestamp: event.timestamp,
            actor_id: persistence_id.to_string(),
        },
    }
}

fn restarted_server(store: SimEventStore) -> ServerState {
    let csdl = parse_csdl(CSDL_XML).expect("CSDL parse");
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        csdl,
        CSDL_XML.to_string(),
        &[("TimedTask", TIMED_TASK_IOA)],
    );

    let system = ActorSystem::new("dst-state-timeout-restart");
    let mut state = ServerState::from_registry(system, registry);
    state.set_storage_stack(StorageStack::from_sim(store, None));
    state
}

#[tokio::test]
async fn overdue_timeout_fires_after_restart_without_an_unrelated_dispatch() {
    let (_guard, _clock, _ids) = install_deterministic_context(203);
    let store = SimEventStore::no_faults(203);
    let tenant = TenantId::default();
    let entity_id = "timed-task-overdue";
    let persistence_id = format!("default:TimedTask:{entity_id}");
    let entered_running_at = sim_now() - chrono::Duration::seconds(61);

    let created = EntityEvent {
        action: "Created".to_string(),
        from_status: String::new(),
        to_status: "Idle".to_string(),
        timestamp: entered_running_at,
        params: serde_json::json!({"Id": entity_id}),
        idempotency_key: None,
    };
    let started = EntityEvent {
        action: "Start".to_string(),
        from_status: "Idle".to_string(),
        to_status: "Running".to_string(),
        timestamp: entered_running_at,
        params: serde_json::json!({}),
        idempotency_key: None,
    };
    store
        .append(
            &persistence_id,
            0,
            &[
                persisted_event(&persistence_id, 1, created),
                persisted_event(&persistence_id, 2, started),
            ],
        )
        .await
        .expect("seed durable timed state");

    let state = restarted_server(store);
    state.hydrate_from_store(&tenant).await;

    let mut observed_status = String::new();
    for _ in 0..32 {
        tokio::task::yield_now().await;
        observed_status = state
            .get_tenant_entity_state(&tenant, "TimedTask", entity_id)
            .await
            .expect("hydrated entity remains readable")
            .state
            .status;
        if observed_status == "TimedOut" {
            break;
        }
    }

    assert_eq!(
        observed_status, "TimedOut",
        "a persisted overdue state_timeout must fire after restart without waiting for unrelated action traffic"
    );
}
