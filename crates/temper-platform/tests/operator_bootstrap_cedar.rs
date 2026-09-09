//! ARN-389 / ADR-0172: operator bootstrap seeds `manage_policies`.
//!
//! Proves a virgin store's verified operator can manage Cedar, that
//! unverified / non-operator principals stay denied, that re-bootstrap is
//! idempotent, and that OS-app Cedar still loads.

use std::collections::{BTreeMap, HashMap};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use temper_authz::{AuthzDecision, SecurityContext};
use temper_platform::bootstrap::{bootstrap_agent_specs, bootstrap_operator_credential};
use temper_platform::install_os_app;
use temper_platform::recovery::recover_cedar_policies;
use temper_platform::router::build_platform_router;
use temper_platform::state::PlatformState;
use temper_runtime::tenant::TenantId;
use temper_server::StorageStack;
use temper_server::identity::hash_token;
use temper_server::request_context::AgentContext;
use temper_server::state::PendingDecision;
use temper_store_turso::TursoEventStore;
use tower::ServiceExt;

mod common;
use common::http::body_json;

const OPERATOR_KEY: &str = "tmpr_operator-bootstrap-cedar";
const DEVELOPER_KEY: &str = "tmpr_developer-bootstrap-cedar";
const POLICY_ID: &str = "operator-bootstrap-manage-policies";

fn virgin_state(tenant: &str) -> PlatformState {
    let state = PlatformState::new(None);
    bootstrap_agent_specs(&state, tenant, false, &BTreeMap::new());
    state
}

