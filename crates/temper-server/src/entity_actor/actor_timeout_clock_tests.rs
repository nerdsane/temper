use super::*;

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
