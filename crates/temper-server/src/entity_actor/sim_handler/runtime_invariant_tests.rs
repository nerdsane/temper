use super::*;
use temper_runtime::scheduler::{
    FaultConfig, SimActorSystem, SimActorSystemConfig, install_deterministic_context,
};

const ORDER_IOA: &str = include_str!("../../../../../test-fixtures/specs/order.ioa.toml");

#[test]
fn unsupported_invariant_is_retained_as_deterministic_simulation_violation() {
    let unsupported_ioa = r#"
[automaton]
name = "Order"
states = ["Draft"]
initial = "Draft"
allow_indefinite_states = ["Draft"]

[[invariant]]
name = "UnsupportedQuota"
when = ["Draft"]
assert = "used_bytes ** quota_limit"
"#;
    let config = SimActorSystemConfig {
        seed: 213,
        max_ticks: 0,
        faults: FaultConfig::none(),
        ..SimActorSystemConfig::default()
    };
    let mut simulation = SimActorSystem::new(config);
    let handler = EntityActorHandler::new(
        "Order",
        "o1",
        Arc::new(TransitionTable::from_ioa_source(ORDER_IOA)),
    )
    .with_ioa_invariants(unsupported_ioa);
    simulation.register_actor("o1", Box::new(handler));

    let result = simulation.run_random();

    assert!(!result.all_invariants_held);
    assert_eq!(result.transitions, 0);
    assert!(simulation.has_violations());
    assert!(
        simulation
            .violations()
            .iter()
            .any(|violation| violation.description.contains("UnsupportedQuota"))
    );
}

#[test]
fn undeclared_counter_and_trigger_state_fail_closed_in_direct_simulation() {
    let cases = [
        (
            "UndeclaredCounter",
            r#"
[automaton]
name = "Order"
states = ["Draft"]
initial = "Draft"
allow_indefinite_states = ["Draft"]

[[invariant]]
name = "UndeclaredCounter"
when = ["Draft"]
assert = "ghost > 0"
"#,
        ),
        (
            "UndeclaredTrigger",
            r#"
[automaton]
name = "Order"
states = ["Draft"]
initial = "Draft"
allow_indefinite_states = ["Draft"]

[[invariant]]
name = "UndeclaredTrigger"
when = ["Ghost"]
assert = "true"
"#,
        ),
    ];

    for (name, ioa) in cases {
        let mut simulation = SimActorSystem::new(SimActorSystemConfig {
            seed: 213,
            faults: FaultConfig::none(),
            ..SimActorSystemConfig::default()
        });
        let handler = EntityActorHandler::new(
            "Order",
            "o1",
            Arc::new(TransitionTable::from_ioa_source(ORDER_IOA)),
        )
        .with_ioa_invariants(ioa);
        simulation.register_actor("o1", Box::new(handler));

        simulation.step("o1", "AddItem", "{}").expect("step order");

        assert!(
            simulation
                .violations()
                .iter()
                .any(|violation| violation.description.contains(name)),
            "{name} must be retained as an unsupported invariant violation"
        );
    }
}

#[test]
fn runtime_classification_requires_matching_table_enforcement_artifact() {
    let ioa = r#"
[automaton]
name = "Order"
states = ["Draft"]
initial = "Draft"
allow_indefinite_states = ["Draft"]

[[state]]
name = "goal"
type = "string"
initial = ""

[[invariant]]
name = "GoalRequired"
when = ["Draft"]
assert = "goal != ''"
"#;
    let mut simulation = SimActorSystem::new(SimActorSystemConfig {
        seed: 213,
        faults: FaultConfig::none(),
        ..SimActorSystemConfig::default()
    });
    let handler = EntityActorHandler::new(
        "Order",
        "o1",
        Arc::new(TransitionTable::from_ioa_source(ORDER_IOA)),
    )
    .with_ioa_invariants(ioa);
    simulation.register_actor("o1", Box::new(handler));

    simulation.step("o1", "AddItem", "{}").expect("step order");

    assert!(simulation.has_violations());
    assert!(
        simulation
            .violations()
            .iter()
            .any(|violation| violation.description.contains("GoalRequired"))
    );
}

