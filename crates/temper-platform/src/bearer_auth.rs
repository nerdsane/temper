//! Tenant-scoped bearer authentication middleware.
//!
//! Every protected request resolves a bearer credential in the requested
//! tenant and receives one typed [`temper_authz::AuthenticatedRequestContext`].
//! `TEMPER_API_KEY` may bootstrap a normal tenant credential, but it has no
//! special runtime fallback or deployment-wide Admin authority.

use crate::state::PlatformState;
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use temper_runtime::tenant::TenantId;

const BASIC_CREDENTIAL_DECODE_BUDGET: usize = 8 * 1024;

/// Resolve the request's bearer credential and attach typed authority.
pub async fn bearer_auth_check(
    State(state): State<PlatformState>,
    mut req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let bearer = bearer_credential(&req);
    let tenant = requested_tenant(&req)?;

    if bearer
        .as_deref()
        .is_some_and(temper_server::internal_invocation::is_internal_invocation_bearer)
    {
        let token = bearer.as_deref().ok_or(StatusCode::UNAUTHORIZED)?;
        let authenticated = state
            .server
            .internal_invocation_credentials
            .consume_for_request(token, &tenant, req.method(), req.uri())
            .map_err(|_| StatusCode::UNAUTHORIZED)?;
        // The capability has already been consumed; do not expose it to
        // downstream handlers, logs, or accidental forwarding.
        req.headers_mut().remove("authorization");
        req.extensions_mut().insert(authenticated);
        return Ok(next.run(req).await);
    }

    if is_public_request(&req) {
        if !req.uri().path().starts_with("/webhooks/") {
            req.headers_mut().remove("authorization");
        }
        return Ok(next.run(req).await);
    }

    let matched_endpoint = match state.server.http_endpoint_tables.get(&tenant).await {
        Some(table) => {
            table
                .match_request(req.method().as_str(), req.uri().path())
                .await
        }
        None => None,
    };
    let request_method = req.method().as_str().to_string();
    let request_path = req.uri().path().to_string();

    if let Some(token) = request_credential(&req)
        && let Some(identity) = temper_server::identity::IdentityResolver::new()
            .resolve(&state.server, &tenant, &token)
            .await
    {
        let security_context = temper_authz::SecurityContext::from_resolved_identity(
            &identity.agent_instance_id,
            &identity.agent_type_name,
            None,
        );
        let authenticated =
            temper_authz::AuthenticatedRequestContext::new(tenant.clone(), security_context);

        // Retained during the handler migration for non-authority metadata.
        // Cedar consumers use only `AuthenticatedRequestContext`.
        req.extensions_mut().insert(identity);
        req.extensions_mut().insert(authenticated);
        if let Some(matched) = matched_endpoint {
            req.extensions_mut()
                .insert(temper_server::http_endpoint::AdmittedHttpEndpoint::new(
                    tenant.clone(),
                    &request_method,
                    &request_path,
                    matched,
                ));
        }
        // The credential has served its only purpose. Downstream handlers and
        // tenant WASM modules receive typed authority, never the reusable secret.
        req.headers_mut().remove("authorization");
        return Ok(next.run(req).await);
    }

    // Routes declared public have an explicit anonymous Customer context.
    // This lets foreign-protocol adapters serve public reads without the old
    // System fallback or trusting a guest-returned principal.
    if let Some(matched) = matched_endpoint {
        if matched.route.requires_auth {
            return Ok((
                StatusCode::UNAUTHORIZED,
                [("www-authenticate", "Basic realm=\"Temper\"")],
            )
                .into_response());
        }
        req.headers_mut().remove("authorization");
        req.extensions_mut()
            .insert(temper_authz::AuthenticatedRequestContext::new(
                tenant.clone(),
                temper_authz::SecurityContext::from_headers(&[]),
            ));
        req.extensions_mut()
            .insert(temper_server::http_endpoint::AdmittedHttpEndpoint::new(
                tenant,
                &request_method,
                &request_path,
                matched,
            ));
        return Ok(next.run(req).await);
    }

    Err(StatusCode::UNAUTHORIZED)
}

fn authorization_parts(req: &Request) -> Option<(&str, &str)> {
    let value = req.headers().get("authorization")?.to_str().ok()?;
    let (scheme, credential) = value.split_once(' ')?;
    let credential = credential.trim();
    (!credential.is_empty()).then_some((scheme, credential))
}

fn bearer_credential(req: &Request) -> Option<String> {
    let (scheme, credential) = authorization_parts(req)?;
    (scheme.eq_ignore_ascii_case("bearer")
        && credential.len() <= temper_server::identity::MAX_CREDENTIAL_BYTES)
        .then(|| credential.to_string())
}

fn request_credential(req: &Request) -> Option<String> {
    let (scheme, credential) = authorization_parts(req)?;
    if scheme.eq_ignore_ascii_case("bearer") {
        return (credential.len() <= temper_server::identity::MAX_CREDENTIAL_BYTES)
            .then(|| credential.to_string());
    }
    if !scheme.eq_ignore_ascii_case("basic") || credential.len() > BASIC_CREDENTIAL_DECODE_BUDGET {
        return None;
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(credential)
        .ok()?;
    if decoded.len() > BASIC_CREDENTIAL_DECODE_BUDGET {
        return None;
    }
    let decoded = std::str::from_utf8(&decoded).ok()?;
    let (username, password) = decoded.split_once(':')?;
    let token = if password.is_empty() {
        username
    } else {
        password
    };
    (!token.is_empty()).then(|| token.to_string())
}

fn is_public_request(req: &Request) -> bool {
    temper_server::authz::is_public_kernel_request(req.method(), req.uri().path())
        || matches!(
            (req.method(), req.uri().path()),
            (&Method::GET, "/healthz") | (&Method::POST, "/api/identity/resolve")
        )
}

fn requested_tenant(req: &Request) -> Result<TenantId, StatusCode> {
    let tenant = req
        .headers()
        .get("x-tenant-id")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|tenant| !tenant.is_empty())
        .unwrap_or("default");
    TenantId::try_new(tenant).map_err(|_| StatusCode::BAD_REQUEST)
}

#[cfg(test)]
#[path = "bearer_auth/tests.rs"]
mod tests;
