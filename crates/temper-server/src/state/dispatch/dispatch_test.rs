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
        .reload_tenant_policies("test-tenant", "")
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
        .reload_tenant_policies(
            "test-tenant",
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

/// Every field of each event logged while it runs, written out as text.
#[derive(Clone, Default)]
struct Logged(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Logged {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        struct Text(String);
        impl tracing::field::Visit for Text {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.0.push_str(&format!(" {}={value:?}", field.name()));
            }
        }
        let mut text = Text(String::new());
        event.record(&mut text);
        self.0.lock().unwrap().push(text.0);
    }
}

/// A refused answer's log line names the values it carried and the reason,
/// but not the values: they are whatever an integration returned.
#[test]
fn a_refused_answer_is_logged_without_its_values() {
    use tracing_subscriber::prelude::*;

    let logged = Logged::default();
    let _guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(logged.clone()));
    let state = test_state();
    let tenant = TenantId::default();
    state.record_answer_refusal(
        WasmEntityRef {
            tenant: &tenant,
            entity_type: "Order",
            entity_id: "o1",
        },
        "RecordReply",
        "Action 'RecordReply' is not valid from state 'Done'",
        &serde_json::json!({ "reply_token": "value-only-the-record-holds", "count": 7351 }),
    );

    let lines = logged.0.lock().unwrap().clone();
    let line = lines
        .iter()
        .find(|line| line.contains("integration callback rejected"))
        .unwrap_or_else(|| panic!("no refusal line in {lines:?}"));
    assert!(
        line.contains("reply_token") && line.contains("count"),
        "{line}"
    );
    assert!(line.contains("is not valid from state 'Done'"), "{line}");
    assert!(!line.contains("value-only-the-record-holds"), "{line}");
    assert!(!line.contains("7351"), "{line}");

    let events = state
        .entity_observe_replay(tenant.as_str(), "Order", "o1", 0)
        .events;
    assert_eq!(
        events[0].data["params"]["reply_token"], "value-only-the-record-holds",
        "the record's own observe stream keeps the values"
    );
}
