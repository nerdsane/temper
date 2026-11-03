use std::collections::BTreeMap;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use temper_authz::{AuthenticatedRequestContext, SecurityContext};
use temper_runtime::tenant::TenantId;
use temper_server::platform_store::InstalledAppRecord;
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
                    label: None,
                    attributes: Default::default(),
                },
                context_attrs: Default::default(),
                correlation_id: "platform-admin-side-channel-test".to_string(),
                cause: None,
                credential: None,
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

#[tokio::test]
async fn requested_app_guide_denial_has_an_agent_decision() {
    let state = PlatformState::new(None);
    let mut decisions = state.server.pending_decision_tx.subscribe();
    let mut request = typed_request(
        Method::GET,
        "/os-apps/project-management",
        serde_json::Value::Null,
        "tenant-a",
    );
    request
        .extensions_mut()
        .insert(agent_context("tenant-a").with_session_id(Some("catalog-session".to_string())));
    let response = tenant_api_router()
        .with_state(state)
        .oneshot(request)
        .await
        .expect("request");
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
        .await
        .expect("body");
    let body: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    let id = body["decision_id"]
        .as_str()
        .expect("requested guide denial must offer a decision");
    assert!(
        body["error"]["message"].as_str().unwrap().contains(id),
        "existing MCP clients extract the decision ID from the error message"
    );
    let decision = decisions.try_recv().expect("decision broadcast");
    assert_eq!(decision.id, id);
    assert_eq!(decision.tenant, "tenant-a");
    assert_eq!(decision.agent_id, "agent-1");
    assert_eq!(decision.action, "read_app_catalog");
    assert_eq!(decision.resource_type, "AppCatalogEntry");
    assert_eq!(decision.resource_id, "project-management");
    assert_eq!(decision.session_id.as_deref(), Some("catalog-session"));
    assert!(body.get("guide").is_none());
}

#[tokio::test]
async fn requested_install_denial_offers_decision_only_in_credential_tenant() {
    let state = PlatformState::new(None);
    let mut decisions = state.server.pending_decision_tx.subscribe();
    let app = tenant_api_router().with_state(state);
    for target in ["other-tenant", "tenant-a"] {
        let mut request = typed_request(
            Method::POST,
            "/genesis/apps/install",
            serde_json::json!({
                "tenant": target,
                "app_ref": "owner/app@0123456789abcdef",
                "registry_url": "https://example.invalid",
                "registry_tenant": "default",
            }),
            "tenant-a",
        );
        request
            .extensions_mut()
            .insert(agent_context("tenant-a").with_session_id(Some("install-session".to_string())));
        let response = app.clone().oneshot(request).await.expect("request");
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
            .await
            .expect("body");
        let body: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        if target == "other-tenant" {
            assert!(body.get("decision_id").is_none());
            assert!(decisions.try_recv().is_err());
        } else {
            let id = body["decision_id"]
                .as_str()
                .expect("same-tenant install must offer a decision");
            let decision = decisions.try_recv().expect("decision broadcast");
            assert_eq!(decision.id, id);
            assert_eq!(decision.tenant, "tenant-a");
            assert_eq!(decision.action, "install_app");
            assert_eq!(decision.resource_type, "App");
            assert_eq!(decision.resource_id, "owner/app@0123456789abcdef");
            assert_eq!(decision.agent_id, "agent-1");
            assert_eq!(decision.session_id.as_deref(), Some("install-session"));
        }
    }
}

fn installed_app(tenant: &str, app_name: &str, version_hash: &str) -> InstalledAppRecord {
    InstalledAppRecord {
        tenant: tenant.to_string(),
        app_name: app_name.to_string(),
        source_kind: "genesis".to_string(),
        app_ref: format!("owner/{app_name}@{version_hash}"),
        version_hash: version_hash.to_string(),
        pinned_version_hash: version_hash.to_string(),
        current_version_hash: version_hash.to_string(),
        follow_policy: "pinned".to_string(),
        status: "installed".to_string(),
        ..Default::default()
    }
}

