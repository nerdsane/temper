//! Tenant access validation for authenticated requests.

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;
use temper_authz::{AuthenticatedRequestContext, PrincipalKind};

use crate::state::PlatformState;

/// Validate that the credential-bound tenant is the tenant addressed by the
/// request, then apply routed-storage membership checks for GitHub users.
pub async fn tenant_access_check(
    State(state): State<PlatformState>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let Some(authenticated) = req
        .extensions()
        .get::<AuthenticatedRequestContext>()
        .cloned()
    else {
        // The outer bearer edge permits only exact public routes without a
        // context. It remains authoritative for that classification.
        return Ok(next.run(req).await);
    };

    let credential_tenant = authenticated.tenant().as_str();
    if let Some(path_tenant) = tenant_from_path(req.uri().path())
        && path_tenant != credential_tenant
    {
        return Err(StatusCode::FORBIDDEN);
    }

    let principal = &authenticated.security_context().principal;
    if principal.kind == PrincipalKind::Agent || !principal.id.starts_with("github:") {
        return Ok(next.run(req).await);
    }

    let Some(provider) = state
        .server
        .storage_stack
        .as_ref()
        .and_then(|stack| stack.turso.clone())
    else {
        return Ok(next.run(req).await);
    };
    if !provider.supports_tenant_admin() {
        return Ok(next.run(req).await);
    }

    match provider.tenants_for_user(&principal.id).await {
        Ok(user_tenants)
            if user_tenants
                .iter()
                .any(|tenant| tenant.tenant_id == credential_tenant) =>
        {
            Ok(next.run(req).await)
        }
        Ok(_) => Err(StatusCode::FORBIDDEN),
        Err(error) => {
            tracing::error!(
                principal = %principal.id,
                tenant = credential_tenant,
                error = %error,
                "failed to check tenant access"
            );
            Err(StatusCode::SERVICE_UNAVAILABLE)
        }
    }
}

fn tenant_from_path(path: &str) -> Option<&str> {
    let rest = path.strip_prefix("/api/tenants/")?;
    rest.split('/').next().filter(|tenant| !tenant.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_only_explicit_tenant_path_segments() {
        assert_eq!(tenant_from_path("/api/tenants/acme/specs"), Some("acme"));
        assert_eq!(tenant_from_path("/api/tenants/"), None);
        assert_eq!(tenant_from_path("/tdata/Orders"), None);
    }
}
