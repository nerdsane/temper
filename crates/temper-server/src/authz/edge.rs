//! Ingress edge: strip client-asserted identity, materialize resolved identity
//! (ADR-0157).
//!
//! A request from an external client must never assert its own Cedar principal.
//! `SecurityContext::from_headers` reads a family of `x-temper-*` headers —
//! principal id/kind, agent type/role, scopes, ABAC attributes, action-context
//! — to build the authoritative principal. The edge removes all of those
//! *before any handler or auth middleware reads them*, so identity can only
//! come from a resolved credential or a trusted in-process context (ADR-0033).
//!
//! Because a client's headers are stripped but the platform still needs to
//! convey the principal it resolved from a credential, the credential is carried
//! as an [`EdgeAuthenticatedPrincipal`] request *extension* — which the strip
//! (a header-only transform) cannot forge or remove — and materialized back
//! into trusted headers after the strip by [`materialize_authenticated_principal`].
//!
//! Two layers, ordered outer→inner:
//! 1. [`strip_inbound_identity_headers`] (outermost) — remove client `x-temper-*`.
//! 2. credential resolution (platform `bearer_auth`) — set the extension.
//! 3. [`materialize_authenticated_principal`] (innermost) — extension → headers.

use axum::extract::Request;
use axum::http::{HeaderName, HeaderValue};
use axum::middleware::Next;
use axum::response::Response;
use temper_authz::{PrincipalKind, TRUSTED_PRINCIPAL_HEADER};

/// `x-temper-*` header-name prefixes that carry request-scoped *observability*
/// context (session, intent, workflow trace correlation). These are the only
/// `x-temper-*` headers a client is permitted to set; everything else under
/// `x-temper-` is authority the platform derives, never the caller.
///
/// See `request_context::extract_agent_context`, which reads exactly these.
const OBSERVABILITY_PREFIXES: [&str; 2] = ["x-temper-observe-", "x-temper-workflow-"];

/// A principal the platform resolved from a credential, to be materialized into
/// trusted headers by the edge after client headers are stripped.
///
/// Set only by credential-resolution middleware (never derived from client
/// input). Carried as a request extension so it survives
/// [`strip_inbound_identity_headers`], which only rewrites headers.
#[derive(Debug, Clone)]
pub struct EdgeAuthenticatedPrincipal {
    /// The resolved principal kind.
    pub kind: PrincipalKind,
    /// The resolved principal id.
    pub id: String,
}

impl EdgeAuthenticatedPrincipal {
    /// The bootstrapped operator/admin principal for a verified global-API-key
    /// (`TEMPER_API_KEY`) holder.
    pub fn operator() -> Self {
        Self {
            kind: PrincipalKind::Admin,
            id: "api-key-holder".to_string(),
        }
    }
}

/// Whether `name` is an `x-temper-*` header the client is not allowed to
/// assert. `HeaderName` is stored lowercase, so a plain prefix match is a
/// case-insensitive match.
fn is_client_asserted_identity_header(name: &HeaderName) -> bool {
    let name = name.as_str();
    name.starts_with("x-temper-")
        && !OBSERVABILITY_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
}

/// Axum middleware that removes every client-asserted `x-temper-*` identity
/// header at the ingress edge (ADR-0157, Sub-Decision 2).
///
/// Applied outermost so it runs before credential resolution and before any
/// handler: internal components (the bearer-auth edge, the WASM host) convey
/// identity only *after* this layer — via [`EdgeAuthenticatedPrincipal`] or a
/// resolved-identity extension — never through a header this layer would strip.
pub async fn strip_inbound_identity_headers(mut req: Request, next: Next) -> Response {
    let headers = req.headers_mut();
    // Collect first (immutable borrow ends), then remove — a header name may
    // carry multiple values, and `remove` clears all of them at once.
    let to_remove: Vec<HeaderName> = headers
        .keys()
        .filter(|name| is_client_asserted_identity_header(name))
        .cloned()
        .collect();
    for name in to_remove {
        headers.remove(&name);
    }
    next.run(req).await
}