#[tokio::test]
async fn app_installs_lists_only_the_credential_tenants_apps() {
    let db_path = format!("/tmp/temper-test-app-installs-{}.db", uuid::Uuid::new_v4());
    let turso = temper_store_turso::TursoEventStore::new(&format!("file:{db_path}"), None)
        .await
        .expect("turso store");
    let mut state = PlatformState::new(None);
    state
        .server
        .set_storage_stack(temper_server::StorageStack::from_turso(turso));
    let platform_store = state
        .server
        .storage_stack
        .as_ref()
        .and_then(|stack| stack.platform.clone())
        .expect("platform store");
    // Recorded out of name order: the route sorts by app name.
    for record in [
        installed_app("tenant-a", "beta-app", "fd62a5dc"),
        installed_app("tenant-a", "alpha-app", "0a1b2c3d"),
        installed_app("tenant-b", "alpha-app", "99999999"),
    ] {
        platform_store
            .record_installed_app_metadata(&record)
            .await
            .expect("record installed app");
    }
    state
        .server
        .authz
        .reload_tenant_policies(
            "tenant-a",
            r#"
permit(
  principal == Agent::"agent-1",
  action == Action::"read_app_installs",
  resource == AppInstall::"tenant-a"
);
"#,
        )
        .expect("install read policy should parse");
    let app = tenant_api_router().with_state(state);

    let response = app
        .clone()
        .oneshot(typed_request(
            Method::GET,
            "/genesis/apps/installs",
            serde_json::Value::Null,
            "tenant-a",
        ))
        .await
        .expect("request should run");
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
        .await
        .expect("body");
    let body: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    let rows = body["value"].as_array().expect("value array");
    let summary: Vec<(&str, &str, &str, &str)> = rows
        .iter()
        .map(|row| {
            (
                row["app_name"].as_str().expect("app_name"),
                row["version_hash"].as_str().expect("version_hash"),
                row["pinned_version_hash"]
                    .as_str()
                    .expect("pinned_version_hash"),
                row["current_version_hash"]
                    .as_str()
                    .expect("current_version_hash"),
            )
        })
        .collect();
    assert_eq!(
        summary,
        vec![
            ("alpha-app", "0a1b2c3d", "0a1b2c3d", "0a1b2c3d"),
            ("beta-app", "fd62a5dc", "fd62a5dc", "fd62a5dc"),
        ],
        "only tenant-a's apps, sorted by name, with the hashes each runs"
    );
    assert_eq!(rows[1]["app_ref"], "owner/beta-app@fd62a5dc");
    assert_eq!(rows[1]["source_kind"], "genesis");
    assert_eq!(rows[1]["follow_policy"], "pinned");
    assert_eq!(rows[1]["status"], "installed");
    assert!(rows[1]["installed_at"].is_string());

    let unauthenticated = app
        .clone()
        .oneshot(
            Request::get("/genesis/apps/installs")
                .body(Body::empty())
                .expect("request should build"),
        )
        .await
        .expect("request should run");
    assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);

    // tenant-b has no policy granting the read, so Cedar refuses it.
    let unpermitted = app
        .oneshot(typed_request(
            Method::GET,
            "/genesis/apps/installs",
            serde_json::Value::Null,
            "tenant-b",
        ))
        .await
        .expect("request should run");
    assert_eq!(unpermitted.status(), StatusCode::FORBIDDEN);

    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(format!("{db_path}-wal"));
    let _ = std::fs::remove_file(format!("{db_path}-shm"));
}

#[tokio::test]
async fn an_install_from_a_registry_other_than_the_configured_one_is_refused() {
    let state = PlatformState::new(None);
    state
        .server
        .authz
        .reload_tenant_policies(
            "tenant-a",
            r#"permit(principal, action == Action::"install_app", resource);"#,
        )
        .expect("install policy should parse");
    let app = tenant_api_router().with_state(state);
    // Whether or not this process has a registry configured, a registry_url
    // that is not it is refused before anything is fetched.
    let response = app
        .oneshot(typed_request(
            Method::POST,
            "/genesis/apps/install",
            serde_json::json!({
                "tenant": "tenant-a",
                "app_ref": "acme/notes@0123456789abcdef0123456789abcdef01234567",
                "registry_url": "http://127.0.0.1:9/not-the-configured-registry",
            }),
            "tenant-a",
        ))
        .await
        .expect("request should run");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
        .await
        .expect("body");
    let body: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|error| error.contains("TEMPER_GENESIS_REGISTRY_URL")),
        "{body}"
    );
}
