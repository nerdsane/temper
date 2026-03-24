use super::super::*;
use super::ORDER_IOA;

#[test]
fn test_agent_trigger_parsed() {
    let spec = r#"
[automaton]
name = "Project"
states = ["Draft", "Ready"]
initial = "Draft"

[[action]]
name = "MarkReady"
from = ["Draft"]
to = "Ready"

[[agent_trigger]]
name = "test_on_ready"
on_action = "MarkReady"
to_state = "Ready"
agent_role = "tester"
agent_goal = "Run integration tests"
agent_type_id = "tester-type-1"
"#;
    let automaton = parse_automaton(spec).expect("agent_trigger should parse");
    assert_eq!(automaton.agent_triggers.len(), 1);
    let trigger = &automaton.agent_triggers[0];
    assert_eq!(trigger.name, "test_on_ready");
    assert_eq!(trigger.on_action, "MarkReady");
    assert_eq!(trigger.to_state, Some("Ready".to_string()));
    assert_eq!(trigger.agent_role, "tester");
    assert_eq!(trigger.agent_goal, "Run integration tests");
    assert_eq!(trigger.agent_type_id, Some("tester-type-1".to_string()));
    assert!(trigger.agent_model.is_none());
}

#[test]
fn test_agent_trigger_defaults_empty() {
    let automaton = parse_automaton(ORDER_IOA).expect("should parse");
    assert!(automaton.agent_triggers.is_empty());
}
