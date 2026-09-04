#![cfg(feature = "observe")]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use temper_authz::{AuthenticatedRequestContext, Principal, PrincipalKind, SecurityContext};
use temper_runtime::tenant::TenantId;
use temper_runtime::ActorSystem;
use temper_server::registry::SpecRegistry;
use temper_server::storage::StorageStack;
use temper_server::{build_router, ServerState};
use temper_store_turso::TursoEventStore;
use tower::ServiceExt;

const TENANT: &str = "default";
const OPERATOR_POLICY: &str = r#"
permit(
  principal is Agent,
  action == Action::"manage_policies",
  resource == PolicySet::"default"
) when {
  principal.agent_type == "operator" &&
  principal.agentTypeVerified == true
};
"#;

async fn policy_state() -> (ServerState, TursoEventStore, tempfile::TempDir) {
    let temp = tempfile::tempdir().expect("temporary policy database");
    let db_url = format!("file:{}", temp.path().join("policy.db").display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create Turso policy store");
    let mut state =
        ServerState::from_registry(ActorSystem::new("policy-auth"), SpecRegistry::new());
    state.set_storage_stack(StorageStack::from_turso(store.clone()));
    state
        .authz
        .reload_tenant_policies(TENANT, OPERATOR_POLICY)
        .expect("install operator policy");
    (state, store, temp)
}

fn request_with_context(
    method: &str,
    uri: &str,
    body: Body,
    tenant: &str,
    security_context: SecurityContext,
) -> Request<Body> {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(body)
        .expect("policy request should build");
    request
        .extensions_mut()
        .insert(AuthenticatedRequestContext::new(
            TenantId::new(tenant),
            security_context,
        ));
    request
}

fn admin_context() -> SecurityContext {
    SecurityContext {
        principal: Principal {
            id: "claimed-admin".to_string(),
            kind: PrincipalKind::Admin,
            role: None,
            acting_for: None,
            agent_type: None,
            attributes: Default::default(),
        },
        context_attrs: Default::default(),
        correlation_id: "policy-admin-test".to_string(),
    }
}

#[tokio::test]
async fn policy_management_requires_tenant_bound_cedar_authority() {
    let (state, store, _temp) = policy_state().await;
    let app = build_router(state);
    let uri = "/api/tenants/default/policies/create";

    let forged_header = app
        .clone()
        .oneshot(
            Request::post(uri)
                .header("content-type", "application/json")
                .header("x-temper-principal-kind", "admin")
                .body(Body::from("{}"))
                .expect("forged-header request should build"),
        )
        .await
        .expect("forged-header request should run");
    assert_eq!(forged_header.status(), StatusCode::UNAUTHORIZED);

    let typed_admin = app
        .clone()
        .oneshot(request_with_context(
            "POST",
            uri,
            Body::from("{}"),
            TENANT,
            admin_context(),
        ))
        .await
        .expect("typed-admin request should run");
    assert_eq!(typed_admin.status(), StatusCode::FORBIDDEN);

    let wrong_tenant = app
        .clone()
        .oneshot(request_with_context(
            "POST",
            uri,
            Body::from("{}"),
            "other-tenant",
            SecurityContext::from_resolved_identity("operator", "operator", None),
        ))
        .await
        .expect("wrong-tenant request should run");
    assert_eq!(wrong_tenant.status(), StatusCode::UNAUTHORIZED);

    let allowed = app
        .oneshot(request_with_context(
            "POST",
            uri,
            Body::from(
                serde_json::json!({
                    "policy_id": "operator-baseline",
                    "cedar_text": OPERATOR_POLICY,
                    "created_by": "forged-actor"
                })
                .to_string(),
            ),
            TENANT,
            SecurityContext::from_resolved_identity("operator", "operator", None),
        ))
        .await
        .expect("verified-operator request should run");
    assert_eq!(allowed.status(), StatusCode::CREATED);

    let rows = store
        .load_policies_for_tenant(TENANT)
        .await
        .expect("load created policy");
    let row = rows
        .iter()
        .find(|row| row.policy_id == "operator-baseline")
        .expect("created policy should be durable");
    assert_eq!(row.created_by, "operator");
    assert_ne!(row.created_by, "forged-actor");
}

const EXTRA_OPERATOR_RULE: &str = r#"
permit(
  principal is Agent,
  action == Action::"manage_policies",
  resource == PolicySet::"default"
) when {
  principal.id == "operator"
};
"#;

/// ARN-462: add-rule must persist the new rule as its own row. Writing the
/// concatenated tenant text into `primary` is how live primary grew to 449k.
#[tokio::test]
async fn add_policy_rule_persists_own_row_without_rewriting_primary() {
    let (state, store, _temp) = policy_state().await;
    store
        .save_policy(TENANT, "primary", OPERATOR_POLICY, "system")
        .await
        .expect("seed primary");
    {
        let mut policies = state.tenant_policies.write().unwrap();
        policies.insert(TENANT.to_string(), OPERATOR_POLICY.to_string());
    }

    let app = build_router(state);
    let uri = "/api/tenants/default/policies/rules";
    let body = serde_json::json!({ "rule": EXTRA_OPERATOR_RULE }).to_string();
    let added = app
        .clone()
        .oneshot(request_with_context(
            "POST",
            uri,
            Body::from(body.clone()),
            TENANT,
            SecurityContext::from_resolved_identity("operator", "operator", None),
        ))
        .await
        .expect("add-rule should run");
    assert_eq!(added.status(), StatusCode::OK);

    let rows = store
        .load_policies_for_tenant(TENANT)
        .await
        .expect("load after add-rule");
    let primary = rows
        .iter()
        .find(|row| row.policy_id == "primary")
        .expect("primary must still exist");
    assert_eq!(
        primary.cedar_text, OPERATOR_POLICY,
        "add-rule must not dump the tenant concat into primary"
    );
    assert!(primary.enabled, "add-rule must not disable primary");
    let rule_rows: Vec<_> = rows
        .iter()
        .filter(|row| row.enabled && row.cedar_text.trim() == EXTRA_OPERATOR_RULE.trim())
        .collect();
    assert_eq!(
        rule_rows.len(),
        1,
        "new rule must be its own enabled row, not folded into primary: {rows:?}"
    );
    assert_ne!(rule_rows[0].policy_id, "primary");

    let added_again = app
        .oneshot(request_with_context(
            "POST",
            uri,
            Body::from(body),
            TENANT,
            SecurityContext::from_resolved_identity("operator", "operator", None),
        ))
        .await
        .expect("duplicate add-rule should run");
    assert_eq!(added_again.status(), StatusCode::OK);

    let rows = store
        .load_policies_for_tenant(TENANT)
        .await
        .expect("load after duplicate add-rule");
    let rule_rows: Vec<_> = rows
        .iter()
        .filter(|row| row.enabled && row.cedar_text.trim() == EXTRA_OPERATOR_RULE.trim())
        .collect();
    assert_eq!(
        rule_rows.len(),
        1,
        "identical enabled cedar_text must not insert a duplicate row"
    );
}