/// Axum middleware that materializes an [`EdgeAuthenticatedPrincipal`] extension
/// into trusted `x-temper-principal-*` headers plus the internal trust marker
/// (ADR-0157).
///
/// Runs *after* [`strip_inbound_identity_headers`] and after credential
/// resolution, so the headers it writes are the only principal headers a
/// downstream handler's `from_headers` will see. A `System` principal is
/// intentionally never materialized — `System` is not derivable from headers;
/// in-process code uses `SecurityContext::system()`.
pub async fn materialize_authenticated_principal(mut req: Request, next: Next) -> Response {
    if let Some(principal) = req
        .extensions()
        .get::<EdgeAuthenticatedPrincipal>()
        .cloned()
    {
        let kind = match principal.kind {
            PrincipalKind::Customer => "customer",
            PrincipalKind::Agent => "agent",
            PrincipalKind::Admin => "admin",
            // Not materialized into headers; from_headers never honors it.
            PrincipalKind::System => "customer",
        };
        if let Ok(id_value) = HeaderValue::from_str(&principal.id) {
            let headers = req.headers_mut();
            headers.insert(
                HeaderName::from_static("x-temper-principal-kind"),
                HeaderValue::from_static(kind),
            );
            headers.insert(HeaderName::from_static("x-temper-principal-id"), id_value);
            headers.insert(
                HeaderName::from_static(TRUSTED_PRINCIPAL_HEADER),
                HeaderValue::from_static("1"),
            );
        } else {
            tracing::warn!(
                principal_id = %principal.id,
                "resolved principal id is not a valid header value; dropping to anonymous"
            );
        }
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::{HeaderMap, Request as HttpRequest, StatusCode};
    use axum::routing::get;
    use tower::ServiceExt;

    /// Echo back which `x-temper-*` headers survived, so tests can assert what
    /// reached the handler.
    async fn echo_temper_headers(headers: HeaderMap) -> String {
        let mut names: Vec<String> = headers
            .keys()
            .map(|k| k.as_str().to_string())
            .filter(|k| k.starts_with("x-temper-"))
            .collect();
        names.sort();
        names.join(",")
    }

    fn strip_app() -> Router {
        Router::new()
            .route("/echo", get(echo_temper_headers))
            .layer(axum::middleware::from_fn(strip_inbound_identity_headers))
    }

    async fn surviving_headers(req: HttpRequest<Body>) -> String {
        let resp = strip_app().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(body.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn strips_principal_and_trust_marker_headers() {
        let req = HttpRequest::get("/echo")
            .header("x-temper-principal-kind", "admin")
            .header("x-temper-principal-id", "attacker")
            .header("x-temper-agent-type", "claude-code")
            .header("x-temper-agent-role", "supervisor")
            .header("x-temper-acting-for", "victim")
            .header("x-temper-principal-scopes", "repo:push")
            .header("x-temper-attr-approvallimit", "999999")
            .header("x-temper-action-context", "composite:App.Fork")
            .header("x-temper-ctx-agenttypeverified", "true")
            // A client cannot smuggle the internal trust marker either.
            .header(TRUSTED_PRINCIPAL_HEADER, "1")
            .body(Body::empty())
            .unwrap();
        assert_eq!(surviving_headers(req).await, "");
    }

    #[tokio::test]
    async fn preserves_observability_headers() {
        let req = HttpRequest::get("/echo")
            .header("x-temper-observe-session-id", "sess-1")
            .header("x-temper-observe-intent", "approve invoice")
            .header("x-temper-workflow-run-id", "WF:1")
            .header("x-temper-workflow-root-entity-type", "Job")
            // authority header alongside them — must still be stripped
            .header("x-temper-principal-kind", "admin")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            surviving_headers(req).await,
            "x-temper-observe-intent,x-temper-observe-session-id,x-temper-workflow-root-entity-type,x-temper-workflow-run-id"
        );
    }

    #[tokio::test]
    async fn leaves_non_temper_headers_untouched() {
        let req = HttpRequest::get("/echo")
            .header("authorization", "Bearer tok")
            .header("x-tenant-id", "default")
            .header("content-type", "application/json")
            .body(Body::empty())
            .unwrap();
        // No x-temper-* headers reached the handler, and the request still ran.
        assert_eq!(surviving_headers(req).await, "");
    }

    #[test]
    fn classifier_matches_authority_not_observability() {
        let authority = [
            "x-temper-principal-kind",
            "x-temper-principal-id",
            "x-temper-agent-type",
            "x-temper-principal-scopes",
            "x-temper-attr-region",
            "x-temper-ctx-sessionid",
            "x-temper-action-context",
            TRUSTED_PRINCIPAL_HEADER,
        ];
        for h in authority {
            assert!(
                is_client_asserted_identity_header(&HeaderName::from_static(leak(h))),
                "{h} should be treated as client-asserted authority"
            );
        }
        let allowed = [
            "x-temper-observe-session-id",
            "x-temper-workflow-run-id",
            "content-type",
            "authorization",
            "x-tenant-id",
        ];
        for h in allowed {
            assert!(
                !is_client_asserted_identity_header(&HeaderName::from_static(leak(h))),
                "{h} should pass through"
            );
        }
    }

    /// Materialize turns a resolved operator principal into trusted headers so
    /// `from_headers` yields Admin — the credential-derived path, never a client
    /// header.
    #[tokio::test]
    async fn materialize_injects_trusted_operator_headers() {
        async fn set_operator(mut req: Request, next: Next) -> Response {
            req.extensions_mut()
                .insert(EdgeAuthenticatedPrincipal::operator());
            next.run(req).await
        }

        let app = Router::new()
            .route("/echo", get(echo_principal))
            // Inner: materialize. Outer: set the extension (stands in for
            // bearer-auth) then strip.
            .layer(axum::middleware::from_fn(
                materialize_authenticated_principal,
            ))
            .layer(axum::middleware::from_fn(set_operator))
            .layer(axum::middleware::from_fn(strip_inbound_identity_headers));

        // Client tries to assert admin directly — stripped — but the resolved
        // operator extension still yields a trusted Admin.
        let resp = app
            .oneshot(
                HttpRequest::get("/echo")
                    .header("x-temper-principal-id", "attacker")
                    .header("x-temper-principal-kind", "admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            String::from_utf8(body.to_vec()).unwrap(),
            "Admin:api-key-holder"
        );
    }

    async fn echo_principal(headers: HeaderMap) -> String {
        let pairs: Vec<(String, String)> = headers
            .iter()
            .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();
        let ctx = temper_authz::SecurityContext::from_headers(&pairs);
        format!("{:?}:{}", ctx.principal.kind, ctx.principal.id)
    }

    /// `HeaderName::from_static` needs a `'static str`; leak the small test
    /// inputs so the loop can build names from an array of `&str`.
    fn leak(s: &str) -> &'static str {
        Box::leak(s.to_string().into_boxed_str())
    }
}
