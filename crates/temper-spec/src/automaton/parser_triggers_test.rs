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

// ─── ADR-0046: [[action.triggers]] parser tests ───────────────────────────

#[test]
fn test_action_triggers_entity_kind_parses() {
    let spec = r#"
[automaton]
name = "File"
states = ["Created", "Ready"]
initial = "Created"

[[action]]
name = "StreamUpdated"
from = ["Created", "Ready"]
to = "Ready"

[[action.triggers]]
name = "stream_updated_creates_version"
kind = "entity"
principal = "file-service"
target_entity = "FileVersion"
target_action = "Create"

[action.triggers.resolve_target]
type = "create_if_missing"
id_field = "last_version_id"
"#;
    let automaton = parse_automaton(spec).expect("action.triggers should parse");
    assert_eq!(automaton.actions.len(), 1);
    let action = &automaton.actions[0];
    assert_eq!(action.name, "StreamUpdated");
    assert_eq!(action.triggers.len(), 1);
    let trigger = &action.triggers[0];
    assert_eq!(trigger.name, "stream_updated_creates_version");
    assert_eq!(trigger.kind, TriggerKind::Entity);
    assert_eq!(trigger.principal.as_deref(), Some("file-service"));
    assert_eq!(trigger.target_entity.as_deref(), Some("FileVersion"));
    assert_eq!(trigger.target_action.as_deref(), Some("Create"));
    assert!(matches!(
        trigger.resolve_target,
        Some(TargetResolver::CreateIfMissing { .. })
    ));
}

#[test]
fn test_action_triggers_default_empty() {
    let spec = r#"
[automaton]
name = "Simple"
states = ["A", "B"]
initial = "A"

[[action]]
name = "Move"
from = ["A"]
to = "B"
"#;
    let automaton = parse_automaton(spec).expect("should parse");
    assert_eq!(automaton.actions.len(), 1);
    assert!(automaton.actions[0].triggers.is_empty());
}

#[test]
fn test_action_triggers_no_principal_defaults_to_none() {
    let spec = r#"
[automaton]
name = "Source"
states = ["Start", "Done"]
initial = "Start"

[[action]]
name = "Finish"
from = ["Start"]
to = "Done"

[[action.triggers]]
name = "audit_log"
kind = "entity"
target_entity = "AuditLog"
target_action = "Record"

[action.triggers.resolve_target]
type = "create"
"#;
    let automaton = parse_automaton(spec).expect("trigger without principal should parse");
    let trigger = &automaton.actions[0].triggers[0];
    assert!(
        trigger.principal.is_none(),
        "absent principal inherits invoker"
    );
    assert!(matches!(
        trigger.resolve_target,
        Some(TargetResolver::Create)
    ));
}

#[test]
fn test_action_triggers_multiple_on_one_action() {
    let spec = r#"
[automaton]
name = "File"
states = ["Created", "Ready"]
initial = "Created"

[[action]]
name = "StreamUpdated"
from = ["Created", "Ready"]
to = "Ready"

[[action.triggers]]
name = "create_version"
kind = "entity"
principal = "file-service"
target_entity = "FileVersion"
target_action = "Create"

[action.triggers.resolve_target]
type = "create_if_missing"
id_field = "last_version_id"

[[action.triggers]]
name = "supersede_previous"
kind = "entity"
principal = "file-service"
target_entity = "FileVersion"
target_action = "Supersede"

[action.triggers.resolve_target]
type = "field"
field = "last_version_id"
"#;
    let automaton = parse_automaton(spec).expect("multi-trigger should parse");
    let action = &automaton.actions[0];
    assert_eq!(action.triggers.len(), 2);
    assert_eq!(action.triggers[0].name, "create_version");
    assert_eq!(action.triggers[1].name, "supersede_previous");
    assert!(matches!(
        action.triggers[1].resolve_target,
        Some(TargetResolver::Field { .. })
    ));
}

