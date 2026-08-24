use std::collections::BTreeMap;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use temper_authz::{AuthenticatedRequestContext, SecurityContext};
use temper_runtime::tenant::TenantId;
use tower::ServiceExt;

use super::tenant_api_router;
use crate::state::PlatformState;

fn agent_context(tenant: &str) -> AuthenticatedRequestContext {
    AuthenticatedRequestContext::new(
        TenantId::new(tenant),
        SecurityContext::from_resolved_identity("agent-1", "operator", None),
    )
}

fn typed_request(
    method: Method,
    uri: &str,
    body: serde_json::Value,
    tenant: &str,
) -> Request<Body> {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("request should build");
    request.extensions_mut().insert(agent_context(tenant));
    request
}

#[tokio::test]
async fn tenant_admin_routes_require_typed_authority() {
    let app = tenant_api_router().with_state(PlatformState::new(None));
    let response = app
        .oneshot(
            Request::post("/tenants")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"tenant_id":"victim"}"#))
                .expect("request should build"),
        )
        .await
        .expect("request should run");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn typed_admin_kind_does_not_bypass_platform_cedar() {
    let app = tenant_api_router().with_state(PlatformState::new(None));
    let mut request = Request::get("/tenants")
        .body(Body::empty())
        .expect("request should build");
    request
        .extensions_mut()
        .insert(AuthenticatedRequestContext::new(
            TenantId::new("default"),
            SecurityContext {
                principal: temper_authz::Principal {
                    id: "claimed-admin".to_string(),
                    kind: temper_authz::PrincipalKind::Admin,
                    role: None,
                    acting_for: None,
                    agent_type: None,
                    attributes: Default::default(),
                },
                context_attrs: Default::default(),
                correlation_id: "platform-admin-side-channel-test".to_string(),
            },
        ));
    let response = app.oneshot(request).await.expect("request should run");
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn credential_cannot_delete_or_manage_users_in_another_tenant() {
    let app = tenant_api_router().with_state(PlatformState::new(None));
    let delete = app
        .clone()
        .oneshot(typed_request(
            Method::DELETE,
            "/tenants/victim",
            serde_json::Value::Null,
            "attacker",
        ))
        .await
        .expect("request should run");
    assert_eq!(delete.status(), StatusCode::FORBIDDEN);

    let add_user = app
        .oneshot(typed_request(
            Method::POST,
            "/tenants/victim/users",
            serde_json::json!({"user_id": "attacker", "role": "owner"}),
            "attacker",
        ))
        .await
        .expect("request should run");
    assert_eq!(add_user.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn genesis_install_body_cannot_select_another_tenant() {
    let app = tenant_api_router().with_state(PlatformState::new(None));
    let response = app
        .oneshot(typed_request(
            Method::POST,
            "/genesis/apps/install",
            serde_json::json!({
                "tenant": "victim",
                "app_ref": "owner/app@0123456789abcdef",
                "registry_url": "https://example.invalid",
            }),
            "attacker",
        ))
        .await
        .expect("request should run");
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn local_bundle_install_body_cannot_select_another_tenant() {
    let app = tenant_api_router().with_state(PlatformState::new(None));
    let response = app
        .oneshot(typed_request(
            Method::POST,
            "/app-bundles/install",
            serde_json::json!({
                "tenant": "victim",
                "provenance": {"source_locator": "display-only", "lock_digest": ""},
                "manifest": {
                    "schema_version": 1,
                    "root_app": "sample",
                    "apps": [],
                    "bundle_digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                },
                "blobs": []
            }),
            "attacker",
        ))
        .await
        .expect("request should run");
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn app_catalog_authorization_is_resource_specific() {
    let state = PlatformState::new(None);
    state
        .server
        .authz
        .reload_tenant_policies(
            "tenant-a",
            r#"
permit(
  principal == Agent::"agent-1",
  action == Action::"read_app_catalog",
  resource == AppCatalog::"all"
);
"#,
        )
        .expect("catalog policy should parse");
    let app = tenant_api_router().with_state(state);

    let list = app
        .clone()
        .oneshot(typed_request(
            Method::GET,
            "/os-apps",
            serde_json::Value::Null,
            "tenant-a",
        ))
        .await
        .expect("request should run");
    assert_eq!(list.status(), StatusCode::OK);

    let guide = app
        .oneshot(typed_request(
            Method::GET,
            "/os-apps/project-management",
            serde_json::Value::Null,
            "tenant-a",
        ))
        .await
        .expect("request should run");
    assert_eq!(guide.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn control_plane_catalog_rejects_non_default_credentials() {
    let state = PlatformState::new(None);
    state
        .server
        .authz
        .reload_tenant_policies("attacker", "permit(principal, action, resource);")
        .expect("attacker policy should parse");
    let app = tenant_api_router().with_state(state);
    let response = app
        .oneshot(typed_request(
            Method::GET,
            "/tenants",
            serde_json::Value::Null,
            "attacker",
        ))
        .await
        .expect("request should run");
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[test]
fn exact_platform_resource_policy_does_not_cover_sibling_resource() {
    let state = PlatformState::new(None);
    state
        .server
        .authz
        .reload_tenant_policies(
            "tenant-a",
            r#"
permit(
  principal == Agent::"agent-1",
  action == Action::"install_app",
  resource == App::"owner/allowed@hash"
);
"#,
        )
        .expect("install policy should parse");
    let authenticated = agent_context("tenant-a");
    let authorize = |resource_id| {
        super::auth::require_resource_authorization(
            &state,
            &authenticated,
            super::auth::PlatformResourceAuthorization {
                action: "install_app",
                resource_type: "App",
                resource_id,
                attrs: BTreeMap::new(),
            },
        )
    };
    assert!(authorize("owner/allowed@hash").is_ok());
    assert!(authorize("owner/victim@hash").is_err());
}
