use super::*;
use temper_runtime::ActorSystem;
use temper_spec::csdl::parse_csdl;

fn test_state() -> crate::state::ServerState {
    let csdl_xml = include_str!("../../../../../test-fixtures/specs/model.csdl.xml");
    let csdl = parse_csdl(csdl_xml).expect("CSDL should parse");
    crate::state::ServerState::new(
        ActorSystem::new("dispatch-wasm-authz-test"),
        csdl,
        csdl_xml.to_string(),
    )
}

#[test]
fn wasm_authz_gate_evaluates_cedar_when_policy_set_is_empty() {
    let state = test_state();
    state
        .authz
        .reload_policies("")
        .expect("empty policy set should parse");

    let gate = state.wasm_authz_gate();
    let decision = gate.authorize_http_call(
        "api.example.com",
        "GET",
        "https://api.example.com/v1/ping",
        &WasmAuthzContext::test_fixture(),
    );

    assert_eq!(
        decision,
        WasmAuthzDecision::Deny("no matching permit policy".to_string())
    );
}

#[test]
fn wasm_authz_gate_allows_when_cedar_policy_matches() {
    let state = test_state();
    state
        .authz
        .reload_policies(
            r#"
            permit(
                principal is Agent,
                action == Action::"http_call",
                resource is HttpEndpoint
            ) when {
                context.module == "stripe_charge"
            };
            "#,
        )
        .expect("policy should parse");

    let gate = state.wasm_authz_gate();
    let decision = gate.authorize_http_call(
        "api.stripe.com",
        "POST",
        "https://api.stripe.com/v1/charges",
        &WasmAuthzContext::test_fixture(),
    );

    assert_eq!(decision, WasmAuthzDecision::Allow);
}
