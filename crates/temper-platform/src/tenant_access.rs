//! Tenant access validation middleware.
//!
//! Defense-in-depth layer that verifies `github:*` principals have access
//! to the requested tenant. Agent principals and requests without identity
//! headers pass through (backward compatibility for local dev / MCP).

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;

use crate::state::PlatformState;

/// Extract the tenant ID from the request.
///
/// Checks `X-Tenant-Id` header first, then falls back to URL path inspection.
fn extract_tenant(req: &Request) -> Option<String> {
    // Check header first.
    if let Some(val) = req.headers().get("x-tenant-id") {
        return val.to_str().ok().map(|s| s.to_string());
    }

    // Check URL path for /api/tenants/:id patterns.
    let path = req.uri().path();
    if let Some(rest) = path.strip_prefix("/api/tenants/") {
        let tenant_id = rest.split('/').next().unwrap_or("");
        if !tenant_id.is_empty() {
            return Some(tenant_id.to_string());
        }
    }

    None
}

/// Axum middleware that validates tenant access for `github:*` principals.
///
/// Passthrough rules (no access check):
/// - No `X-Temper-Principal-Id` header (local dev / backward compat)
/// - `X-Temper-Principal-Kind` is `agent` (trusted backend-to-backend)
/// - Principal doesn't start with `github:` (non-human principal)
/// - No tenant could be extracted from the request
/// - Tenant is `default` or `temper-system` (always accessible)
/// - Not in TenantRouted mode (single-DB has no per-tenant access control)
pub async fn tenant_access_check(
    State(state): State<PlatformState>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    // No identity header → passthrough (local dev).
    let Some(principal_id) = req
        .headers()
        .get("x-temper-principal-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
    else {
        return Ok(next.run(req).await);
    };

    // Agent principals pass through (trusted backend-to-backend).
    let kind = req
        .headers()
        .get("x-temper-principal-kind")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if kind == "agent" {
        return Ok(next.run(req).await);
    }

    // Only validate github:* principals.
    if !principal_id.starts_with("github:") {
        return Ok(next.run(req).await);
    }

    // Extract tenant from request.
    let Some(tenant_id) = extract_tenant(&req) else {
        return Ok(next.run(req).await);
    };

    // Always-accessible tenants.
    if tenant_id == "default" || tenant_id == "temper-system" {
        return Ok(next.run(req).await);
    }

    // Check tenant access via the router.
    let Some(ref store) = state.server.event_store else {
        return Ok(next.run(req).await);
    };
    let Some(router) = store.tenant_router() else {
        // Not in routed mode — no per-tenant access control.
        return Ok(next.run(req).await);
    };

    match router.tenants_for_user(&principal_id).await {
        Ok(user_tenants) => {
            if user_tenants.iter().any(|t| t.tenant_id == tenant_id) {
                Ok(next.run(req).await)
            } else {
                Err(StatusCode::FORBIDDEN)
            }
        }
        Err(_) => {
            // DB error — fail open for availability (log it).
            tracing::warn!(
                principal = %principal_id,
                tenant = %tenant_id,
                "failed to check tenant access, allowing request"
            );
            Ok(next.run(req).await)
        }
    }
}
