use super::*;

use std::time::Duration;
use temper_runtime::ActorSystem;

fn persisted_test_envelope(
    persistence_id: &str,
    event_type: &str,
    timestamp: chrono::DateTime<chrono::Utc>,
    payload: serde_json::Value,
) -> PersistenceEnvelope {
    PersistenceEnvelope {
        sequence_nr: 0,
        event_type: event_type.to_string(),
        payload,
        metadata: EventMetadata {
            event_id: sim_uuid(),
            causation_id: sim_uuid(),
            correlation_id: sim_uuid(),
            timestamp,
            actor_id: persistence_id.to_string(),
        },
    }
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn inactive_current_payloads_migrate_when_a_timeout_is_added_before_restart() {
    use temper_runtime::persistence::EventStore;
    use temper_store_sim::SimEventStore;

    const UNTYPED_TIMEOUT_IOA: &str = r#"
[automaton]
name = "EvolvingTicket"
states = ["Open", "TimedOut"]
initial = "Open"
allow_indefinite_states = ["Open", "TimedOut"]

[[action]]
name = "Observe"
kind = "input"
from = ["Open"]
to = "Open"
"#;
    const TIMED_IOA: &str = r#"
[automaton]
name = "EvolvingTicket"
states = ["Open", "TimedOut"]
initial = "Open"
allow_indefinite_states = ["TimedOut"]

[[action]]
name = "Observe"
kind = "input"
from = ["Open"]
to = "Open"

[[action]]
name = "TimeoutFail"
kind = "internal"
from = ["Open"]
to = "TimedOut"

[[state_timeout]]
state = "Open"
after_seconds = 60
on_timeout = "TimeoutFail"
"#;

    let untimed = TransitionTable::from_ioa_source(UNTYPED_TIMEOUT_IOA);
    let timed = Arc::new(RwLock::new(TransitionTable::from_ioa_source(TIMED_IOA)));
    let t0 = chrono::DateTime::parse_from_rfc3339("2024-01-01T00:00:00Z")
        .expect("fixed timestamp")
        .to_utc();
    let t1 = t0 + chrono::Duration::seconds(30);
    let store = std::sync::Arc::new(SimEventStore::no_faults(230));
    let persistence_id = "default:EvolvingTicket:timeout-added";
    let current_inactive_event =
        |action: &str, from_status: &str, timestamp: chrono::DateTime<chrono::Utc>| {
            persisted_test_envelope(
                persistence_id,
                action,
                timestamp,
                serde_json::json!({
                    "action": action,
                    "from_status": from_status,
                    "to_status": "Open",
                    "timestamp": timestamp,
                    "params": {},
                    "__temper_state_timeout_clock": { "kind": "inactive" }
                }),
            )
        };
    store
        .append(
            persistence_id,
            0,
            &[
                current_inactive_event("Created", "", t0),
                current_inactive_event("Observe", "Open", t1),
            ],
        )
        .await
        .expect("seed current events committed before the timeout existed");

    let actor = EntityActor::with_persistence(
        "EvolvingTicket",
        "timeout-added",
        timed,
        serde_json::json!({}),
        BoxedEventStore::from_arc(store),
        BackendLabel::Sim,
    );
    let system = ActorSystem::new("timeout-added-current-payload-replay");
    let actor_ref = system.spawn(actor, "timeout-added");
    let response: EntityResponse = actor_ref
        .ask(EntityMsg::GetState, Duration::from_secs(1))
        .await
        .expect("journal-only restart migrates the newly declared timeout");

    assert_eq!(response.state.status, "Open");
    assert_eq!(response.state.state_timeout_clock_reset_at, Some(t0));
    assert_eq!(response.state.state_timeout_clock_reset_version, Some(1));
    assert_eq!(
        untimed.state_timeouts.len(),
        0,
        "fixture committed inactive clocks"
    );
}
