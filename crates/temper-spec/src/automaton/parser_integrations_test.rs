use super::super::*;
use super::ORDER_IOA;

#[test]
fn test_integration_section_parsed() {
    let toml = r#"
[automaton]
name = "Order"
states = ["Draft", "Submitted"]
initial = "Draft"

[[action]]
name = "SubmitOrder"
from = ["Draft"]
to = "Submitted"

[[integration]]
name = "notify_fulfillment"
trigger = "SubmitOrder"
type = "webhook"

[[integration]]
name = "charge_payment"
trigger = "ConfirmOrder"
type = "webhook"
"#;
    let automaton = parse_automaton(toml).expect("should parse");
    assert_eq!(automaton.integrations.len(), 2);
    assert_eq!(automaton.integrations[0].name, "notify_fulfillment");
    assert_eq!(automaton.integrations[0].trigger, "SubmitOrder");
    assert_eq!(automaton.integrations[0].integration_type, "webhook");
    assert_eq!(automaton.integrations[1].name, "charge_payment");
}

#[test]
fn test_integration_default_type() {
    let toml = r#"
[automaton]
name = "Order"
states = ["Draft", "Submitted"]
initial = "Draft"

[[integration]]
name = "notify"
trigger = "SubmitOrder"
"#;
    let automaton = parse_automaton(toml).expect("should parse");
    assert_eq!(automaton.integrations.len(), 1);
    assert_eq!(automaton.integrations[0].integration_type, "webhook");
}

#[test]
fn legacy_webhook_integration_is_rejected_until_delivery_is_durable() {
    let toml = r#"
[automaton]
name = "Order"
states = ["Draft"]
initial = "Draft"

[[integration]]
name = "notify_fulfillment"
trigger = "SubmitOrder"
type = "webhook"
"#;

    let err = parse_automaton(toml)
        .expect_err("a legacy outbound webhook without durable runtime support must be rejected");
    assert!(
        err.to_string()
            .contains("outbound IOA webhooks are unsupported until durable delivery is available"),
        "expected the durable-delivery validation error, got: {err}"
    );
}

#[test]
fn defaulted_legacy_webhook_integration_is_also_rejected() {
    let toml = r#"
[automaton]
name = "Order"
states = ["Draft"]
initial = "Draft"

[[integration]]
name = "notify_fulfillment"
trigger = "SubmitOrder"
"#;

    let err = parse_automaton(toml).expect_err(
        "an omitted legacy integration type defaults to webhook and must be rejected",
    );
    assert!(
        err.to_string()
            .contains("outbound IOA webhooks are unsupported until durable delivery is available"),
        "expected the durable-delivery validation error, got: {err}"
    );
}

#[test]
fn test_no_integrations_defaults_empty() {
    let automaton = parse_automaton(ORDER_IOA).expect("should parse");
    assert!(automaton.integrations.is_empty());
}

#[test]
fn test_trigger_effect_parsed() {
    let toml = r#"
[automaton]
name = "Order"
states = ["Submitted", "ChargePending", "Confirmed", "PaymentFailed"]
initial = "Submitted"

[[action]]
name = "ChargePayment"
from = ["Submitted"]
to = "ChargePending"
effect = "trigger charge_payment"

[[integration]]
name = "charge_payment"
trigger = "charge_payment"
type = "webhook"

[[action]]
name = "ChargeSucceeded"
kind = "input"
from = ["ChargePending"]
to = "Confirmed"

[[action]]
name = "ChargeFailed"
kind = "input"
from = ["ChargePending"]
to = "PaymentFailed"
"#;
    let automaton = parse_automaton(toml).expect("should parse");
    let charge = automaton
        .actions
        .iter()
        .find(|action| action.name == "ChargePayment")
        .unwrap();
    assert_eq!(charge.effect.len(), 1);
    match &charge.effect[0] {
        Effect::Trigger { name } => assert_eq!(name, "charge_payment"),
        other => panic!("expected Trigger effect, got: {other:?}"),
    }
}

#[test]
fn test_wasm_integration_parsed() {
    let toml = r#"
[automaton]
name = "Order"
states = ["Submitted", "ChargePending", "Confirmed", "PaymentFailed"]
initial = "Submitted"

[[action]]
name = "ChargePayment"
from = ["Submitted"]
to = "ChargePending"
effect = "trigger charge_payment"

[[action]]
name = "ChargeSucceeded"
kind = "input"
from = ["ChargePending"]
to = "Confirmed"

[[action]]
name = "ChargeFailed"
kind = "input"
from = ["ChargePending"]
to = "PaymentFailed"

[[integration]]
name = "charge_payment"
trigger = "charge_payment"
type = "wasm"
module = "stripe_charge"
on_success = "ChargeSucceeded"
on_failure = "ChargeFailed"
"#;
    let automaton = parse_automaton(toml).expect("should parse");
    assert_eq!(automaton.integrations.len(), 1);
    let integration = &automaton.integrations[0];
    assert_eq!(integration.name, "charge_payment");
    assert_eq!(integration.integration_type, "wasm");
    assert_eq!(integration.module.as_deref(), Some("stripe_charge"));
    assert_eq!(integration.on_success.as_deref(), Some("ChargeSucceeded"));
    assert_eq!(integration.on_failure.as_deref(), Some("ChargeFailed"));
}

