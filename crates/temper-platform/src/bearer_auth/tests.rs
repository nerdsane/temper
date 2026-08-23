use super::*;
use axum::Router;
use axum::body::Body;
use axum::extract::Extension;
use axum::http::Request as HttpRequest;
use axum::middleware;
use axum::routing::{get, post};
use std::collections::BTreeMap;
use temper_authz::AuthenticatedRequestContext;
use tower::ServiceExt;

async fn ok_handler() -> &'static str {
    "ok"
}

/// Reports where the request's session landed: the Cedar-visible
/// `context_attrs["sessionId"]` vs the telemetry-only request context.
async fn session_probe(Extension(context): Extension<AuthenticatedRequestContext>) -> String {
    format!(
        "cedar={:?} telemetry={:?} principal={}",
        context
            .security_context()
            .context_attrs
            .get("sessionId")
            .and_then(|v| v.as_str()),
        context.session_id(),
        context.security_context().principal.id,
    )
}

async fn whoami(
    Extension(context): Extension<AuthenticatedRequestContext>,
    headers: axum::http::HeaderMap,
) -> String {
    format!(
        "{}:{:?}:{}:{}",
        context.tenant(),
        context.security_context().principal.kind,
        context.security_context().principal.id,
        headers.contains_key("authorization")
    )
}

fn protocol_route(requires_auth: bool) -> temper_server::http_endpoint::HttpEndpointRoute {
    temper_server::http_endpoint::HttpEndpointRoute {
        id: "he-protocol".to_string(),
        path_prefix: "/repo.git".to_string(),
        methods: vec!["GET".to_string(), "POST".to_string()],
        integration_module: "protocol-adapter".to_string(),
        requires_auth,
        timeout_secs: 60,
        max_fuel: None,
        max_memory: None,
        max_response_bytes: None,
        action_bridge: None,
    }
}

fn app(state: PlatformState) -> Router {
    Router::new()
        .route("/tdata", get(ok_handler))
        .route("/tdata/$metadata", get(ok_handler))
        .route("/tdata/$hints", get(ok_handler))
        .route("/temper-client.js", get(ok_handler))
        .route("/static/temper-client.js", get(ok_handler))
        .route("/genesis", get(ok_handler))
        .route("/genesis/{*path}", get(ok_handler))
        .route(
            "/webhooks/{tenant}/{*path}",
            get(ok_handler).post(ok_handler),
        )
        .route("/healthz", get(ok_handler))
        .route("/api/identity/resolve", post(ok_handler))
        .route("/session-probe", get(session_probe))
        .route("/api/specs", get(ok_handler))
        .route("/whoami", get(whoami))
        .route("/repo.git/{*path}", get(whoami).post(whoami))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            bearer_auth_check,
        ))
        .layer(middleware::from_fn(
            temper_server::authz::strip_inbound_identity_headers,
        ))
        .with_state(state)
}

mod outer_context;

