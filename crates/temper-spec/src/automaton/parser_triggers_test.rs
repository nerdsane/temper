use super::super::*;
#[allow(unused_imports)]
use super::ORDER_IOA;
use crate::automaton::{ResolvedEffect, dispatch_effects};
use crate::predicate::{Arg, Literal};

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
id_field = "last_version_id"
kind = "create_if_missing"
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
kind = "create"
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
id_field = "last_version_id"
kind = "create_if_missing"

[[action.triggers]]
name = "supersede_previous"
kind = "entity"
principal = "file-service"
target_entity = "FileVersion"
target_action = "Supersede"

[action.triggers.resolve_target]
kind = "field"
id_field = "last_version_id"
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
    assert_eq!(trigger.on_success, ["ChargeSucceeded"]);
    assert_eq!(trigger.on_failure.as_deref(), Some("ChargeFailed"));
}

/// A trigger and a state timer may say, in `says`, words for people reading the app, such as the answer a branch
/// gives or what a timer does. It is optional, and a spec without it parses as before.
#[test]
fn test_trigger_and_state_timeout_says_parse() {
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
says = "Charge the card"
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
from = ["Confirmed", "Draft"]
to = "Failed"

[[state_timeout]]
state = "Confirmed"
after_seconds = 60
on_timeout = "ChargeFailed"
says = "give up on the charge"

[[state_timeout]]
state = "Draft"
after_seconds = 3600
on_timeout = "ChargeFailed"
"#;
    let automaton =
        parse_automaton(spec).expect("words on a trigger and a state timer should parse");
    // Read through the serialized spec, so this test compiles whatever the structs hold.
    let spec = serde_json::to_value(&automaton).expect("the spec serializes");
    assert_eq!(spec["action"][0]["triggers"][0]["says"], "Charge the card");
    assert_eq!(spec["state_timeout"][0]["says"], "give up on the charge");
    assert!(
        spec["state_timeout"][1].get("says").is_none(),
        "a timer that says nothing has no says"
    );
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
    assert_eq!(integration.on_success, ["RecordMutation"]);
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
fn webhook_kind_trigger_is_refused_at_load() {
    // ADR-0182: nothing sends a kind = "webhook" trigger, so it used to
    // install and never make its request. It is refused with what to write.
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
    let err = parse_automaton(spec).expect_err("a webhook trigger must not install");
    let message = err.to_string();
    assert!(message.contains("notify_slack"), "{message}");
    assert!(message.contains("no dispatcher sends"), "{message}");
    assert!(message.contains("http_fetch"), "{message}");
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
fn test_action_triggers_status_filter_comes_from_the_guard() {
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
target_entity = "AuditLog"
target_action = "Record"
guard = "status == 'Ready'"

[action.triggers.resolve_target]
kind = "create"
"#;
    let automaton = parse_automaton(spec).expect("status-guarded trigger should parse");
    let trigger = &automaton.actions[0].triggers[0];
    assert_eq!(trigger.status_filter(), Some(vec!["Ready".to_string()]));
}

#[test]
fn test_action_triggers_args_hold_literals_and_fields() {
    let spec = r#"
[automaton]
name = "Order"
states = ["Draft", "Confirmed"]
initial = "Draft"

[[state]]
name = "total_cents"
type = "counter"
initial = 0

[[state]]
name = "currency_code"
type = "string"
initial = ""

[[state]]
name = "payment_id"
type = "string"
initial = ""

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

[action.triggers.args]
requested_by = "'system'"
amount = "total_cents"
currency = "currency_code"

[action.triggers.resolve_target]
kind = "field"
id_field = "payment_id"
"#;
    let automaton = parse_automaton(spec).expect("args trigger should parse");
    let trigger = &automaton.actions[0].triggers[0];
    assert_eq!(trigger.args["amount"], Arg::Var("total_cents".into()));
    assert_eq!(trigger.args["currency"], Arg::Var("currency_code".into()));
    assert_eq!(
        trigger.args["requested_by"],
        Arg::Lit(Literal::Str("system".into()))
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
kind = "same_id"
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
kind = "same_id"
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
target_entity = "Y"
target_action = "Do"
guard = "status == 'NotAState'"

[action.triggers.resolve_target]
kind = "same_id"
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

[action.triggers.args]
amount = "total"

[action.triggers.resolve_target]
kind = "same_id"
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
kind = "same_id"

[[action.triggers]]
name = "dup"
kind = "entity"
target_entity = "Z"
target_action = "Go"

[action.triggers.resolve_target]
kind = "same_id"
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
kind = "same_id"
"#;
    let err = parse_automaton(spec).expect_err("empty trigger name must fail");
    assert!(err.to_string().contains("empty"));
}

// ─── ADR-0046: wasm/adapter expansion into integrations ─────────────────

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
    assert_eq!(ig.on_success, ["ChargeSucceeded"]);
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
fn entity_kind_trigger_does_not_synthesize_integration() {
    // Regression: only Wasm/Adapter expand. Entity-kind triggers go
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
kind = "same_id"
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

/// The umbrella app: a forecast module computes the chance of rain, and the
/// spec decides the move from it.
const UMBRELLA: &str = r#"
[automaton]
name = "Day"
states = ["Asked", "Wet", "Dry"]
initial = "Asked"

[[action]]
name = "Ask"
from = ["Asked"]

[[action.triggers]]
name = "forecast"
kind = "wasm"
module = "forecast"
on_success = ["TakeUmbrella", "LeaveUmbrella"]

[[action]]
name = "TakeUmbrella"
from = ["Asked"]
to = "Wet"
params = [{ name = "rain", type = "int" }]
guard = "params.rain >= 50"

[[action]]
name = "LeaveUmbrella"
from = ["Asked"]
to = "Dry"
params = [{ name = "rain", type = "int" }]
"#;

#[test]
fn a_wasm_trigger_lists_its_answers_and_their_guards_read_its_values() {
    let automaton = parse_automaton(UMBRELLA).expect("a list of answers should parse");
    let trigger = &automaton.actions[0].triggers[0];
    assert_eq!(trigger.on_success, ["TakeUmbrella", "LeaveUmbrella"]);
    assert_eq!(automaton.integrations[0].on_success, trigger.on_success);
    assert_eq!(automaton.actions[1].guard.params_read(), ["rain"]);
}

#[test]
fn an_answer_listed_twice_is_refused() {
    let spec = UMBRELLA.replace(
        r#"["TakeUmbrella", "LeaveUmbrella"]"#,
        r#"["TakeUmbrella", "TakeUmbrella"]"#,
    );
    let error = parse_automaton(&spec).unwrap_err().to_string();
    assert!(
        error.contains("on_success names 'TakeUmbrella' twice"),
        "{error}"
    );
}

#[test]
fn a_guard_reads_only_the_params_its_action_declares() {
    let spec = UMBRELLA.replace("params.rain >= 50", "params.wind >= 50");
    let error = parse_automaton(&spec).unwrap_err().to_string();
    assert!(
        error.contains("`params.wind` is not a parameter of 'TakeUmbrella'"),
        "{error}"
    );
}

#[test]
fn only_an_action_guard_reads_a_param() {
    let spec = format!(
        "{UMBRELLA}\n[[invariant]]\nname = \"wet_means_rain\"\nassert = \"status == 'Wet' => params.rain >= 50\"\n"
    );
    let error = parse_automaton(&spec).unwrap_err().to_string();
    assert!(
        error.contains("`params.rain` is a parameter of a move; only an action's guard reads one"),
        "{error}"
    );
}
