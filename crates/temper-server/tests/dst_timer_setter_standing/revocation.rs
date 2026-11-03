//! Signing a user out everywhere and revoking an agent's grant, as the
//! platform does: by advancing the generation keyed by the user or the grant.

use serde_json::json;
use temper_runtime::tenant::TenantId;
use temper_server::request_context::AgentContext;

/// The grant `user`'s agent acts under.
pub(super) fn grant(user: &str) -> String {
    format!("grant-{user}")
}

/// Advance the sign-out generation keyed by `key`, as signing out everywhere
/// or revoking a grant does.
pub(super) async fn bump_generation(
    state: &temper_server::ServerState,
    tenant: &TenantId,
    key: &str,
) -> Result<(), String> {
    let response = state
        .dispatch_tenant_action(
            tenant,
            "PrincipalGeneration",
            key,
            "BumpGeneration",
            json!({}),
            &AgentContext::system(),
        )
        .await?;
    if !response.success {
        return Err(format!("{key}: {}", response.error.unwrap_or_default()));
    }
    Ok(())
}