async fn virgin_state_with_store(tenant: &str) -> (PlatformState, tempfile::TempDir) {
    let temp = tempfile::tempdir().expect("temp policy db");
    let db_url = format!("file:{}", temp.path().join("policy.db").display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create turso store");
    let mut state = virgin_state(tenant);
    state
        .server
        .set_storage_stack(StorageStack::from_turso(store));
    (state, temp)
}

fn manage_policies_attrs(tenant: &str) -> HashMap<String, serde_json::Value> {
    let mut attrs = HashMap::new();
    attrs.insert("id".to_string(), json!(tenant));
    attrs.insert("tenant".to_string(), json!(tenant));
    attrs
}

fn authorize_manage_policies(
    state: &PlatformState,
    tenant: &str,
    ctx: &SecurityContext,
) -> AuthzDecision {
    state.server.authz.authorize_for_tenant(
        tenant,
        ctx,
        "manage_policies",
        "PolicySet",
        &manage_policies_attrs(tenant),
    )
}

fn unverified_operator_context() -> SecurityContext {
    let mut ctx = SecurityContext::from_resolved_identity("operator", "operator", None);
    ctx.principal
        .attributes
        .insert("agentTypeVerified".to_string(), json!(false));
    ctx.context_attrs
        .insert("agentTypeVerified".to_string(), json!(false));
    ctx
}

async fn issue_developer_credential(state: &PlatformState, tenant: &str, plaintext: &str) {
    let tenant_id = TenantId::new(tenant);
    let ctx = AgentContext::system();
    let key_hash = hash_token(plaintext);
    let _ = state
        .server
        .dispatch_tenant_action(
            &tenant_id,
            "AgentType",
            "developer-type",
            "Define",
            json!({
                "name": "developer",
                "system_prompt": "test",
                "tool_set": "local",
                "model": "none",
                "max_turns": "0",
                "adapter_config": "{}",
                "default_budget_cents": "0"
            }),
            &ctx,
        )
        .await;
    let _ = state
        .server
        .dispatch_tenant_action(
            &tenant_id,
            "AgentCredential",
            &key_hash,
            "Issue",
            json!({
                "agent_type_id": "developer-type",
                "agent_instance_id": "developer",
                "key_hash": key_hash,
                "key_prefix": plaintext.chars().take(8).collect::<String>(),
                "description": "non-operator test credential",
                "created_by": "test",
                "expires_at": ""
            }),
            &ctx,
        )
        .await;
}

fn statement_occurrences(haystack: &str, tenant: &str) -> usize {
    let needle = format!(r#"resource == PolicySet::"{tenant}""#);
    haystack.matches(&needle).count()
}

#[tokio::test]
async fn virgin_store_verified_operator_can_manage_policies() {
    let tenant = "acme";
    let state = virgin_state(tenant);
    bootstrap_operator_credential(&state, OPERATOR_KEY, tenant).await;

    let operator = SecurityContext::from_resolved_identity("operator", "operator", None);
    let decision = authorize_manage_policies(&state, tenant, &operator);
    assert!(
        decision.is_allowed(),
        "verified operator must be allowed manage_policies on a virgin store, got {decision:?}"
    );

    let text = state
        .server
        .authz
        .get_tenant_policy_text(tenant)
        .expect("seeded policy text");
    assert!(text.contains(r#"Action::"manage_policies""#));
    assert!(text.contains(r#"PolicySet::"acme""#));
}

#[tokio::test]
async fn unverified_or_non_operator_cannot_manage_policies() {
    let tenant = "acme";
    let state = virgin_state(tenant);
    bootstrap_operator_credential(&state, OPERATOR_KEY, tenant).await;

    let unverified = authorize_manage_policies(&state, tenant, &unverified_operator_context());
    assert!(
        !unverified.is_allowed(),
        "unverified operator must stay denied, got {unverified:?}"
    );

    let developer = SecurityContext::from_resolved_identity("developer", "developer", None);
    let denied = authorize_manage_policies(&state, tenant, &developer);
    assert!(
        !denied.is_allowed(),
        "non-operator must stay denied, got {denied:?}"
    );
}

#[tokio::test]
async fn rebootstrap_is_idempotent_and_persists_one_granular_row() {
    let tenant = "acme";
    let (state, _temp) = virgin_state_with_store(tenant).await;

    bootstrap_operator_credential(&state, OPERATOR_KEY, tenant).await;
    bootstrap_operator_credential(&state, OPERATOR_KEY, tenant).await;

    let text = state
        .server
        .authz
        .get_tenant_policy_text(tenant)
        .expect("seeded policy text");
    assert_eq!(
        statement_occurrences(&text, tenant),
        1,
        "re-bootstrap must not duplicate the live permit: {text}"
    );

    let rows = state
        .server
        .policy_store()
        .expect("policy store")
        .load_policies_for_tenant(tenant)
        .await
        .expect("load policies");
    let matches: Vec<_> = rows
        .iter()
        .filter(|row| row.policy_id == POLICY_ID)
        .collect();
    assert_eq!(
        matches.len(),
        1,
        "re-bootstrap must not create duplicate granular rows: {rows:?}"
    );
    assert!(matches[0].enabled);
    assert!(
        matches[0]
            .cedar_text
            .contains(r#"Action::"manage_policies""#)
    );
}

#[tokio::test]
async fn existing_app_cedar_still_loads_after_operator_bootstrap() {
    let tenant = "app-tenant";
    let (state, _temp) = virgin_state_with_store(tenant).await;

    bootstrap_operator_credential(&state, OPERATOR_KEY, tenant).await;
    install_os_app(&state, tenant, "project-management")
        .await
        .expect("install project-management");

    let operator = SecurityContext::from_resolved_identity("operator", "operator", None);
    assert!(
        authorize_manage_policies(&state, tenant, &operator).is_allowed(),
        "operator manage_policies must survive app install"
    );

    let any = SecurityContext::from_resolved_identity("operator", "operator", None);
    let mut issue_attrs = HashMap::new();
    issue_attrs.insert("id".to_string(), json!("issue-1"));
    let create =
        state
            .server
            .authz
            .authorize_for_tenant(tenant, &any, "create", "Issue", &issue_attrs);
    assert!(
        create.is_allowed(),
        "project-management Issue.create must remain allowed after operator seed: {create:?}"
    );

    let rows = state
        .server
        .policy_store()
        .expect("policy store")
        .load_policies_for_tenant(tenant)
        .await
        .expect("load policies");
    assert!(
        rows.iter().any(|row| row.policy_id == POLICY_ID),
        "operator bootstrap row must remain: {rows:?}"
    );
    assert!(
        rows.iter().any(|row| {
            row.policy_id == "project-management-issue"
                && row.cedar_text.contains("resource is Issue")
        }),
        "app Issue cedar row must persist: {rows:?}"
    );
}

#[tokio::test]
async fn recovered_granular_row_still_allows_verified_operator() {
    let tenant = "acme";
    let temp = tempfile::tempdir().expect("temp recovery db");
    let db_url = format!("file:{}", temp.path().join("policy.db").display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create turso store");

    let mut seeded = virgin_state(tenant);
    seeded
        .server
        .set_storage_stack(StorageStack::from_turso(store.clone()));
    bootstrap_operator_credential(&seeded, OPERATOR_KEY, tenant).await;

    let recovered = PlatformState::new(None);
    recover_cedar_policies(&recovered, &store).await;

    let operator = SecurityContext::from_resolved_identity("operator", "operator", None);
    let decision = authorize_manage_policies(&recovered, tenant, &operator);
    assert!(
        decision.is_allowed(),
        "recovered operator permit must allow manage_policies, got {decision:?}"
    );
}

#[tokio::test]
async fn http_policy_api_allows_operator_and_denies_developer() {
    let tenant = "acme";
    let (state, _temp) = virgin_state_with_store(tenant).await;
    bootstrap_operator_credential(&state, OPERATOR_KEY, tenant).await;
    issue_developer_credential(&state, tenant, DEVELOPER_KEY).await;

    let app = build_platform_router(state);

    let allowed = app
        .clone()
        .oneshot(
            Request::get(format!("/api/tenants/{tenant}/policies"))
                .header("Authorization", format!("Bearer {OPERATOR_KEY}"))
                .header("X-Tenant-Id", tenant)
                .body(Body::empty())
                .expect("operator GET policies"),
        )
        .await
        .expect("operator GET policies should run");
    assert_eq!(
        allowed.status(),
        StatusCode::OK,
        "verified operator GET /policies must succeed"
    );
    let body = body_json(allowed).await;
    let text = body["policy_text"].as_str().unwrap_or_default();
    assert!(text.contains(r#"Action::"manage_policies""#), "{body}");

    let listed = app
        .clone()
        .oneshot(
            Request::get(format!("/api/tenants/{tenant}/policies/list"))
                .header("Authorization", format!("Bearer {OPERATOR_KEY}"))
                .header("X-Tenant-Id", tenant)
                .body(Body::empty())
                .expect("operator list policies"),
        )
        .await
        .expect("operator list should run");
    assert_eq!(listed.status(), StatusCode::OK);
    let listed_body = body_json(listed).await;
    let empty_policies = Vec::new();
    let ids: Vec<&str> = listed_body["policies"]
        .as_array()
        .unwrap_or(&empty_policies)
        .iter()
        .filter_map(|row| row["policy_id"].as_str())
        .collect();
    assert!(
        ids.contains(&POLICY_ID),
        "list must include the bootstrap row: {listed_body}"
    );

    let denied = app
        .oneshot(
            Request::get(format!("/api/tenants/{tenant}/policies"))
                .header("Authorization", format!("Bearer {DEVELOPER_KEY}"))
                .header("X-Tenant-Id", tenant)
                .body(Body::empty())
                .expect("developer GET policies"),
        )
        .await
        .expect("developer GET policies should run");
    assert_eq!(
        denied.status(),
        StatusCode::FORBIDDEN,
        "non-operator GET /policies must be 403"
    );
}

#[tokio::test]
async fn denied_developer_cannot_self_approve_operator_can() {
    let tenant = "acme";
    let (state, _temp) = virgin_state_with_store(tenant).await;
    bootstrap_operator_credential(&state, OPERATOR_KEY, tenant).await;
    issue_developer_credential(&state, tenant, DEVELOPER_KEY).await;

    let developer_permit = format!(
        r#"permit(
  principal is Agent,
  action == Action::"manage_policies",
  resource == PolicySet::"{tenant}"
) when {{
  principal.agent_type == "developer" &&
  principal.agentTypeVerified == true
}};"#
    );
    let pending = PendingDecision::from_denial(
        tenant,
        "developer",
        "Assign",
        "Issue",
        "issue-1",
        json!({"id": "issue-1"}),
        "test denial",
        None,
    );
    let decision_id = pending.id.clone();
    state
        .server
        .persist_pending_decision(&pending)
        .await
        .expect("persist pending decision");

    let app = build_platform_router(state);
    let created = app
        .clone()
        .oneshot(
            Request::post(format!("/api/tenants/{tenant}/policies/create"))
                .header("Authorization", format!("Bearer {OPERATOR_KEY}"))
                .header("X-Tenant-Id", tenant)
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "policy_id": "developer-manage-policies",
                        "cedar_text": developer_permit,
                    })
                    .to_string(),
                ))
                .expect("create developer policy"),
        )
        .await
        .expect("create developer policy should run");
    assert_eq!(
        created.status(),
        StatusCode::CREATED,
        "operator must be able to add Cedar"
    );

    let approve_body = json!({
        "scope": {
            "principal": "this_agent",
            "action": "this_action",
            "resource": "this_resource",
            "duration": "always"
        }
    })
    .to_string();

    let self_approve = app
        .clone()
        .oneshot(
            Request::post(format!(
                "/api/tenants/{tenant}/decisions/{decision_id}/approve"
            ))
            .header("Authorization", format!("Bearer {DEVELOPER_KEY}"))
            .header("X-Tenant-Id", tenant)
            .header("content-type", "application/json")
            .body(Body::from(approve_body.clone()))
            .expect("developer self-approve"),
        )
        .await
        .expect("developer self-approve should run");
    assert_eq!(
        self_approve.status(),
        StatusCode::FORBIDDEN,
        "developer with manage_policies must still get 403 on their own decision"
    );

    let operator_approve = app
        .oneshot(
            Request::post(format!(
                "/api/tenants/{tenant}/decisions/{decision_id}/approve"
            ))
            .header("Authorization", format!("Bearer {OPERATOR_KEY}"))
            .header("X-Tenant-Id", tenant)
            .header("content-type", "application/json")
            .body(Body::from(approve_body))
            .expect("operator approve"),
        )
        .await
        .expect("operator approve should run");
    assert_eq!(
        operator_approve.status(),
        StatusCode::OK,
        "verified operator must approve another agent's decision"
    );
    let approved = body_json(operator_approve).await;
    assert_eq!(approved["status"], "approved");
    assert!(
        approved["generated_policy"]
            .as_str()
            .unwrap_or_default()
            .contains(r#"Action::"Assign""#),
        "{approved}"
    );
}
