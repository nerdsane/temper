//! Identity resolution HTTP endpoint.
//!
//! `POST /api/identity/resolve` — resolves a bearer token to a verified
//! agent identity. Used by the MCP server at startup to resolve its
//! credential once and cache the result for the session lifetime.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use temper_runtime::tenant::TenantId;

use super::ResolvedIdentity;
use crate::state::ServerState;

/// Request body for identity resolution.
#[derive(serde::Deserialize)]
pub struct ResolveRequest {
    /// The bearer token to resolve.
    pub bearer_token: String,
    /// Tenant scope for credential lookup.
    #[serde(default = "default_tenant")]
    pub tenant: String,
}

fn default_tenant() -> String {
    "default".to_string()
}

/// Response body for identity resolution.
#[derive(serde::Serialize)]
pub struct ResolveResponse {
    /// Platform-assigned agent instance ID.
    pub agent_instance_id: String,
    /// The AgentType entity ID.
    pub agent_type_id: String,
    /// The AgentType's human-readable name.
    pub agent_type_name: String,
    /// Whether the identity was verified via the credential registry.
    pub verified: bool,
}

impl From<ResolvedIdentity> for ResolveResponse {
    fn from(id: ResolvedIdentity) -> Self {
        Self {
            agent_instance_id: id.agent_instance_id,
            agent_type_id: id.agent_type_id,
            agent_type_name: id.agent_type_name,
            verified: id.verified,
        }
    }
}

/// Handle `POST /api/identity/resolve`.
///
/// Resolves a bearer token to a verified agent identity by looking up
/// the `AgentCredential` entity and its linked `AgentType`.
pub async fn handle_identity_resolve(
    State(state): State<ServerState>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<ResolveRequest>,
) -> impl IntoResponse {
    // Extract tenant from header or body.
    let tenant_str = headers
        .get("x-tenant-id")
        .and_then(|v| v.to_str().ok())
        .map(String::from)
        .unwrap_or(body.tenant);
    let tenant = TenantId::new(&tenant_str);

    // Use the server-level identity resolver (no cache layer here — the
    // MCP server caches the result for the session lifetime).
    let resolver = super::IdentityResolver::new();
    match resolver.resolve(&state, &tenant, &body.bearer_token).await {
        Some(identity) => {
            let response: ResolveResponse = identity.into();
            (StatusCode::OK, axum::Json(response)).into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            axum::Json(serde_json::json!({
                "error": "Credential not found or inactive"
            })),
        )
            .into_response(),
    }
}
