use super::super::*;
#[allow(unused_imports)]
use super::ORDER_IOA;
use crate::automaton::{ResolvedEffect, dispatch_effects};

// ADR-0046: `[[agent_trigger]]` retired along with the AgentTrigger struct.
// The equivalent behavior is now an `[[action.triggers]]` block with
// kind="entity" targeting the Agent entity. Auto-start-on-Assign lives
// as a self-trigger on the target agent entity's own spec (Sub-Decision 7).

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
states = ["Draft", "Confirmed", "Charged", "Failed"]
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

[[action]]
name = "ChargeSucceeded"
from = ["Confirmed"]
to = "Charged"

[[action]]
name = "ChargeFailed"
from = ["Confirmed"]
to = "Failed"
"#;
    let automaton = parse_automaton(spec).expect("wasm trigger should parse");
    let confirm = &automaton.actions[0];
    assert_eq!(confirm.name, "ConfirmOrder");
    let trigger = &confirm.triggers[0];
    assert_eq!(trigger.kind, TriggerKind::Wasm);
    assert_eq!(trigger.module.as_deref(), Some("stripe_charge"));
    assert_eq!(trigger.on_success.as_deref(), Some("ChargeSucceeded"));
    assert_eq!(trigger.on_failure.as_deref(), Some("ChargeFailed"));
}

#[test]
fn test_action_triggers_adapter_kind_synthesizes_integration() {
    let spec = r#"
[automaton]
name = "EvolutionRun"
states = ["Proposing", "Verifying", "Failed"]
initial = "Proposing"

[[action]]
name = "RecordDataset"
from = ["Proposing"]
to = "Proposing"

[[action.triggers]]
name = "propose_mutation"
kind = "adapter"
adapter = "claude_code"
on_success = "RecordMutation"
on_failure = "Fail"

[action.triggers.config]
command = "/tmp/mock-claude"

[[action]]
name = "RecordMutation"
from = ["Proposing"]
to = "Verifying"

[[action]]
name = "Fail"
from = ["Proposing", "Verifying"]
to = "Failed"
"#;
    let automaton = parse_automaton(spec).expect("adapter trigger should parse");
    let trigger = &automaton.actions[0].triggers[0];
    assert_eq!(trigger.kind, TriggerKind::Adapter);
    assert_eq!(trigger.adapter.as_deref(), Some("claude_code"));

    let integration = automaton
        .integrations
        .iter()
        .find(|ig| ig.trigger == "__trigger__:RecordDataset:propose_mutation")
        .expect("adapter trigger should synthesize integration");
    assert_eq!(integration.integration_type, "adapter");
    assert_eq!(integration.on_success.as_deref(), Some("RecordMutation"));
    assert_eq!(integration.on_failure.as_deref(), Some("Fail"));
    assert_eq!(
        integration.config.get("adapter").map(String::as_str),
        Some("claude_code")
    );
    assert_eq!(
        integration.config.get("command").map(String::as_str),
        Some("/tmp/mock-claude")
    );

    assert_eq!(
        dispatch_effects(&automaton.actions[0]),
        [ResolvedEffect::Dispatch(
            "__trigger__:RecordDataset:propose_mutation".into()
        )],
        "source action should dispatch the synthesized record"
    );
}

#[test]
fn test_action_triggers_adapter_kind_requires_adapter_key() {
    let spec = r#"
[automaton]
name = "EvolutionRun"
states = ["Proposing", "Failed"]
initial = "Proposing"

[[action]]
name = "RecordDataset"
from = ["Proposing"]
to = "Proposing"

[[action.triggers]]
name = "propose_mutation"
kind = "adapter"
on_failure = "Fail"

[[action]]
name = "Fail"
from = ["Proposing"]
to = "Failed"
"#;
    let err = parse_automaton(spec).expect_err("adapter trigger without adapter must fail");
    assert!(
        err.to_string().contains("missing 'adapter'"),
        "expected adapter validation error, got: {err}"
    );
}

