//! Tenant-scoped bearer authentication middleware.
//!
//! Every protected request resolves a bearer credential, or carries a
//! [`HostVerifiedIdentity`], in the requested tenant and receives one typed
//! [`temper_authz::AuthenticatedRequestContext`].
//! `TEMPER_API_KEY` may bootstrap a normal tenant credential, but it has no
//! special runtime fallback or deployment-wide Admin authority.

use crate::host_identity::HostVerifiedIdentity;
use crate::state::PlatformState;
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use temper_runtime::tenant::TenantId;

const BASIC_CREDENTIAL_DECODE_BUDGET: usize = 8 * 1024;

/// Is the presented credential in a scheme a protocol guest is meant to resolve?
/// The rule itself lives in `temper_server::authz::is_forwardable_protocol_scheme`.
fn credential_is_forwardable_protocol_format(headers: &axum::http::HeaderMap) -> bool {
    headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim_start().split_once(' '))
        .is_some_and(|(scheme, _)| temper_server::authz::is_forwardable_protocol_scheme(scheme))
}

/// Resolve the request's bearer credential and attach typed authority.
pub async fn bearer_auth_check(
    State(state): State<PlatformState>,
    mut req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let bearer = bearer_credential(&req);
    let tenant = requested_tenant(&req)?;

    if let Some(identity) = req.extensions_mut().remove::<HostVerifiedIdentity>() {
        let authenticated = identity.into_request_context(tenant, req.headers());
        req.headers_mut().remove("authorization");
        req.extensions_mut().insert(authenticated);
        return Ok(next.run(req).await);
    }

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

    let resolved_identity = match request_credential(&req) {
        Some(token) => match temper_server::identity::IdentityResolver::new()
            .resolve(&state.server, &tenant, &token)
            .await
        {
            Ok(identity) => Some(identity),
            // The identity authority could not be read — the credential was
            // never shown to be bad. Fail closed as a dependency failure
            // (503), never as 401/anonymous/protocol-forwarded, and never
            // reach `next.run` with a mutated request.
            Err(temper_server::identity::IdentityError::Unavailable(reason)) => {
                tracing::warn!(
                    tenant = %tenant,
                    path = %req.uri().path(),
                    reason,
                    "identity authority unavailable; failing closed with 503"
                );
                return Err(StatusCode::SERVICE_UNAVAILABLE);
            }
            Err(temper_server::identity::IdentityError::Invalid) => None,
        },
        None => None,
    };

    if let Some(identity) = resolved_identity {
        let session_id = temper_server::request_context::session_id_from_headers(req.headers());
        let intent = temper_server::request_context::intent_from_headers(req.headers());
        let verified_session = match session_id.as_deref() {
            Some(sid) => state
                .server
                .session_grant_verified(tenant.as_str(), &identity.agent_instance_id, sid)
                .await
                .then_some(sid),
            None => None,
        };
        // A trusted-issuer JWT resolves to an agent acting for a human (or the
        // human themselves, a Customer) and carries a verified acting_for/role
        // that from_resolved_identity cannot express (ARN-255, RFC-0002). The
        // AgentCredential path stays exactly as before.
        let security_context = if identity.from_jwt {
            let kind = if identity.is_human {
                temper_authz::PrincipalKind::Customer
            } else {
                temper_authz::PrincipalKind::Agent
            };
            let agent_type = if identity.agent_type_name.is_empty() {
                None
            } else {
                Some(identity.agent_type_name.as_str())
            };
            temper_authz::SecurityContext::from_verified_jwt(
                &identity.agent_instance_id,
                kind,
                agent_type,
                identity.acting_for.as_deref(),
                identity.role.as_deref(),
                verified_session,
            )
        } else {
            temper_authz::SecurityContext::from_resolved_identity(
                &identity.agent_instance_id,
                &identity.agent_type_name,
                verified_session,
            )
        };
        let authenticated =
            temper_authz::AuthenticatedRequestContext::new(tenant.clone(), security_context)
                .with_intent(intent)
                .with_session_id(session_id);

        req.extensions_mut().insert(authenticated);
        let route_resolves_its_own_credential = matched_endpoint
            .as_ref()
            .is_some_and(|matched| matched.route.forwards_credential);
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
        //
        // Unless the matched endpoint implements a protocol whose credential the
        // kernel cannot interpret and the app must resolve itself. Git
        // smart-HTTP presents a GitToken as HTTP Basic: opaque here, and the
        // only thing the guest can authenticate with. Removing it does not
        // withhold kernel authority — it withholds the app's own credential and
        // makes every authenticated push arrive anonymous.
        // Always, on this branch. Reaching it means the credential resolved to
        // a kernel identity, so it IS kernel authority and must not reach a
        // guest whatever the route declared. The forwarding exception exists
        // for credentials the kernel cannot interpret; this one it just did.
        //
        // There is an operational consequence worth saying out loud, because it
        // is not hypothetical: if one secret is registered BOTH as a GitToken's
        // HashedSecret and as an AgentCredential -- the state D7 found on
        // `gt-paw-agent` -- a push carrying it resolves here, is stripped, and
        // the guest sees an anonymous request. The push then fails for a reason
        // nothing in the git output explains. Say it here rather than leave the
        // next person to rediscover it from a 401.
        if route_resolves_its_own_credential && req.headers().contains_key("authorization") {
            tracing::warn!(
                tenant = %tenant,
                path = %req.uri().path(),
                "a protocol route that resolves its own credential received one the KERNEL \
                 resolved; it is kernel authority and is withheld from the guest. If this is a \
                 GitToken, its secret is also registered as an AgentCredential and the two \
                 identities must be separated, or the guest sees this request as anonymous"
            );
        }
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
        // Same exception on the public-route path, and this is the one git
        // actually takes: its endpoints declare RequiresAuth=false so the guest
        // can issue the smart-HTTP challenge itself.
        // Forward ONLY the credential formats the protocol actually uses, and
        // never `Bearer`.
        //
        // Failing to resolve a credential does not make it harmless. Resolution
        // is tenant-scoped, so a VALID kernel bearer for tenant A presented
        // with `X-Tenant-Id: B` fails to resolve and lands here still valid --
        // and forwarding "whatever did not resolve" would hand it to untrusted
        // WASM. Git smart-HTTP presents a GitToken as HTTP Basic and the
        // GitHub-compatible REST surface uses the `token` scheme; those are the
        // formats a guest is expected to resolve, so those are the only ones
        // that travel.
        if !matched.route.forwards_credential
            || !credential_is_forwardable_protocol_format(req.headers())
        {
            req.headers_mut().remove("authorization");
        }
        req.extensions_mut()
            .insert(temper_authz::AuthenticatedRequestContext::new(
                tenant.clone(),
                temper_authz::SecurityContext::anonymous(),
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

    // Last resort: a route that serves anonymous callers. Reached only after
    // credential resolution has already had its chance, so a caller who DID
    // present a valid credential took the authenticated branch above and the
    // handler sees their authority. One that presented none arrives here with
    // no `AuthenticatedRequestContext`, which is how the handler knows to apply
    // the anonymous rules instead.
    if temper_server::authz::allows_anonymous_fallback(req.method(), req.uri().path()) {
        req.headers_mut().remove("authorization");
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
            (&Method::GET, "/healthz")
                | (&Method::GET, "/version")
                | (&Method::POST, "/api/identity/resolve")
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
