//! The live Cedar set of a tenant is what its stored, enabled entries reload to.
//!
//! Whole-set replacement and decision approval both change the live set; each
//! must leave the store so that a reload or restart activates the same set.

use std::collections::{BTreeMap, BTreeSet};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use temper_authz::statements::{StatementKey, statement_keys};
use temper_platform::bootstrap::{
    bootstrap_agent_specs, bootstrap_operator_credential, bootstrap_system_tenant,
};
use temper_platform::router::build_platform_router;
use temper_platform::state::PlatformState;
use temper_runtime::tenant::TenantId;
use temper_server::StorageStack;
use temper_server::request_context::AgentContext;
use temper_server::state::PendingDecision;
use temper_store_turso::TursoEventStore;
use tower::ServiceExt;

const TENANT: &str = "acme";
const OPERATOR_KEY: &str = "tmpr_operator-policy-in-step";

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

fn keys(text: &str) -> BTreeSet<StatementKey> {
    statement_keys(text).expect("Cedar text parses")
}

fn live_keys(state: &PlatformState) -> BTreeSet<StatementKey> {
    keys(
        &state
            .server
            .authz
            .get_tenant_policy_text(TENANT)
            .unwrap_or_default(),
    )
}

/// What a reload would activate: every enabled stored entry.
async fn stored_keys(state: &PlatformState) -> BTreeSet<StatementKey> {
    let rows = state
        .server
        .policy_store()
        .expect("durable policy store")
        .load_policies_for_tenant(TENANT)
        .await
        .expect("load stored policies");
    let enabled: Vec<String> = rows
        .into_iter()
        .filter(|row| row.enabled)
        .map(|row| row.cedar_text)
        .collect();
    keys(&enabled.join("\n"))
}

fn operator_request(method: &str, path: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(format!("/api/tenants/{TENANT}{path}"))
        .header("Authorization", format!("Bearer {OPERATOR_KEY}"))
        .header("X-Tenant-Id", TENANT)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("request builds")
}

#[tokio::test]
async fn replacing_the_whole_set_leaves_only_that_set_to_reload() {
    let (state, _temp) = operator_state().await;
    let app = build_platform_router(state.clone());
    let extra =
        r#"permit(principal == Agent::"helper", action == Action::"Assign", resource is Issue);"#;
    let created = app
        .clone()
        .oneshot(operator_request(
            "POST",
            "/policies/create",
            json!({"policy_id": "helper-assign", "cedar_text": extra}),
        ))
        .await
        .expect("create runs");
    assert_eq!(created.status(), StatusCode::CREATED);

    let live_before = state
        .server
        .authz
        .get_tenant_policy_text(TENANT)
        .unwrap_or_default();
    let without_extra: String = live_before.replace(extra, "");
    assert_ne!(without_extra, live_before, "the extra permit is live first");

    let replaced = app
        .oneshot(operator_request(
            "PUT",
            "/policies",
            json!({"policy_text": without_extra}),
        ))
        .await
        .expect("replace runs");
    assert_eq!(replaced.status(), StatusCode::OK);

    assert_eq!(live_keys(&state), keys(&without_extra));
    assert_eq!(
        stored_keys(&state).await,
        keys(&without_extra),
        "a reload must not bring back the removed permit"
    );
    let rows = state
        .server
        .policy_store()
        .expect("durable policy store")
        .load_policies_for_tenant(TENANT)
        .await
        .expect("load stored policies");
    let helper = rows
        .iter()
        .find(|row| row.policy_id == "helper-assign")
        .expect("the replaced entry stays listed");
    assert!(!helper.enabled, "the replaced entry is disabled");
}

#[tokio::test]
async fn an_approved_decision_is_live_only_as_stored() {
    let (state, _temp) = operator_state().await;
    let gd_id = "GD-policy-in-step";
    state
        .server
        .dispatch_tenant_action(
            &TenantId::new("temper-system"),
            "GovernanceDecision",
            gd_id,
            "CreateGovernanceDecision",
            json!({
                "tenant": TENANT,
                "agent_id": "helper",
                "action_name": "Assign",
                "resource_type": "Issue",
                "resource_id": "issue-1",
                "denial_reason": "test denial",
                "scope": "narrow",
            }),
            &AgentContext::system(),
        )
        .await
        .expect("create governance decision");
    let mut pending = PendingDecision::from_denial(
        TENANT,
        "helper",
        "Assign",
        "Issue",
        "issue-1",
        json!({"id": "issue-1"}),
        "test denial",
        None,
    );
    pending.governance_decision_id = Some(gd_id.to_string());
    let decision_id = pending.id.clone();
    state
        .server
        .persist_pending_decision(&pending)
        .await
        .expect("persist pending decision");

    // Any Issue, so the stored permit differs from a per-resource one.
    let approved = build_platform_router(state.clone())
        .oneshot(operator_request(
            "POST",
            &format!("/decisions/{decision_id}/approve"),
            json!({"scope": {
                "principal": "this_agent",
                "action": "this_action",
                "resource": "any_of_type",
                "duration": "always"
            }}),
        ))
        .await
        .expect("approve runs");
    assert_eq!(approved.status(), StatusCode::OK);

    let system = TenantId::new("temper-system");
    let mut resolved = false;
    for _ in 0..200 {
        let decision = state
            .server
            .get_tenant_entity_state(&system, "GovernanceDecision", gd_id)
            .await
            .expect("read governance decision");
        if decision.state.status == "Approved" {
            resolved = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(resolved, "the approval resolves its governance decision");

    // The resolution's hooks run right after the transition; give them time.
    for _ in 0..20 {
        assert_eq!(
            live_keys(&state),
            stored_keys(&state).await,
            "every live permit is a stored, listed one"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}