#[test]
fn test_action_triggers_webhook_kind() {
    let spec = r#"
[automaton]
name = "Order"
states = ["Draft", "Confirmed", "Notified"]
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

[[action]]
name = "NotificationSent"
from = ["Confirmed"]
to = "Notified"
"#;
    let automaton = parse_automaton(spec).expect("webhook trigger should parse");
    let confirm = &automaton.actions[0];
    let trigger = &confirm.triggers[0];
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
fn test_action_triggers_invalid_nested_toml_fails_loud() {
    let spec = r#"
[automaton]
name = "File"
states = ["Created", "Ready"]
initial = "Created"

[[action]]
name = "StreamUpdated"
from = ["Created"]
to = "Ready"

[[action.triggers]]
name = "broken_trigger"
kind = "entity"
target_entity = "FileVersion"
target_action = "Create"

[action.triggers.resolve_target
type = "create"
"#;

    let err = parse_automaton(spec).expect_err("invalid nested trigger TOML must fail");
    assert!(
        err.to_string().contains("action.triggers"),
        "expected action.triggers parse failure, got: {err}"
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
    assert_eq!(
        trigger.params_from.get("amount").map(String::as_str),
        Some("total_cents")
    );
    assert_eq!(
        trigger.params_from.get("currency").map(String::as_str),
        Some("currency_code")
    );
    assert_eq!(
        trigger.params.get("requested_by").and_then(|v| v.as_str()),
        Some("system")
    );
}

// ─── ADR-0046: parse-time validator tests ──────────────────────────────────

#[test]
fn test_entity_trigger_requires_target_entity() {
    let spec = r#"
[automaton]
name = "X"
states = ["A"]
initial = "A"

[[action]]
name = "Go"
from = ["A"]

[[action.triggers]]
name = "bad"
kind = "entity"
target_action = "Do"

[action.triggers.resolve_target]
type = "same_id"
"#;
    let err = parse_automaton(spec).expect_err("missing target_entity must fail");
    let msg = err.to_string();
    assert!(msg.contains("target_entity"), "error = {msg}");
}

#[test]
fn test_entity_trigger_requires_target_action() {
    let spec = r#"
[automaton]
name = "X"
states = ["A"]
initial = "A"

[[action]]
name = "Go"
from = ["A"]

[[action.triggers]]
name = "bad"
kind = "entity"
target_entity = "Y"

[action.triggers.resolve_target]
type = "same_id"
"#;
    let err = parse_automaton(spec).expect_err("missing target_action must fail");
    assert!(err.to_string().contains("target_action"));
}

#[test]
fn test_entity_trigger_requires_resolve_target() {
    let spec = r#"
[automaton]
name = "X"
states = ["A"]
initial = "A"

[[action]]
name = "Go"
from = ["A"]

[[action.triggers]]
name = "bad"
kind = "entity"
target_entity = "Y"
target_action = "Do"
"#;
    let err = parse_automaton(spec).expect_err("missing resolve_target must fail");
    assert!(err.to_string().contains("resolve_target"));
}

#[test]
fn test_wasm_trigger_requires_module() {
    let spec = r#"
[automaton]
name = "X"
states = ["A"]
initial = "A"

[[action]]
name = "Go"
from = ["A"]

[[action.triggers]]
name = "bad"
kind = "wasm"
"#;
    let err = parse_automaton(spec).expect_err("missing module must fail");
    assert!(err.to_string().contains("module"));
}

#[test]
fn test_webhook_trigger_requires_url_and_method() {
    let spec_no_url = r#"
[automaton]
name = "X"
states = ["A"]
initial = "A"

[[action]]
name = "Go"
from = ["A"]

[[action.triggers]]
name = "bad"
kind = "webhook"
method = "POST"
"#;
    assert!(
        parse_automaton(spec_no_url)
            .expect_err("missing url must fail")
            .to_string()
            .contains("url")
    );

    let spec_no_method = r#"
[automaton]
name = "X"
states = ["A"]
initial = "A"

[[action]]
name = "Go"
from = ["A"]

[[action.triggers]]
name = "bad"
kind = "webhook"
url = "https://example.com/hook"
"#;
    assert!(
        parse_automaton(spec_no_method)
            .expect_err("missing method must fail")
            .to_string()
            .contains("method")
    );
}

#[test]
fn test_to_state_must_be_declared_state() {
    let spec = r#"
[automaton]
name = "X"
states = ["A", "B"]
initial = "A"

[[action]]
name = "Go"
from = ["A"]
to = "B"

[[action.triggers]]
name = "bad"
kind = "entity"
to_state = "NotAState"
target_entity = "Y"
target_action = "Do"

[action.triggers.resolve_target]
type = "same_id"
"#;
    let err = parse_automaton(spec).expect_err("bad to_state must fail");
    assert!(err.to_string().contains("NotAState"));
}

#[test]
fn test_on_success_must_be_declared_action() {
    let spec = r#"
[automaton]
name = "X"
states = ["A"]
initial = "A"

[[action]]
name = "Go"
from = ["A"]

[[action.triggers]]
name = "bad"
kind = "wasm"
module = "m"
on_success = "NoSuchAction"
"#;
    let err = parse_automaton(spec).expect_err("unknown on_success action must fail");
    assert!(err.to_string().contains("NoSuchAction"));
}

#[test]
fn test_params_and_params_from_collision_rejected() {
    let spec = r#"
[automaton]
name = "X"
states = ["A"]
initial = "A"

[[action]]
name = "Go"
from = ["A"]

[[action.triggers]]
name = "bad"
kind = "entity"
target_entity = "Y"
target_action = "Do"

[action.triggers.params]
amount = "fixed"

[action.triggers.params_from]
amount = "total"

[action.triggers.resolve_target]
type = "same_id"
"#;
    let err = parse_automaton(spec).expect_err("params/params_from collision must fail");
    let msg = err.to_string();
    assert!(msg.contains("amount"), "error = {msg}");
}

#[test]
fn test_trigger_name_uniqueness_per_action() {
    let spec = r#"
[automaton]
name = "X"
states = ["A"]
initial = "A"

[[action]]
name = "Go"
from = ["A"]

[[action.triggers]]
name = "dup"
kind = "entity"
target_entity = "Y"
target_action = "Do"

[action.triggers.resolve_target]
type = "same_id"

[[action.triggers]]
name = "dup"
kind = "entity"
target_entity = "Z"
target_action = "Go"

[action.triggers.resolve_target]
type = "same_id"
"#;
    let err = parse_automaton(spec).expect_err("duplicate trigger name must fail");
    assert!(err.to_string().contains("dup"));
}

#[test]
fn test_empty_trigger_name_rejected() {
    let spec = r#"
[automaton]
name = "X"
states = ["A"]
initial = "A"

[[action]]
name = "Go"
from = ["A"]

[[action.triggers]]
name = ""
kind = "entity"
target_entity = "Y"
target_action = "Do"

[action.triggers.resolve_target]
type = "same_id"
"#;
    let err = parse_automaton(spec).expect_err("empty trigger name must fail");
    assert!(err.to_string().contains("empty"));
}

// ─── ADR-0046: wasm/webhook expansion into integrations ─────────────────

#[test]
fn wasm_trigger_expands_into_integration_and_effect() {
    let spec = r#"
[automaton]
name = "Order"
states = ["Draft", "Confirmed", "Charged", "Failed"]
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

[[action]]
name = "ChargeSucceeded"
from = ["Confirmed"]
to = "Charged"

[[action]]
name = "ChargeFailed"
from = ["Confirmed"]
to = "Failed"
"#;
    let automaton = parse_automaton(spec).expect("wasm expansion should parse");

    // A synthesized integration should be present.
    let ig = automaton
        .integrations
        .iter()
        .find(|i| i.trigger == "__trigger__:ConfirmOrder:charge_payment")
        .expect("synthesized integration");
    assert_eq!(ig.integration_type, "wasm");
    assert_eq!(ig.module.as_deref(), Some("stripe_charge"));
    assert_eq!(ig.on_success.as_deref(), Some("ChargeSucceeded"));
    assert_eq!(ig.on_failure.as_deref(), Some("ChargeFailed"));

    // The source action dispatches the synthesized record by name.
    let confirm = automaton
        .actions
        .iter()
        .find(|a| a.name == "ConfirmOrder")
        .unwrap();
    assert_eq!(
        dispatch_effects(confirm),
        [ResolvedEffect::Dispatch(
            "__trigger__:ConfirmOrder:charge_payment".into()
        )]
    );
}

#[test]
fn each_action_dispatches_its_own_copy_of_a_shared_trigger() {
    let spec = r#"
[automaton]
name = "Session"
states = ["Ready", "Executing", "Waiting"]
initial = "Ready"

[[action]]
name = "Prepare"
from = ["Ready"]
to = "Waiting"

[[action.triggers]]
name = "prepare_context"
kind = "wasm"
module = "context_preparer"
on_failure = "Fail"

[action.triggers.config]
temper_api_url = "{secret:temper_api_url}"

[[action]]
name = "Continue"
from = ["Executing"]
to = "Waiting"

[[action.triggers]]
name = "prepare_context"
kind = "wasm"
module = "context_preparer"
on_failure = "Fail"
config = { temper_api_url = "{secret:temper_api_url}" }

[[action]]
name = "Fail"
from = ["Ready", "Executing", "Waiting"]
to = "Ready"
"#;

    let automaton = parse_automaton(spec).expect("a trigger declared on two actions should parse");
    let continue_action = automaton
        .actions
        .iter()
        .find(|a| a.name == "Continue")
        .expect("Continue action");
    assert_eq!(
        dispatch_effects(continue_action),
        [ResolvedEffect::Dispatch(
            "__trigger__:Continue:prepare_context".into()
        )]
    );
    assert_eq!(automaton.integrations.len(), 2);
}

#[test]
fn trigger_and_emit_effects_point_at_action_triggers() {
    for effect in ["trigger('prepare_context')", "emit('Prepared')"] {
        let spec = format!(
            r#"
[automaton]
name = "Session"
states = ["A"]
initial = "A"

[[action]]
name = "Prepare"
from = ["A"]
effect = ["{effect}"]
"#
        );
        let err = parse_automaton(&spec).expect_err("trigger/emit effects are gone");
        assert!(
            err.to_string().contains("[[action.triggers]]"),
            "{effect}: {err}"
        );
    }
}

#[test]
fn integration_blocks_are_rejected() {
    let spec = r#"
[automaton]
name = "Session"
states = ["A"]
initial = "A"

[[integration]]
name = "prepare"
trigger = "prepare"
type = "wasm"
module = "context_preparer"
"#;
    let err = parse_automaton(spec).expect_err("[[integration]] is gone");
    assert!(err.to_string().contains("[[action.triggers]]"), "{err}");
}

#[test]
fn hook_triggers_dispatch_platform_hooks() {
    let spec = r#"
[automaton]
name = "GovernanceDecision"
states = ["Pending", "Approved"]
initial = "Pending"

[[action]]
name = "Approve"
from = ["Pending"]
to = "Approved"

[[action.triggers]]
name = "GenerateCedarPolicy"
kind = "hook"
hook = "GenerateCedarPolicy"

[[action.triggers]]
name = "DispatchCallback"
kind = "hook"
hook = "DispatchCallback"
"#;

    let automaton = parse_automaton(spec).expect("hook triggers should parse");
    let approve = automaton
        .actions
        .iter()
        .find(|a| a.name == "Approve")
        .unwrap();
    assert_eq!(
        dispatch_effects(approve),
        [
            ResolvedEffect::Dispatch("GenerateCedarPolicy".into()),
            ResolvedEffect::Dispatch("DispatchCallback".into()),
        ]
    );
    assert!(automaton.integrations.is_empty());

    let missing = spec.replace("hook = \"DispatchCallback\"\n", "");
    let err = parse_automaton(&missing).expect_err("a hook trigger names its hook");
    assert!(err.to_string().contains("missing 'hook'"), "{err}");
}

#[test]
fn webhook_trigger_expands_with_url_and_method_in_config() {
    let spec = r#"
[automaton]
name = "Order"
states = ["Draft", "Confirmed", "Notified"]
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

[action.triggers.headers]
"Content-Type" = "application/json"

[[action]]
name = "NotificationSent"
from = ["Confirmed"]
to = "Notified"
"#;
    let automaton = parse_automaton(spec).expect("webhook expansion should parse");
    let ig = automaton
        .integrations
        .iter()
        .find(|i| i.trigger == "__trigger__:ConfirmOrder:notify_slack")
        .expect("synthesized integration");
    assert_eq!(ig.integration_type, "webhook");
    assert_eq!(
        ig.config.get("url").map(String::as_str),
        Some("https://hooks.slack.com/services/xxx")
    );
    assert_eq!(ig.config.get("method").map(String::as_str), Some("POST"));
    assert_eq!(
        ig.config.get("header.Content-Type").map(String::as_str),
        Some("application/json")
    );
    assert_eq!(ig.on_success.as_deref(), Some("NotificationSent"));
}

#[test]
fn entity_kind_trigger_does_not_synthesize_integration() {
    // Regression: only Wasm/Webhook expand. Entity-kind triggers go
    // through the reaction dispatcher and must NOT appear as integrations.
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
name = "creates_version"
kind = "entity"
principal = "file-service"
target_entity = "FileVersion"
target_action = "Create"

[action.triggers.resolve_target]
type = "same_id"
"#;
    let automaton = parse_automaton(spec).expect("entity-kind should parse");
    assert!(
        automaton.integrations.is_empty(),
        "entity-kind triggers must not synthesize integrations"
    );
    // The source action dispatches nothing by name.
    assert!(dispatch_effects(&automaton.actions[0]).is_empty());
}

// test_agent_trigger_section_does_not_overwrite_previous_action removed —
// ADR-0046 deleted the [[agent_trigger]] section. The equivalent
// invariant ([[action.triggers]] body doesn't leak into action fields) is
// covered by the action-triggers parser tests above.
