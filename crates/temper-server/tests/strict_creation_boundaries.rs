mod common;
use serde_json::json;
use temper_runtime::tenant::TenantId;
const SPEC: &str = r#"
[automaton]
name = "Order"
states = ["Draft", "Submitted"]
initial = "Draft"
strict_action_params = true
[[action]]
name = "SubmitOrder"
kind = "input"
from = ["Draft"]
to = "Submitted"
params = ["Notes"]
"#;
#[tokio::test]
async fn round_three_absent_initializer_reports_refusal_without_creating_child() {
    let parent = SPEC.replace(
        "params = [\"Notes\"]",
        "params = [\"Notes\"]\neffect = [{type=\"spawn\",entity_type=\"Customer\",entity_id_source=\"Notes\"}]",
    );
    let child = SPEC.replace("name = \"Order\"", "name = \"Customer\"");
    let (state, _) = common::build_single_tenant_state(
        0,
        "missing-strict-initializer",
        "default",
        &[("Order", &parent), ("Customer", &child)],
    );
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .unwrap();
    let tenant = TenantId::default();
    state
        .get_or_create_tenant_entity(&tenant, "Order", "parent", json!({}))
        .await
        .unwrap();
    let mut events = state.entity_observe_tx.subscribe();
    state
        .dispatch_tenant_action(
            &tenant,
            "Order",
            "parent",
            "SubmitOrder",
            json!({"Notes":"child"}),
            &Default::default(),
        )
        .await
        .unwrap();
    let refusal = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        for _ in 0..32 {
            let event = events.recv().await.unwrap();
            if event.entity_id == "parent" && event.event_name == "integration_callback_rejected" {
                assert_eq!(event.data["action"], "spawn");
                return;
            }
        }
        panic!("spawn refusal was not reported");
    })
    .await;
    assert_eq!(
        state.active_actor_count(),
        1,
        "created a child with no declared initializer"
    );
    refusal.expect("spawn refusal was not observable");
}

#[tokio::test]
async fn invalid_creation_fields_are_rejected_before_and_after_cache_population() {
    let (state, _) =
        common::build_single_tenant_state(0, "strict-cached-create", "default", &[("Order", SPEC)]);
    let tenant = TenantId::default();
    assert!(
        state
            .get_or_create_tenant_entity(&tenant, "Order", "valid", json!({"Notes":"forbidden"}))
            .await
            .is_err()
    );
    assert_eq!(state.active_actor_count(), 0);
    state
        .get_or_create_tenant_entity(&tenant, "Order", "valid", json!({}))
        .await
        .unwrap();
    assert!(
        state
            .get_or_create_tenant_entity(&tenant, "Order", "valid", json!({"Notes":"forbidden"}))
            .await
            .is_err()
    );
    let valid = state
        .get_or_create_tenant_entity(&tenant, "Order", "valid", json!({}))
        .await
        .unwrap();
    assert!(valid.state.fields.get("Notes").is_none());
    assert_eq!(state.active_actor_count(), 1);
}
