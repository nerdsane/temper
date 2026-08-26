use temper_spec::automaton::FailureCategory;

use super::TransitionTable;

const SPEC: &str = r#"
[automaton]
name = "Payment"
states = ["Created", "Charging", "RetryScheduled", "AwaitingApproval"]
initial = "Created"

[[action]]
name = "Charge"
from = ["Created"]
to = "Charging"

[[action.triggers]]
name = "charge_card"
kind = "wasm"
module = "payments"

[[action.triggers.failure_routes]]
category = "transient"
action = "ScheduleRetry"

[[action.triggers.failure_routes]]
category = "authorization"
to_state = "AwaitingApproval"

[[action]]
name = "ScheduleRetry"
from = ["Charging"]
to = "RetryScheduled"
params = [{ name = "failure", type = "failure_v1" }]

[[action]]
name = "AwaitApproval"
from = ["Charging"]
to = "AwaitingApproval"
params = [{ name = "failure", type = "failure_v1" }]
"#;

#[test]
fn transition_table_carries_resolved_failure_actions() {
    let table = TransitionTable::try_from_ioa_source(SPEC).expect("valid transition table");
    assert_eq!(table.failure_routes.len(), 2);
    assert_eq!(table.failure_routes[0].source_action, "Charge");
    assert_eq!(table.failure_routes[0].trigger_name, "charge_card");
    assert_eq!(table.failure_routes[0].category, FailureCategory::Transient);
    assert_eq!(table.failure_routes[0].callback_action, "ScheduleRetry");
    assert_eq!(
        table.failure_routes[1].category,
        FailureCategory::Authorization
    );
    assert_eq!(table.failure_routes[1].callback_action, "AwaitApproval");
}

#[test]
fn transition_table_failure_routes_survive_serialization() {
    let table = TransitionTable::try_from_ioa_source(SPEC).expect("valid transition table");
    let encoded = serde_json::to_vec(&table).expect("serialize transition table");
    let decoded: TransitionTable =
        serde_json::from_slice(&encoded).expect("deserialize transition table");
    assert_eq!(decoded.failure_routes, table.failure_routes);
}

#[test]
fn old_transition_tables_default_to_no_failure_routes() {
    let table = TransitionTable::try_from_ioa_source(SPEC).expect("valid transition table");
    let mut encoded = serde_json::to_value(table).expect("serialize transition table");
    encoded
        .as_object_mut()
        .expect("transition table object")
        .remove("failure_routes");
    let decoded: TransitionTable =
        serde_json::from_value(encoded).expect("deserialize legacy transition table");
    assert!(decoded.failure_routes.is_empty());
}
