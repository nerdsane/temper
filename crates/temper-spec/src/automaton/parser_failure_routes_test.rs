use super::super::{FailureCategory, parse_automaton};

const PREFIX: &str = r#"
[automaton]
name = "Payment"
states = ["Created", "Charging", "RetryScheduled", "AwaitingApproval", "Reconciling"]
initial = "Created"

[[action]]
name = "Charge"
from = ["Created"]
to = "Charging"

[[action.triggers]]
name = "charge_card"
kind = "wasm"
module = "payments"
"#;

const CALLBACKS: &str = r#"
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

[[action]]
name = "Reconcile"
from = ["Charging"]
to = "Reconciling"
params = [{ name = "failure", type = "failure_v1" }]
"#;

fn parse(routes: &str) -> Result<super::super::Automaton, super::super::AutomatonParseError> {
    parse_automaton(&format!("{PREFIX}{routes}{CALLBACKS}"))
}

#[test]
fn typed_failure_routes_parse_with_closed_categories() {
    let automaton = parse(
        r#"
[[action.triggers.failure_routes]]
category = "transient"
action = "ScheduleRetry"

[[action.triggers.failure_routes]]
category = "authorization"
to_state = "AwaitingApproval"

[[action.triggers.failure_routes]]
category = "ambiguous"
action = "Reconcile"
"#,
    )
    .expect("valid typed failure routes");

    let routes = &automaton.actions[0].triggers[0].failure_routes;
    assert_eq!(routes.len(), 3);
    assert_eq!(routes[0].category, FailureCategory::Transient);
    assert_eq!(routes[1].category, FailureCategory::Authorization);
    assert_eq!(routes[2].category, FailureCategory::Ambiguous);
    let integration = automaton
        .integrations
        .iter()
        .find(|integration| integration.name == "__trigger__:Charge:charge_card")
        .expect("synthesized integration");
    assert_eq!(integration.failure_routes.len(), 3);
    assert_eq!(
        integration.failure_routes[1].callback_action,
        "AwaitApproval"
    );
}

#[test]
fn duplicate_category_fails_closed() {
    let error = parse(
        r#"
[[action.triggers.failure_routes]]
category = "transient"
action = "ScheduleRetry"

[[action.triggers.failure_routes]]
category = "transient"
action = "Reconcile"
"#,
    )
    .expect_err("duplicate category must fail");
    assert!(error.to_string().contains("more than once"));
}

#[test]
fn legacy_and_typed_failure_callbacks_cannot_mix() {
    let spec = format!(
        "{PREFIX}on_failure = \"ScheduleRetry\"\n[[action.triggers.failure_routes]]\ncategory = \"transient\"\naction = \"ScheduleRetry\"\n{CALLBACKS}"
    );
    let error = parse_automaton(&spec).expect_err("mixed compatibility paths must fail");
    assert!(error.to_string().contains("cannot mix"));
}

#[test]
fn route_requires_exactly_one_target_form() {
    for route in [
        r#"
[[action.triggers.failure_routes]]
category = "transient"
"#,
        r#"
[[action.triggers.failure_routes]]
category = "transient"
action = "ScheduleRetry"
to_state = "RetryScheduled"
"#,
    ] {
        let error = parse(route).expect_err("zero or two target forms must fail");
        assert!(error.to_string().contains("exactly one"));
    }
}

#[test]
fn callback_requires_canonical_failure_parameter() {
    let invalid_callbacks = [
        "params = []",
        "params = [{ name = \"error\", type = \"failure_v1\" }]",
        "params = [{ name = \"failure\", type = \"string\" }]",
        "params = [{ name = \"failure\", type = \"failure_v1\" }, \"extra\"]",
    ];
    for params in invalid_callbacks {
        let callbacks = CALLBACKS.replacen(
            "params = [{ name = \"failure\", type = \"failure_v1\" }]",
            params,
            1,
        );
        let spec = format!(
            "{PREFIX}[[action.triggers.failure_routes]]\ncategory = \"transient\"\naction = \"ScheduleRetry\"\n{callbacks}"
        );
        let error = parse_automaton(&spec).expect_err("invalid callback ABI must fail");
        assert!(error.to_string().contains("must declare exactly params"));
    }
}

#[test]
fn direct_callback_must_be_enabled_from_committed_state() {
    let callbacks = CALLBACKS.replacen("from = [\"Charging\"]", "from = [\"Created\"]", 1);
    let spec = format!(
        "{PREFIX}[[action.triggers.failure_routes]]\ncategory = \"transient\"\naction = \"ScheduleRetry\"\n{callbacks}"
    );
    let error = parse_automaton(&spec).expect_err("disabled callback must fail");
    assert!(error.to_string().contains("not enabled"));
}

#[test]
fn state_shorthand_must_resolve_to_exactly_one_callback() {
    let ambiguous = format!(
        "{PREFIX}[[action.triggers.failure_routes]]\ncategory = \"transient\"\nto_state = \"RetryScheduled\"\n{CALLBACKS}\n[[action]]\nname = \"AlsoScheduleRetry\"\nfrom = [\"Charging\"]\nto = \"RetryScheduled\"\nparams = [{{ name = \"failure\", type = \"failure_v1\" }}]\n"
    );
    let error = parse_automaton(&ambiguous).expect_err("ambiguous shorthand must fail");
    assert!(error.to_string().contains("resolves to 2 callback actions"));

    let missing = parse(
        r#"
[[action.triggers.failure_routes]]
category = "transient"
to_state = "Charging"
"#,
    )
    .expect_err("missing shorthand callback must fail");
    assert!(
        missing
            .to_string()
            .contains("resolves to 0 callback actions")
    );
}

#[test]
fn route_cannot_replay_source_action() {
    let error = parse(
        r#"
[[action.triggers.failure_routes]]
category = "transient"
action = "Charge"
"#,
    )
    .expect_err("typed route must not replay source operation");
    assert!(error.to_string().contains("cannot replay source action"));
}

#[test]
fn resolved_routes_cannot_be_injected_through_top_level_integrations() {
    let spec = format!(
        r#"{PREFIX}{CALLBACKS}

[[integration]]
name = "forged"
trigger = "Charge"
type = "wasm"
module = "payments"

[[integration.failure_routes]]
category = "transient"
callback_action = "ScheduleRetry"
"#
    );

    let error = parse_automaton(&spec).expect_err("injected resolved metadata must be rejected");
    assert!(error.to_string().contains("integration.failure_routes"));
}