#[test]
fn test_wasm_integration_missing_module_rejected() {
    let toml = r#"
[automaton]
name = "Order"
states = ["Submitted", "ChargePending"]
initial = "Submitted"

[[action]]
name = "ChargePayment"
from = ["Submitted"]
to = "ChargePending"

[[integration]]
name = "charge_payment"
trigger = "charge_payment"
type = "wasm"
"#;
    let result = parse_automaton(toml);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("missing 'module'"), "got: {err}");
}

#[test]
fn test_wasm_integration_unknown_callback_rejected() {
    let toml = r#"
[automaton]
name = "Order"
states = ["Submitted", "ChargePending", "Confirmed"]
initial = "Submitted"

[[action]]
name = "ChargePayment"
from = ["Submitted"]
to = "ChargePending"

[[integration]]
name = "charge_payment"
trigger = "charge_payment"
type = "wasm"
module = "stripe_charge"
on_success = "NonExistentAction"
"#;
    let result = parse_automaton(toml);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("NonExistentAction"),
        "should mention missing action, got: {err}"
    );
}

#[test]
fn test_integration_config_captures_unknown_keys() {
    let toml = r#"
[automaton]
name = "Weather"
states = ["Idle", "Fetching", "Ready", "Failed"]
initial = "Idle"

[[action]]
name = "FetchWeather"
from = ["Idle"]
to = "Fetching"
effect = "trigger fetch_weather"

[[action]]
name = "FetchSucceeded"
kind = "input"
from = ["Fetching"]
to = "Ready"

[[action]]
name = "FetchFailed"
kind = "input"
from = ["Fetching"]
to = "Failed"

[[integration]]
name = "fetch_weather"
trigger = "fetch_weather"
type = "wasm"
module = "http_fetch"
on_success = "FetchSucceeded"
on_failure = "FetchFailed"
url = "https://api.open-meteo.com/v1/forecast"
method = "GET"
"#;
    let automaton = parse_automaton(toml).expect("should parse");
    assert_eq!(automaton.integrations.len(), 1);
    let integration = &automaton.integrations[0];
    assert_eq!(integration.name, "fetch_weather");
    assert_eq!(integration.integration_type, "wasm");
    assert_eq!(integration.module.as_deref(), Some("http_fetch"));
    assert_eq!(
        integration.config.get("url").map(String::as_str),
        Some("https://api.open-meteo.com/v1/forecast")
    );
    assert_eq!(
        integration.config.get("method").map(String::as_str),
        Some("GET")
    );
    assert!(!integration.config.contains_key("name"));
    assert!(!integration.config.contains_key("trigger"));
    assert!(!integration.config.contains_key("type"));
    assert!(!integration.config.contains_key("module"));
}

#[test]
fn test_integration_llm_flag_parsed() {
    let toml = r#"
[automaton]
name = "Chat"
states = ["Preparing", "Calling", "Applying", "Done", "Failed"]
initial = "Preparing"

[[action]]
name = "CallProvider"
from = ["Preparing"]
to = "Calling"
effect = "trigger call_provider"

[[action]]
name = "ProviderResponseReady"
kind = "input"
from = ["Calling"]
to = "Applying"

[[action]]
name = "ApplyProviderResponse"
from = ["Applying"]
to = "Done"
effect = "trigger apply_provider_response"

[[action]]
name = "Fail"
kind = "input"
from = ["Calling", "Applying"]
to = "Failed"

[[integration]]
name = "call_provider"
trigger = "call_provider"
type = "wasm"
module = "provider_caller"
llm = true
on_failure = "Fail"

[[integration]]
name = "apply_provider_response"
trigger = "apply_provider_response"
type = "wasm"
module = "provider_response_applier"
on_failure = "Fail"
"#;
    let automaton = parse_automaton(toml).expect("should parse");
    let call_provider = automaton
        .integrations
        .iter()
        .find(|i| i.name == "call_provider")
        .expect("call_provider integration present");
    assert!(call_provider.llm, "llm = true should set Integration::llm");
    assert!(
        !call_provider.config.contains_key("llm"),
        "llm flag must not leak into config bag"
    );

    let apply = automaton
        .integrations
        .iter()
        .find(|i| i.name == "apply_provider_response")
        .expect("apply_provider_response integration present");
    assert!(
        !apply.llm,
        "integration without llm key should default to false"
    );
}