#[tokio::test]
async fn no_key_mode_rejects_protected_requests() {
    let response = app(PlatformState::new(None))
        .oneshot(
            HttpRequest::get("/api/specs")
                .header("x-temper-principal-kind", "admin")
                .header("x-temper-principal-id", "attacker")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn deployment_key_has_no_unregistered_fallback() {
    let mut state = PlatformState::new(None);
    state.api_token = Some("deployment-root".to_string());
    let response = app(state)
        .oneshot(
            HttpRequest::get("/api/specs")
                .header("authorization", "Bearer deployment-root")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn exact_public_routes_do_not_require_credentials() {
    let app = app(PlatformState::new(None));
    for request in [
        HttpRequest::get("/tdata").body(Body::empty()).unwrap(),
        HttpRequest::get("/tdata/$metadata")
            .body(Body::empty())
            .unwrap(),
        HttpRequest::get("/temper-client.js")
            .body(Body::empty())
            .unwrap(),
        HttpRequest::get("/static/temper-client.js")
            .body(Body::empty())
            .unwrap(),
        HttpRequest::get("/genesis/app.js")
            .body(Body::empty())
            .unwrap(),
        HttpRequest::post("/webhooks/tenant/provider")
            .body(Body::empty())
            .unwrap(),
        HttpRequest::get("/healthz").body(Body::empty()).unwrap(),
        HttpRequest::post("/api/identity/resolve")
            .body(Body::empty())
            .unwrap(),
    ] {
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
}

#[tokio::test]
async fn tenant_hints_require_a_credential() {
    let response = app(PlatformState::new(None))
        .oneshot(
            HttpRequest::get("/tdata/$hints")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn registered_credential_is_tenant_bound_and_headers_cannot_replace_it() {
    let state = PlatformState::new(None);
    crate::bootstrap::bootstrap_agent_specs(&state, "default", false, &BTreeMap::new());
    crate::bootstrap::bootstrap_operator_credential(&state, "tenant-key", "default").await;
    let router = app(state);

    let response = router
        .clone()
        .oneshot(
            HttpRequest::get("/whoami")
                .header("authorization", "Bearer tenant-key")
                .header("x-tenant-id", "default")
                .header("x-temper-principal-kind", "admin")
                .header("x-temper-principal-id", "attacker")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        String::from_utf8(body.to_vec()).unwrap(),
        "default:Agent:operator:false"
    );

    let cross_tenant = router
        .oneshot(
            HttpRequest::get("/whoami")
                .header("authorization", "Bearer tenant-key")
                .header("x-tenant-id", "other")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(cross_tenant.status(), StatusCode::UNAUTHORIZED);

    let basic = base64::engine::general_purpose::STANDARD.encode("git:tenant-key");
    let state = PlatformState::new(None);
    crate::bootstrap::bootstrap_agent_specs(&state, "default", false, &BTreeMap::new());
    crate::bootstrap::bootstrap_operator_credential(&state, "tenant-key", "default").await;
    let response = app(state)
        .oneshot(
            HttpRequest::get("/whoami")
                .header("authorization", format!("Basic {basic}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[test]
fn basic_credential_parser_uses_password_then_empty_password_username() {
    for (decoded, expected) in [
        ("git:tenant-key", "tenant-key"),
        ("tenant-key:", "tenant-key"),
    ] {
        let encoded = base64::engine::general_purpose::STANDARD.encode(decoded);
        let request = HttpRequest::get("/")
            .header("authorization", format!("Basic {encoded}"))
            .body(Body::empty())
            .unwrap();
        assert_eq!(request_credential(&request), Some(expected.to_string()));
    }
}

#[test]
fn oversized_bearer_credentials_are_rejected_before_resolution() {
    let oversized = "x".repeat(temper_server::identity::MAX_CREDENTIAL_BYTES + 1);
    let request = HttpRequest::get("/")
        .header("authorization", format!("Bearer {oversized}"))
        .body(Body::empty())
        .unwrap();
    assert!(bearer_credential(&request).is_none());
    assert!(request_credential(&request).is_none());
}

#[tokio::test]
async fn malformed_tenant_header_is_rejected_without_panicking() {
    let response = app(PlatformState::new(None))
        .oneshot(
            HttpRequest::get("/healthz")
                .header("x-tenant-id", ":")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn public_classifier_does_not_allow_prefix_confusion() {
    let app = app(PlatformState::new(None));
    for path in ["/tdata/Orders", "/genesis-evil", "/webhooks-evil/path"] {
        let response = app
            .clone()
            .oneshot(HttpRequest::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
    }
}

#[tokio::test]
async fn unauthenticated_unknown_tenant_does_not_allocate_route_table() {
    let state = PlatformState::new(None);
    let tables = state.server.http_endpoint_tables.clone();
    let response = app(state)
        .oneshot(
            HttpRequest::get("/unknown-protocol")
                .header("x-tenant-id", "attacker-created")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(tables.tenant_count().await, 0);
}

#[tokio::test]
async fn internal_capability_restores_exact_context_without_identity_resolution() {
    let state = PlatformState::new(None);
    let mut security_context = temper_authz::SecurityContext::from_resolved_identity(
        "invoking-agent",
        "planner",
        Some("session-1"),
    );
    security_context.principal.role = Some("planner-role".to_string());
    let token = state
        .server
        .internal_invocation_credentials
        .issue_for_url(
            AuthenticatedRequestContext::new(TenantId::new("tenant-a"), security_context),
            "GET",
            "http://127.0.0.1:3000/whoami?view=full",
        )
        .expect("internal credential should issue");

    let response = app(state)
        .oneshot(
            HttpRequest::get("/whoami?view=full")
                .header("authorization", format!("Bearer {token}"))
                .header("x-tenant-id", "tenant-a")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        String::from_utf8(body.to_vec()).unwrap(),
        "tenant-a:Agent:invoking-agent:false"
    );
}

#[tokio::test]
async fn reserved_internal_prefix_never_falls_back_to_agent_credentials() {
    let state = PlatformState::new(None);
    crate::bootstrap::bootstrap_agent_specs(&state, "default", false, &BTreeMap::new());
    let reserved = format!(
        "{}registered-as-normal",
        temper_server::internal_invocation::INTERNAL_INVOCATION_BEARER_PREFIX
    );
    crate::bootstrap::bootstrap_operator_credential(&state, &reserved, "default").await;

    let response = app(state)
        .oneshot(
            HttpRequest::get("/whoami")
                .header("authorization", format!("Bearer {reserved}"))
                .header("x-tenant-id", "default")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn internal_capability_on_public_route_is_still_consumed_once() {
    let state = PlatformState::new(None);
    let token = state
        .server
        .internal_invocation_credentials
        .issue_for_url(
            AuthenticatedRequestContext::new(
                TenantId::new("tenant-a"),
                temper_authz::SecurityContext::from_resolved_identity(
                    "invoking-agent",
                    "worker",
                    None,
                ),
            ),
            "GET",
            "http://127.0.0.1:3000/tdata",
        )
        .expect("internal credential should issue");
    let request = || {
        HttpRequest::get("/tdata")
            .header("authorization", format!("Bearer {token}"))
            .header("x-tenant-id", "tenant-a")
            .body(Body::empty())
            .unwrap()
    };
    let app = app(state);

    assert_eq!(
        app.clone().oneshot(request()).await.unwrap().status(),
        StatusCode::OK
    );
    assert_eq!(
        app.oneshot(request()).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
}

/// A caller-asserted session header becomes `context.sessionId` — a Cedar
/// input — only when an approved decision binds that exact session to the
/// asserting principal (ADR-0157). Unvalidated assertions stay telemetry-only,
/// so session-scoped permits cannot be satisfied by replaying a header.
#[tokio::test]
async fn session_header_reaches_cedar_only_through_an_approved_grant() {
    let mut state = PlatformState::new(None);
    let dir = std::env::temp_dir().join(format!(
        "temper-session-edge-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let turso = temper_store_turso::TursoEventStore::new(
        &format!("file:{}", dir.join("grants.db").display()),
        None,
    )
    .await
    .expect("create local turso db");
    state
        .server
        .set_storage_stack(temper_server::storage::StorageStack::from_turso(turso));
    crate::bootstrap::bootstrap_agent_specs(&state, "default", false, &BTreeMap::new());
    crate::bootstrap::bootstrap_operator_credential(&state, "tenant-key", "default").await;
    let server_state = state.server.clone();
    let router = app(state);

    let probe = |session: Option<&'static str>| {
        let router = router.clone();
        async move {
            let mut request = HttpRequest::get("/session-probe")
                .header("authorization", "Bearer tenant-key")
                .header("x-tenant-id", "default");
            if let Some(session) = session {
                request = request.header("x-session-id", session);
            }
            let response = router
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            String::from_utf8(body.to_vec()).unwrap()
        }
    };

    // Before any grant: the asserted header is telemetry, never Cedar input.
    let unvalidated = probe(Some("sess-approved")).await;
    assert!(
        unvalidated.starts_with("cedar=None telemetry=Some(\"sess-approved\")"),
        "an unvalidated session assertion must stay out of the Cedar context: {unvalidated}"
    );
    let agent_id = unvalidated
        .rsplit("principal=")
        .next()
        .expect("probe reports the principal")
        .to_string();

    // A human approves a session-scoped decision for exactly this principal.
    let mut scope = temper_authz::PolicyScopeMatrix::default_for(Some("operator"));
    scope.duration = temper_authz::DurationScope::Session;
    scope.session_id = Some("sess-approved".to_string());
    let mut decision = temper_server::state::PendingDecision::from_denial(
        "default",
        &agent_id,
        "Delete",
        "Order",
        "order-1",
        serde_json::json!({}),
        "denied by policy",
        None,
    );
    decision.status = temper_server::state::DecisionStatus::Approved;
    decision.approved_scope = Some(scope);
    server_state
        .persist_pending_decision(&decision)
        .await
        .expect("persist approved session grant");

    // The approved (principal, session) pair now reaches Cedar.
    let validated = probe(Some("sess-approved")).await;
    assert!(
        validated.starts_with("cedar=Some(\"sess-approved\") telemetry=Some(\"sess-approved\")"),
        "the granted session must reach the Cedar context: {validated}"
    );

    // A different asserted session still does not.
    let other = probe(Some("sess-other")).await;
    assert!(
        other.starts_with("cedar=None telemetry=Some(\"sess-other\")"),
        "a session outside the grant must stay out of the Cedar context: {other}"
    );
}