#[test]
fn test_action_triggers_wasm_kind() {
    let spec = r#"
[automaton]
name = "Order"
states = ["Draft", "Confirmed"]
initial = "Draft"

[[action]]
name = "ConfirmOrder"
from = ["Draft"]
to = "Confirmed"

[[action.triggers]]
name = "charge_payment"
kind = "wasm"
module = "stripe_charge"
on_success = "ChargeSucceeded"
on_failure = "ChargeFailed"
principal = "payment-service"
"#;
    let automaton = parse_automaton(spec).expect("wasm trigger should parse");
    let trigger = &automaton.actions[0].triggers[0];
    assert_eq!(trigger.kind, TriggerKind::Wasm);
    assert_eq!(trigger.module.as_deref(), Some("stripe_charge"));
    assert_eq!(trigger.on_success.as_deref(), Some("ChargeSucceeded"));
    assert_eq!(trigger.on_failure.as_deref(), Some("ChargeFailed"));
}

#[test]
fn test_action_triggers_webhook_kind() {
    let spec = r#"
[automaton]
name = "Order"
states = ["Draft", "Confirmed"]
initial = "Draft"

[[action]]
name = "ConfirmOrder"
from = ["Draft"]
to = "Confirmed"

[[action.triggers]]
name = "notify_slack"
kind = "webhook"
url = "https://hooks.slack.com/services/xxx"
method = "POST"
on_success = "NotificationSent"
principal = "notification-service"

[action.triggers.headers]
"Content-Type" = "application/json"
"#;
    let automaton = parse_automaton(spec).expect("webhook trigger should parse");
    let trigger = &automaton.actions[0].triggers[0];
    assert_eq!(trigger.kind, TriggerKind::Webhook);
    assert_eq!(
        trigger.url.as_deref(),
        Some("https://hooks.slack.com/services/xxx")
    );
    assert_eq!(trigger.method.as_deref(), Some("POST"));
    assert_eq!(
        trigger.headers.get("Content-Type").map(String::as_str),
        Some("application/json")
    );
}

#[test]
fn test_action_triggers_to_state_filter() {
    let spec = r#"
[automaton]
name = "File"
states = ["Created", "Ready", "Failed"]
initial = "Created"

[[action]]
name = "Finish"
from = ["Created"]
to = "Ready"

[[action.triggers]]
name = "only_on_ready"
kind = "entity"
principal = "file-service"
to_state = "Ready"
target_entity = "AuditLog"
target_action = "Record"

[action.triggers.resolve_target]
type = "create"
"#;
    let automaton = parse_automaton(spec).expect("to_state trigger should parse");
    let trigger = &automaton.actions[0].triggers[0];
    assert_eq!(trigger.to_state.as_deref(), Some("Ready"));
}

#[test]
fn test_action_triggers_params_and_params_from() {
    let spec = r#"
[automaton]
name = "Order"
states = ["Draft", "Confirmed"]
initial = "Draft"

[[action]]
name = "ConfirmOrder"
from = ["Draft"]
to = "Confirmed"

[[action.triggers]]
name = "authorize_payment"
kind = "entity"
principal = "payment-service"
target_entity = "Payment"
target_action = "Authorize"

[action.triggers.params]
requested_by = "system"

[action.triggers.params_from]
amount = "total_cents"
currency = "currency_code"

[action.triggers.resolve_target]
type = "field"
field = "payment_id"
"#;
    let automaton = parse_automaton(spec).expect("params trigger should parse");
    let trigger = &automaton.actions[0].triggers[0];
    assert_eq!(trigger.params_from.get("amount").map(String::as_str), Some("total_cents"));
    assert_eq!(
        trigger.params_from.get("currency").map(String::as_str),
        Some("currency_code")
    );
    assert_eq!(
        trigger.params.get("requested_by").and_then(|v| v.as_str()),
        Some("system")
    );
}

#[test]
fn test_agent_trigger_section_does_not_overwrite_previous_action() {
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
name = "kickoff_tests"
on_action = "MarkReady"
agent_role = "tester"
agent_goal = "Verify the project"
"#;

    let automaton = parse_automaton(spec).expect("agent trigger should parse cleanly");
    let action_names: Vec<&str> = automaton
        .actions
        .iter()
        .map(|action| action.name.as_str())
        .collect();
    assert_eq!(action_names, vec!["MarkReady"]);
    assert_eq!(automaton.agent_triggers.len(), 1);
    assert_eq!(automaton.agent_triggers[0].name, "kickoff_tests");
}
