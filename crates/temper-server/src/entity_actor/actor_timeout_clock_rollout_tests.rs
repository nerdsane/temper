use super::*;

use temper_runtime::persistence::EventStore;
use temper_store_sim::SimEventStore;

fn timed_table() -> TransitionTable {
    TransitionTable::from_ioa_source(
        r#"
[automaton]
name = "TimedTicket"
states = ["Open", "TimedOut"]
initial = "Open"
allow_indefinite_states = ["TimedOut"]

[[action]]
name = "Heartbeat"
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
reset_on = ["Heartbeat"]
"#,
    )
}

fn envelope(
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

#[tokio::test]
async fn current_legacy_current_writer_rollout_remains_hydratable() {
    let (_guard, _clock, _ids) = temper_runtime::scheduler::install_deterministic_context(230);
    let t0 = chrono::DateTime::parse_from_rfc3339("2024-01-01T00:00:00Z")
        .expect("fixed timestamp")
        .to_utc();
    let t1 = t0 + chrono::Duration::seconds(30);
    let t2 = t1 + chrono::Duration::seconds(30);
    let table = timed_table();
    let store = std::sync::Arc::new(SimEventStore::no_faults(230));
    let persistence_id = "default:TimedTicket:mixed-writer-rollout";
    store
        .append(
            persistence_id,
            0,
            &[
                envelope(
                    persistence_id,
                    "Created",
                    t0,
                    serde_json::json!({
                        "action": "Created",
                        "from_status": "",
                        "to_status": "Open",
                        "timestamp": t0,
                        "params": {},
                        "__temper_state_timeout_clock": {
                            "kind": "active",
                            "reset_at": t0,
                            "reset_version": 1
                        }
                    }),
                ),
                envelope(
                    persistence_id,
                    "Heartbeat",
                    t1,
                    serde_json::json!({
                        "action": "Heartbeat",
                        "from_status": "Open",
                        "to_status": "Open",
                        "timestamp": t1,
                        "params": {}
                    }),
                ),
                envelope(
                    persistence_id,
                    "Heartbeat",
                    t2,
                    serde_json::json!({
                        "action": "Heartbeat",
                        "from_status": "Open",
                        "to_status": "Open",
                        "timestamp": t2,
                        "params": {},
                        "__temper_state_timeout_clock": {
                            "kind": "active",
                            "reset_at": t2,
                            "reset_version": 3
                        }
                    }),
                ),
            ],
        )
        .await
        .expect("seed current, legacy, current clock history");

    let recovered = recover_entity_state_from_store(
        "default",
        "TimedTicket",
        "mixed-writer-rollout",
        &table,
        &BoxedEventStore::from_arc(store),
        BackendLabel::Sim,
        &serde_json::json!({}),
        None,
        true,
    )
    .await
    .expect("a rolling deployment may interleave current and legacy writers");

    assert_eq!(recovered.status, "Open");
    assert_eq!(recovered.sequence_nr, 3);
    assert_eq!(recovered.state_timeout_clock_reset_at, Some(t2));
    assert_eq!(recovered.state_timeout_clock_reset_version, Some(3));
}

#[tokio::test]
async fn rollback_writer_after_current_snapshot_remains_hydratable() {
    let (_guard, _clock, _ids) = temper_runtime::scheduler::install_deterministic_context(231);
    let t0 = chrono::DateTime::parse_from_rfc3339("2024-01-01T00:00:00Z")
        .expect("fixed timestamp")
        .to_utc();
    let t1 = t0 + chrono::Duration::seconds(30);
    let table = timed_table();
    let store = std::sync::Arc::new(SimEventStore::no_faults(231));
    let persistence_id = "default:TimedTicket:rollback-writer";
    store
        .append(
            persistence_id,
            0,
            &[envelope(
                persistence_id,
                "Created",
                t0,
                serde_json::json!({
                    "action": "Created",
                    "from_status": "",
                    "to_status": "Open",
                    "timestamp": t0,
                    "params": {},
                    "__temper_state_timeout_clock": {
                        "kind": "active",
                        "reset_at": t0,
                        "reset_version": 1
                    }
                }),
            )],
        )
        .await
        .expect("seed current-format boundary event");

    let mut snapshot_state = EntityActor::build_initial_state(
        "TimedTicket",
        "rollback-writer",
        &table,
        &serde_json::json!({}),
    );
    snapshot_state.sequence_nr = 1;
    snapshot_state.total_event_count = 1;
    snapshot_state.last_snapshot_sequence_nr = 1;
    snapshot_state.state_timeout_clock_reset_at = Some(t0);
    snapshot_state.state_timeout_clock_reset_version = Some(1);
    let snapshot =
        EntityActor::serialize_snapshot_state(&snapshot_state).expect("encode current snapshot");
    store
        .save_snapshot(persistence_id, 1, &snapshot)
        .await
        .expect("persist current snapshot boundary");
    store
        .append(
            persistence_id,
            1,
            &[envelope(
                persistence_id,
                "Heartbeat",
                t1,
                serde_json::json!({
                    "action": "Heartbeat",
                    "from_status": "Open",
                    "to_status": "Open",
                    "timestamp": t1,
                    "params": {}
                }),
            )],
        )
        .await
        .expect("a rolled-back writer appends a legacy tail");

    let recovered = recover_entity_state_from_store(
        "default",
        "TimedTicket",
        "rollback-writer",
        &table,
        &BoxedEventStore::from_arc(store),
        BackendLabel::Sim,
        &serde_json::json!({}),
        None,
        true,
    )
    .await
    .expect("rolling forward must hydrate a legacy suffix after a current snapshot");

    assert_eq!(recovered.status, "Open");
    assert_eq!(recovered.sequence_nr, 2);
    assert_eq!(recovered.state_timeout_clock_reset_at, Some(t1));
    assert_eq!(recovered.state_timeout_clock_reset_version, Some(2));
}