#[test]
fn runtime_string_invariant_rejects_and_rolls_back_tentative_state() {
    let ioa = r#"
[automaton]
name = "Agent"
states = ["Idle", "Assigned"]
initial = "Idle"

[[state]]
name = "goal"
type = "string"
initial = ""

[[action]]
name = "Assign"
from = ["Idle"]
to = "Assigned"
params = ["goal"]

[[invariant]]
name = "AssignedRequiresGoal"
when = ["Assigned"]
assert = "goal != ''"
"#;
    let (_guard, _clock, _id_gen) = install_deterministic_context(213);
    let table = Arc::new(TransitionTable::from_ioa_source(ioa));
    let mut handler = EntityActorHandler::new("Agent", "a1", table);
    handler.init().expect("initialize actor");

    let rejected = handler.handle_message("Assign", r#"{"goal":""}"#);
    assert!(rejected.is_err());
    assert_eq!(handler.current_status(), "Idle");
    assert_eq!(handler.event_count(), 0);

    handler
        .handle_message("Assign", r#"{"goal":"ship safely"}"#)
        .expect("non-empty goal satisfies runtime invariant");
    assert_eq!(handler.current_status(), "Assigned");
    assert_eq!(handler.event_count(), 1);
}

#[test]
fn simulation_initialization_matches_production_runtime_invariant_gate() {
    let ioa = r#"
[automaton]
name = "ToolCall"
states = ["Pending"]
initial = "Pending"
allow_indefinite_states = ["Pending"]

[[state]]
name = "agent_id"
type = "string"
initial = ""

[[action]]
name = "Initialize"
kind = "input"
from = ["Pending"]
params = ["agent_id"]

[[invariant]]
name = "RequiresAgentId"
when = ["Pending"]
assert = "agent_id != ''"
"#;
    let (_guard, _clock, _id_gen) = install_deterministic_context(213);
    let table = Arc::new(TransitionTable::from_ioa_source(ioa));
    let mut invalid = EntityActorHandler::new("ToolCall", "tc-invalid", table.clone());
    invalid
        .init()
        .expect("action-backed pristine state must await initialization");
    assert_eq!(invalid.event_count(), 0);
    invalid
        .handle_message("Initialize", r#"{"agent_id":"agent-1"}"#)
        .expect("initializing action establishes runtime safety");
    assert_eq!(invalid.event_count(), 1);

    let mut valid = EntityActorHandler::new("ToolCall", "tc-valid", table)
        .with_initial_fields(serde_json::json!({"agent_id": "agent-1"}));
    valid
        .init()
        .expect("matching initial fields satisfy the production gate");
}

#[test]
fn runtime_counter_invariant_rejects_over_quota_mutation_atomically() {
    const WORKSPACE_IOA: &str =
        include_str!("../../../../../os-apps/temper-fs/specs/workspace.ioa.toml");
    let (_guard, _clock, _id_gen) = install_deterministic_context(213);
    let table = Arc::new(TransitionTable::from_ioa_source(WORKSPACE_IOA));
    let mut handler = EntityActorHandler::new("Workspace", "w1", table);
    handler.init().expect("initialize workspace");
    handler
        .handle_message("Create", r#"{"name":"docs","quota_limit":10}"#)
        .expect("set initial quota");

    let rejected = handler.handle_message("IncrementUsage", r#"{"size_bytes":11}"#);
    assert!(rejected.is_err());
    assert_eq!(
        handler
            .state
            .counters
            .get("used_bytes")
            .copied()
            .unwrap_or(0),
        0
    );
    assert_eq!(handler.state.counters.get("quota_limit"), Some(&10));
    assert_eq!(handler.event_count(), 1);

    handler
        .handle_message("IncrementUsage", r#"{"size_bytes":10}"#)
        .expect("usage at quota is valid");
    assert_eq!(handler.state.counters.get("used_bytes"), Some(&10));
}
