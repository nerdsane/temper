//! Ingress defense for caller-controlled Temper headers (ADR-0157).
//!
//! Cedar authority is carried in an immutable
//! [`temper_authz::AuthenticatedRequestContext`] request extension. Headers
//! never materialize that authority. This middleware removes the closed
//! `x-temper-*` authority namespace while retaining validated schema routing,
//! request identity, and correlation-only observability metadata.

use axum::extract::Request;
use axum::http::{HeaderName, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

const CORRELATION_PREFIXES: [&str; 2] = ["x-temper-observe-", "x-temper-workflow-"];
const NON_AUTHORITY_HEADERS: [&str; 4] = [
    "x-temper-schema-scope-kind",
    "x-temper-schema-scope-id",
    "x-temper-schema-bundle-digest",
    "x-temper-request-id",
];

/// Return whether a header belongs to the caller-controlled authority
/// namespace that must be removed before dispatch.
///
/// `HeaderName` values are normalized to lowercase. The allowlist is narrow on
/// purpose: new `x-temper-*` headers are authority by default unless explicitly
/// classified as schema routing, request identity, or correlation metadata.
pub(crate) fn is_caller_authority_header(name: &HeaderName) -> bool {
    let name = name.as_str();
    name.starts_with("x-temper-")
        && !NON_AUTHORITY_HEADERS.contains(&name)
        && !CORRELATION_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
}

/// Remove caller-supplied `x-temper-*` authority headers.
///
/// Credential resolution communicates identity through
/// [`temper_authz::AuthenticatedRequestContext`], never by adding headers back
/// after this layer. Schema-pin routing, request identity, and the
/// `x-temper-observe-*` / `x-temper-workflow-*` correlation namespaces survive;
/// none of them are Cedar authority inputs.
pub async fn strip_inbound_identity_headers(mut request: Request, next: Next) -> Response {
    let names = request
        .headers()
        .keys()
        .filter(|name| is_caller_authority_header(name))
        .cloned()
        .collect::<Vec<_>>();
    for name in names {
        request.headers_mut().remove(name);
    }
    next.run(request).await
}

/// Return whether the kernel route has its own non-Class-A admission boundary.
///
/// This is the single classifier shared by the kernel guard and the outer
/// platform bearer edge. Keep it exact: webhook ingress is admitted by its
/// Class B verifier; the remaining routes expose only bootstrap metadata or
/// immutable static assets.
pub fn is_public_kernel_request(method: &Method, path: &str) -> bool {
    if method == Method::GET
        && matches!(
            path,
            "/tdata"
                | "/tdata/"
                | "/tdata/$metadata"
                | "/temper-client.js"
                | "/static/temper-client.js"
                | "/genesis"
                | "/genesis/"
        )
    {
        return true;
    }
    (matches!(*method, Method::GET | Method::POST) && path.starts_with("/webhooks/"))
        || (method == Method::GET && path.starts_with("/genesis/"))
}

/// Reject protected kernel routes that lack authenticated typed authority.
///
/// The guard is installed by [`crate::build_router`] itself so direct embedders
/// cannot accidentally expose a handler that reconstructs identity from HTTP
/// headers. Webhook ingress remains governed by its Class B admission boundary.
pub async fn require_authenticated_request_context(request: Request, next: Next) -> Response {
    if is_public_kernel_request(request.method(), request.uri().path())
        || request
            .extensions()
            .get::<temper_authz::AuthenticatedRequestContext>()
            .is_some()
    {
        return next.run(request).await;
    }

    (
        StatusCode::UNAUTHORIZED,
        axum::Json(serde_json::json!({
            "error": {
                "code": "AuthenticationRequired",
                "message": "A valid tenant credential is required"
            }
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::{HeaderMap, Request as HttpRequest, StatusCode};
    use axum::routing::get;
    use tower::ServiceExt;

    async fn echo_temper_headers(headers: HeaderMap) -> String {
        let mut names = headers
            .keys()
            .map(|name| name.as_str().to_string())
            .filter(|name| name.starts_with("x-temper-"))
            .collect::<Vec<_>>();
        names.sort();
        names.join(",")
    }

    async fn surviving_headers(request: HttpRequest<Body>) -> String {
        let app = Router::new()
            .route("/echo", get(echo_temper_headers))
            .layer(axum::middleware::from_fn(strip_inbound_identity_headers));
        let response = app.oneshot(request).await.expect("request should run");
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body should be readable");
        String::from_utf8(body.to_vec()).expect("header names should be UTF-8")
    }

    #[tokio::test]
    async fn strips_complete_authority_namespace_including_legacy_marker() {
        let request = HttpRequest::get("/echo")
            .header("x-temper-principal-kind", "admin")
            .header("x-temper-principal-id", "attacker")
            .header("x-temper-agent-type", "operator")
            .header("x-temper-agent-role", "supervisor")
            .header("x-temper-acting-for", "victim")
            .header("x-temper-principal-scopes", "repo:push")
            .header("x-temper-attr-approvallimit", "999999")
            .header("x-temper-action-context", "composite:App.Fork")
            .header("x-temper-ctx-agenttypeverified", "true")
            .header("x-temper-internal-trusted-principal", "1")
            .header("x-temper-future-authority", "also stripped")
            .body(Body::empty())
            .expect("request should build");

        assert_eq!(surviving_headers(request).await, "");
    }

    #[tokio::test]
    async fn preserves_non_authority_routing_and_correlation_headers() {
        let request = HttpRequest::get("/echo")
            .header("x-temper-schema-scope-kind", "task")
            .header("x-temper-schema-scope-id", "task-1")
            .header("x-temper-schema-bundle-digest", "sha256:fixture")
            .header("x-temper-request-id", "request-1")
            .header("x-temper-observe-session-id", "session-1")
            .header("x-temper-observe-intent", "approve invoice")
            .header("x-temper-workflow-run-id", "workflow-1")
            .header("x-temper-workflow-root-entity-type", "Job")
            .header("x-temper-principal-kind", "admin")
            .body(Body::empty())
            .expect("request should build");

        assert_eq!(
            surviving_headers(request).await,
            "x-temper-observe-intent,x-temper-observe-session-id,x-temper-request-id,x-temper-schema-bundle-digest,x-temper-schema-scope-id,x-temper-schema-scope-kind,x-temper-workflow-root-entity-type,x-temper-workflow-run-id"
        );
    }

    #[test]
    fn classifier_leaves_non_temper_transport_headers_alone() {
        for name in [
            "authorization",
            "content-type",
            "x-tenant-id",
            "traceparent",
        ] {
            assert!(!is_caller_authority_header(&HeaderName::from_static(name)));
        }
    }

    #[test]
    fn public_route_allowlist_is_exact() {
        for path in [
            "/tdata",
            "/tdata/",
            "/tdata/$metadata",
            "/temper-client.js",
            "/static/temper-client.js",
            "/genesis",
            "/genesis/app.js",
            "/webhooks/tenant/provider",
        ] {
            assert!(is_public_kernel_request(&Method::GET, path), "{path}");
        }
        for path in [
            "/tdata/Orders",
            "/tdata/$hints",
            "/tdata/$events",
            "/api/authorize",
            "/observe/decisions",
            "/_admin/reload",
            "/not-a-static-route",
        ] {
            assert!(!is_public_kernel_request(&Method::GET, path), "{path}");
        }
        assert!(!is_public_kernel_request(&Method::POST, "/tdata"));
        assert!(!is_public_kernel_request(
            &Method::DELETE,
            "/webhooks/tenant/provider"
        ));
    }

    #[tokio::test]
    async fn protected_route_requires_typed_context_not_identity_headers() {
        let app = Router::new()
            .route("/protected", get(|| async { StatusCode::OK }))
            .layer(axum::middleware::from_fn(
                require_authenticated_request_context,
            ));
        let forged = HttpRequest::get("/protected")
            .header("x-temper-principal-kind", "admin")
            .header("x-temper-principal-id", "attacker")
            .body(Body::empty())
            .expect("request should build");
        assert_eq!(
            app.clone()
                .oneshot(forged)
                .await
                .expect("request should run")
                .status(),
            StatusCode::UNAUTHORIZED
        );

        let mut authenticated = HttpRequest::get("/protected")
            .body(Body::empty())
            .expect("request should build");
        authenticated
            .extensions_mut()
            .insert(temper_authz::AuthenticatedRequestContext::new(
                temper_runtime::tenant::TenantId::new("tenant-a"),
                temper_authz::SecurityContext::from_resolved_identity("agent-1", "operator", None),
            ));
        assert_eq!(
            app.oneshot(authenticated)
                .await
                .expect("request should run")
                .status(),
            StatusCode::OK
        );
    }
}
