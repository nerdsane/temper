//! An approved decision installs one permit built from the decision's values.
//!
//! The stored action, resource type and resource id are rendered as typed
//! Cedar values, so the permit has exactly the scope the reviewer chose. A
//! decision whose values are not valid Cedar is refused and changes nothing.

use std::collections::{BTreeMap, HashMap};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use temper_authz::SecurityContext;
use temper_authz::statements::statement_keys;
use temper_platform::bootstrap::{
    bootstrap_agent_specs, bootstrap_operator_credential, bootstrap_system_tenant,
};
use temper_platform::router::build_platform_router;
use temper_platform::state::PlatformState;
use temper_server::StorageStack;
use temper_server::state::PendingDecision;
use temper_store_turso::TursoEventStore;
use tower::ServiceExt;

const TENANT: &str = "acme";
const OPERATOR_KEY: &str = "tmpr_operator-decision-text";

async fn operator_state() -> (PlatformState, tempfile::TempDir) {
    let temp = tempfile::tempdir().expect("temp policy db");
    let db_url = format!("file:{}", temp.path().join("policy.db").display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create turso store");
    let mut state = PlatformState::new(None);
    bootstrap_system_tenant(&state, &BTreeMap::new());
    bootstrap_agent_specs(&state, TENANT, false, &BTreeMap::new());
    state
        .server
        .set_storage_stack(StorageStack::from_turso(store));
    bootstrap_operator_credential(&state, OPERATOR_KEY, TENANT)
        .await
        .expect("operator credential bootstrap should succeed");
    (state, temp)
}

async fn record_decision(
    state: &PlatformState,
    action: &str,
    resource_type: &str,
) -> PendingDecision {
    let pending = PendingDecision::from_denial(
        TENANT,
        "helper",
        action,
        resource_type,
        "o-1",
        json!({"id": "o-1"}),
        "test denial",
        None,
    );
    state
        .server
        .persist_pending_decision(&pending)
        .await
        .expect("persist pending decision");
    pending
}

async fn approve_any_of_type(state: &PlatformState, decision_id: &str) -> StatusCode {
    let request = Request::builder()
        .method("POST")
        .uri(format!(
            "/api/tenants/{TENANT}/decisions/{decision_id}/approve"
        ))
        .header("Authorization", format!("Bearer {OPERATOR_KEY}"))
        .header("X-Tenant-Id", TENANT)
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"scope": {
                "principal": "this_agent",
                "action": "this_action",
                "resource": "any_of_type",
                "duration": "always"
            }})
            .to_string(),
        ))
        .expect("request builds");
    build_platform_router(state.clone())
        .oneshot(request)
        .await
        .expect("approve runs")
        .status()
}

fn helper_may(state: &PlatformState, action: &str, resource_type: &str) -> bool {
    let helper = SecurityContext::from_resolved_identity("helper", "worker", None);
    let mut attrs = HashMap::new();
    attrs.insert("id".to_string(), json!("r-1"));
    state
        .server
        .authz
        .authorize_for_tenant(TENANT, &helper, action, resource_type, &attrs)
        .is_allowed()
}

fn live_text(state: &PlatformState) -> String {
    state
        .server
        .authz
        .get_tenant_policy_text(TENANT)
        .unwrap_or_default()
}

#[tokio::test]
async fn an_approved_action_name_with_cedar_syntax_installs_one_permit_of_that_scope() {
    let (state, _temp) = operator_state().await;
    let action = "read\",\n  resource\n);\npermit(\n  principal == Agent::\"helper\",\n  action == Action::\"read";
    let pending = record_decision(&state, action, "Order").await;

    assert_eq!(
        approve_any_of_type(&state, &pending.id).await,
        StatusCode::OK
    );

    let rows = state
        .server
        .policy_store()
        .expect("durable policy store")
        .load_policies_for_tenant(TENANT)
        .await
        .expect("load stored policies");
    let row = rows
        .iter()
        .find(|row| row.policy_id == format!("decision:{}", pending.id))
        .expect("the decision's permit is stored");
    assert_eq!(
        statement_keys(&row.cedar_text)
            .expect("stored text parses")
            .len(),
        1,
        "the decision installs exactly one statement"
    );
    assert!(helper_may(&state, action, "Order"));
    assert!(!helper_may(&state, "read", "Invoice"));
}

#[tokio::test]
async fn a_decision_whose_type_is_not_a_cedar_type_name_is_refused() {
    let (state, _temp) = operator_state().await;
    let pending = record_decision(
        &state,
        "read",
        "Order,\n  resource\n);\npermit(principal, action",
    )
    .await;
    let before = live_text(&state);

    assert_eq!(
        approve_any_of_type(&state, &pending.id).await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(live_text(&state), before, "nothing is activated");
    assert!(!helper_may(&state, "read", "Invoice"));
}
