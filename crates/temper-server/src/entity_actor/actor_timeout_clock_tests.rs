use super::*;

#[cfg(feature = "sim")]
fn timeout_clock_timed_table() -> TransitionTable {
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

#[cfg(feature = "sim")]
fn timeout_clock_untimed_table() -> TransitionTable {
    TransitionTable::from_ioa_source(
        r#"
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
"#,
    )
}

#[test]
fn missing_clock_repair_uses_a_retained_reset_without_the_older_entry() {
    let table = TransitionTable::from_ioa_source(
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
reset_on = ["Heartbeat"]
"#,
    );
    let reset_at = chrono::DateTime::<chrono::Utc>::from_timestamp(1_700_000_000, 0)
        .expect("valid reset timestamp");
    let repair_at = reset_at + chrono::Duration::seconds(30);
    let mut state = EntityActor::build_initial_state(
        "TimedTicket",
        "retained-reset-only",
        &table,
        &serde_json::json!({}),
    );
    state.total_event_count = 1;
    state.events.push_back(EntityEvent {
        action: "Heartbeat".to_string(),
        from_status: "Open".to_string(),
        to_status: "Open".to_string(),
        timestamp: reset_at,
        params: serde_json::json!({}),
        idempotency_key: None,
    });

    let unrelated = EntityEvent {
        action: "Observe".to_string(),
        from_status: "Open".to_string(),
        to_status: "Open".to_string(),
        timestamp: repair_at,
        params: serde_json::json!({}),
        idempotency_key: None,
    };
    EntityActor::update_state_timeout_clock(&table, &mut state, &unrelated);

    assert_eq!(
        state.state_timeout_clock_reset_at,
        Some(reset_at),
        "a retained same-state reset is a durable anchor even when the older entry is outside the tail"
    );
    assert_eq!(state.state_timeout_clock_reset_version, Some(2));
}

#[cfg(feature = "sim")]
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
async fn present_malformed_clock_metadata_is_fatal_for_tombstones_and_timeout_free_tables() {
    use temper_runtime::persistence::EventStore;
    use temper_store_sim::SimEventStore;

    let timestamp = chrono::DateTime::parse_from_rfc3339("2024-01-01T00:00:00Z")
        .expect("fixed timestamp")
        .to_utc();

    let tombstone_store = std::sync::Arc::new(SimEventStore::no_faults(226));
    let tombstone_pid = "default:TimedTicket:malformed-tombstone-clock";
    tombstone_store
        .append(
            tombstone_pid,
            0,
            &[persisted_test_envelope(
                tombstone_pid,
                "Deleted",
                timestamp,
                serde_json::json!({
                    "action": "Deleted",
                    "from_status": "Open",
                    "to_status": "Deleted",
                    "timestamp": timestamp,
                    "params": {},
                    "__temper_state_timeout_clock": {
                        "kind": "active",
                        "reset_at": timestamp,
                        "reset_version": "not-a-version"
                    }
                }),
            )],
        )
        .await
        .expect("seed malformed tombstone");
    let timed = timeout_clock_timed_table();
    let tombstone_error = recover_entity_state_from_store(
        "default",
        "TimedTicket",
        "malformed-tombstone-clock",
        &timed,
        &BoxedEventStore::from_arc(tombstone_store),
        BackendLabel::Sim,
        &serde_json::json!({}),
        None,
        true,
    )
    .await
    .expect_err("a tombstone cannot discard present malformed clock metadata");
    assert!(
        tombstone_error
            .to_string()
            .contains("invalid persisted state-timeout clock metadata"),
        "unexpected tombstone error: {tombstone_error}"
    );

    let timeout_free_store = std::sync::Arc::new(SimEventStore::no_faults(227));
    let timeout_free_pid = "default:Order:null-clock";
    timeout_free_store
        .append(
            timeout_free_pid,
            0,
            &[persisted_test_envelope(
                timeout_free_pid,
                "CancelOrder",
                timestamp,
                serde_json::json!({
                    "action": "CancelOrder",
                    "from_status": "Draft",
                    "to_status": "Cancelled",
                    "timestamp": timestamp,
                    "params": {},
                    "__temper_state_timeout_clock": null
                }),
            )],
        )
        .await
        .expect("seed null clock metadata");
    let order = timeout_clock_untimed_table();
    let timeout_free_error = recover_entity_state_from_store(
        "default",
        "Order",
        "null-clock",
        &order,
        &BoxedEventStore::from_arc(timeout_free_store),
        BackendLabel::Sim,
        &serde_json::json!({}),
        None,
        true,
    )
    .await
    .expect_err("explicit null metadata is not legacy absence");
    assert!(
        timeout_free_error
            .to_string()
            .contains("invalid persisted state-timeout clock metadata"),
        "unexpected timeout-free error: {timeout_free_error}"
    );
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn authoritative_clock_replay_rejects_reused_and_decreasing_reset_versions() {
    use temper_runtime::persistence::EventStore;
    use temper_store_sim::SimEventStore;

    let t0 = chrono::DateTime::parse_from_rfc3339("2024-01-01T00:00:00Z")
        .expect("fixed timestamp")
        .to_utc();
    let t1 = t0 + chrono::Duration::seconds(30);
    let timed = timeout_clock_timed_table();

    let reused_store = std::sync::Arc::new(SimEventStore::no_faults(228));
    let reused_pid = "default:TimedTicket:reused-clock-version";
    reused_store
        .append(
            reused_pid,
            0,
            &[
                persisted_test_envelope(
                    reused_pid,
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
                persisted_test_envelope(
                    reused_pid,
                    "Heartbeat",
                    t1,
                    serde_json::json!({
                        "action": "Heartbeat",
                        "from_status": "Open",
                        "to_status": "Open",
                        "timestamp": t1,
                        "params": {},
                        "__temper_state_timeout_clock": {
                            "kind": "active",
                            "reset_at": t1,
                            "reset_version": 1
                        }
                    }),
                ),
            ],
        )
        .await
        .expect("seed reused clock identity");
    let reused_error = recover_entity_state_from_store(
        "default",
        "TimedTicket",
        "reused-clock-version",
        &timed,
        &BoxedEventStore::from_arc(reused_store),
        BackendLabel::Sim,
        &serde_json::json!({}),
        None,
        true,
    )
    .await
    .expect_err("a reused version cannot change its timestamp");
    assert!(
        reused_error.to_string().contains("non-monotonic persisted"),
        "unexpected reused-version error: {reused_error}"
    );

    let decreasing_store = std::sync::Arc::new(SimEventStore::no_faults(229));
    let decreasing_pid = "default:TimedTicket:decreasing-clock-version";
    decreasing_store
        .append(
            decreasing_pid,
            0,
            &[
                persisted_test_envelope(
                    decreasing_pid,
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
                persisted_test_envelope(
                    decreasing_pid,
                    "Heartbeat",
                    t1,
                    serde_json::json!({
                        "action": "Heartbeat",
                        "from_status": "Open",
                        "to_status": "Open",
                        "timestamp": t1,
                        "params": {},
                        "__temper_state_timeout_clock": {
                            "kind": "active",
                            "reset_at": t1,
                            "reset_version": 2
                        }
                    }),
                ),
                persisted_test_envelope(
                    decreasing_pid,
                    "Heartbeat",
                    t1,
                    serde_json::json!({
                        "action": "Heartbeat",
                        "from_status": "Open",
                        "to_status": "Open",
                        "timestamp": t1,
                        "params": {},
                        "__temper_state_timeout_clock": {
                            "kind": "active",
                            "reset_at": t0,
                            "reset_version": 1
                        }
                    }),
                ),
            ],
        )
        .await
        .expect("seed decreasing clock identity");
    let decreasing_error = recover_entity_state_from_store(
        "default",
        "TimedTicket",
        "decreasing-clock-version",
        &timed,
        &BoxedEventStore::from_arc(decreasing_store),
        BackendLabel::Sim,
        &serde_json::json!({}),
        None,
        true,
    )
    .await
    .expect_err("an authoritative clock cannot move to an older identity");
    assert!(
        decreasing_error
            .to_string()
            .contains("non-monotonic persisted"),
        "unexpected decreasing-version error: {decreasing_error}"
    );
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn authoritative_clock_replay_rejects_a_later_legacy_event() {
    use temper_runtime::persistence::EventStore;
    use temper_store_sim::SimEventStore;

    let t0 = chrono::DateTime::parse_from_rfc3339("2024-01-01T00:00:00Z")
        .expect("fixed timestamp")
        .to_utc();
    let t1 = t0 + chrono::Duration::seconds(30);
    let t2 = t1 + chrono::Duration::seconds(30);
    let table = timeout_clock_timed_table();
    let store = std::sync::Arc::new(SimEventStore::no_faults(230));
    let persistence_id = "default:TimedTicket:legacy-after-checkpoint";
    store
        .append(
            persistence_id,
            0,
            &[
                persisted_test_envelope(
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
                persisted_test_envelope(
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
                persisted_test_envelope(
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

    let error = recover_entity_state_from_store(
        "default",
        "TimedTicket",
        "legacy-after-checkpoint",
        &table,
        &BoxedEventStore::from_arc(store),
        BackendLabel::Sim,
        &serde_json::json!({}),
        None,
        true,
    )
    .await
    .expect_err("legacy metadata cannot follow an authoritative checkpoint");
    assert!(
        error
            .to_string()
            .contains("missing persisted state-timeout clock"),
        "unexpected missing-clock error: {error}"
    );
}
