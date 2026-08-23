use super::*;
use temper_server::http_endpoint::AdmittedHttpEndpoint;

fn outer_context(tenant: &str, principal: &str) -> AuthenticatedRequestContext {
    AuthenticatedRequestContext::new(
        TenantId::new(tenant),
        temper_authz::SecurityContext::from_resolved_identity(principal, "embedded-host", None),
    )
}

#[tokio::test]
async fn declared_public_http_endpoint_gets_anonymous_typed_context() {
    let state = PlatformState::new(None);
    state
        .server
        .http_endpoint_tables
        .table_for(&TenantId::default())
        .await
        .replace(vec![protocol_route(false)])
        .await;

    let response = app(state)
        .oneshot(
            HttpRequest::get("/repo.git/info/refs")
                .header("authorization", "Basic malformed")
                .body(Body::empty())
                .expect("build public endpoint request"),
        )
        .await
        .expect("dispatch public endpoint request");
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read public endpoint response body");
    assert_eq!(
        String::from_utf8(body.to_vec()).expect("public endpoint body is utf-8"),
        "default:Customer:anonymous:false"
    );
}

#[tokio::test]
async fn private_http_endpoint_challenges_basic_credentials_before_guest_dispatch() {
    let state = PlatformState::new(None);
    state
        .server
        .http_endpoint_tables
        .table_for(&TenantId::default())
        .await
        .replace(vec![protocol_route(true)])
        .await;

    let response = app(state)
        .oneshot(
            HttpRequest::get("/repo.git/info/refs")
                .body(Body::empty())
                .expect("build private endpoint request"),
        )
        .await
        .expect("dispatch private endpoint request");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        response
            .headers()
            .get("www-authenticate")
            .and_then(|value| value.to_str().ok()),
        Some("Basic realm=\"Temper\"")
    );
}

#[tokio::test]
async fn matching_tenant_typed_outer_context_completes_authentication() {
    let response = app(PlatformState::new(None))
        .layer(Extension(outer_context("default", "outer-user")))
        .oneshot(
            HttpRequest::get("/whoami")
                .header("x-tenant-id", "default")
                .header("authorization", "Bearer must-not-reach-handler")
                .body(Body::empty())
                .expect("build typed-context request"),
        )
        .await
        .expect("dispatch typed-context request");

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read typed-context response body");
    assert_eq!(
        String::from_utf8(body.to_vec()).expect("typed-context body is utf-8"),
        "default:Agent:outer-user:false"
    );
}

#[tokio::test]
async fn typed_outer_context_cannot_cross_tenants_on_protected_or_public_routes() {
    let router =
        app(PlatformState::new(None)).layer(Extension(outer_context("tenant-a", "outer-user")));
    for request in [
        HttpRequest::get("/whoami")
            .header("x-tenant-id", "tenant-b")
            .body(Body::empty())
            .expect("build protected cross-tenant request"),
        HttpRequest::get("/healthz")
            .header("x-tenant-id", "tenant-b")
            .body(Body::empty())
            .expect("build public cross-tenant request"),
    ] {
        let response = router
            .clone()
            .oneshot(request)
            .await
            .expect("dispatch cross-tenant request");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
}

#[tokio::test]
async fn typed_outer_context_strips_authorization_on_public_routes() {
    async fn authorization_probe(headers: axum::http::HeaderMap) -> &'static str {
        if headers.contains_key("authorization") {
            "present"
        } else {
            "absent"
        }
    }

    let state = PlatformState::new(None);
    let router = Router::new()
        .route("/healthz", get(authorization_probe))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            bearer_auth_check,
        ))
        .layer(middleware::from_fn(
            temper_server::authz::strip_inbound_identity_headers,
        ))
        .with_state(state)
        .layer(Extension(outer_context("default", "outer-user")));

    let response = router
        .oneshot(
            HttpRequest::get("/healthz")
                .header("authorization", "Bearer must-not-reach-handler")
                .body(Body::empty())
                .expect("build public typed-context request"),
        )
        .await
        .expect("dispatch public typed-context request");
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read public typed-context response body");
    assert_eq!(
        String::from_utf8(body.to_vec()).expect("public typed-context body is utf-8"),
        "absent"
    );
}

#[tokio::test]
async fn typed_outer_context_preserves_protocol_route_admission() {
    async fn admitted(
        Extension(context): Extension<AuthenticatedRequestContext>,
        Extension(endpoint): Extension<AdmittedHttpEndpoint>,
    ) -> String {
        let matched = endpoint
            .into_matched(context.tenant(), "GET", "/repo.git/info/refs")
            .expect("admission must stay bound to this request");
        format!("{}:{}", context.tenant(), matched.route.id)
    }

    let state = PlatformState::new(None);
    state
        .server
        .http_endpoint_tables
        .table_for(&TenantId::default())
        .await
        .replace(vec![protocol_route(true)])
        .await;
    let router = Router::new()
        .route("/repo.git/{*path}", get(admitted))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            bearer_auth_check,
        ))
        .layer(middleware::from_fn(
            temper_server::authz::strip_inbound_identity_headers,
        ))
        .with_state(state)
        .layer(Extension(outer_context("default", "outer-user")));

    let response = router
        .oneshot(
            HttpRequest::get("/repo.git/info/refs")
                .header("x-tenant-id", "default")
                .body(Body::empty())
                .expect("build admitted protocol request"),
        )
        .await
        .expect("dispatch admitted protocol request");
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read admitted protocol response body");
    assert_eq!(
        String::from_utf8(body.to_vec()).expect("admitted protocol body is utf-8"),
        "default:he-protocol"
    );
}
