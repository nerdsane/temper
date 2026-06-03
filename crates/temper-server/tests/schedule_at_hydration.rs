//! Regression tests for `schedule_at` timer recovery after event-journal hydration.

mod common;

use std::time::Duration;

use serde_json::json;
use temper_runtime::persistence::{EventMetadata, EventStore, PersistenceEnvelope};
use temper_runtime::scheduler::{install_deterministic_context, sim_now, sim_uuid};
use temper_runtime::tenant::TenantId;
use temper_server::entity_actor::EntityEvent;
use temper_store_sim::SimEventStore;

const HYDRATED_CRON_IOA: &str = r#"
[automaton]
name = "HydratedCron"
states = ["Active", "Fired"]
initial = "Active"
allow_indefinite_states = ["Fired"]

[[state]]
name = "next_run_at"
type = "string"
initial = ""

[[action]]
name = "TriggerComplete"
kind = "input"
from = ["Active"]
params = ["next_run_at"]
effect = [{ type = "schedule_at", field = "next_run_at", action = "Trigger" }]

[[action]]
name = "Trigger"
kind = "internal"
from = ["Active"]
to = "Fired"
effect = [{ type = "increment", var = "fires" }]
"#;

async fn append_event(
    store: &SimEventStore,
    persistence_id: &str,
    sequence_nr: u64,
    event: EntityEvent,
) {
    let timestamp = event.timestamp;
    let envelope = PersistenceEnvelope {
        sequence_nr,
        event_type: event.action.clone(),
        payload: serde_json::to_value(event).expect("event serializes"),
        metadata: EventMetadata {
            event_id: sim_uuid(),
            causation_id: sim_uuid(),
            correlation_id: sim_uuid(),
            timestamp,
            actor_id: persistence_id.to_string(),
        },
    };
    store
        .append(persistence_id, sequence_nr - 1, &[envelope])
        .await
        .expect("event append succeeds");
}

#[tokio::test]
async fn hydrate_from_store_rearms_due_schedule_at_timer() {
    let (_guard, _clock, _ids) = install_deterministic_context(138);
    let store = SimEventStore::no_faults(138);
    let tenant = TenantId::default();
    let entity_id = "cron-hydrate-due";
    let persistence_id = format!("{tenant}:HydratedCron:{entity_id}");
    let due_at = (sim_now() - chrono::Duration::seconds(30)).to_rfc3339();

    append_event(
        &store,
        &persistence_id,
        1,
        EntityEvent {
            action: "Created".into(),
            from_status: String::new(),
            to_status: "Active".into(),
            timestamp: sim_now(),
            params: json!({ "Id": entity_id }),
            idempotency_key: None,
        },
    )
    .await;
    append_event(
        &store,
        &persistence_id,
        2,
        EntityEvent {
            action: "TriggerComplete".into(),
            from_status: "Active".into(),
            to_status: "Active".into(),
            timestamp: sim_now(),
            params: json!({ "next_run_at": due_at }),
            idempotency_key: None,
        },
    )
    .await;

    let state = common::build_single_tenant_state_with_store(
        store,
        "schedule-at-hydration",
        "default",
        &[("HydratedCron", HYDRATED_CRON_IOA)],
    );

    state.hydrate_from_store(&tenant).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let hydrated = state
        .get_tenant_entity_state(&tenant, "HydratedCron", entity_id)
        .await
        .expect("hydrated entity should be queryable");

    assert_eq!(hydrated.state.status, "Fired");
    assert_eq!(hydrated.state.counters.get("fires"), Some(&1));
}
