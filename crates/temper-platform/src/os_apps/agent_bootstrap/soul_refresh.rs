use temper_runtime::tenant::TenantId;

use crate::state::PlatformState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::os_apps) enum AgentSoulRefreshDecision {
    Upload,
    AlreadyCurrent,
    PreserveCustomized,
}

pub(super) fn slugify_bootstrapped_agent_name(name: &str) -> String {
    name.trim().to_lowercase().replace(' ', "-")
}

pub(in crate::os_apps) fn bootstrapped_agent_soul_entity_id(name: &str) -> String {
    format!(
        "sl-bootstrap-agent-soul-{}",
        slugify_bootstrapped_agent_name(name)
    )
}

pub(super) async fn inspect_agent_soul_refresh(
    state: &PlatformState,
    tenant_id: &TenantId,
    file_id: &str,
    desired_hash: &str,
    agent_ctx: &temper_server::request_context::AgentContext,
) -> Result<AgentSoulRefreshDecision, String> {
    let response = state
        .server
        .get_tenant_entity_state_in_generation(tenant_id, "File", file_id, agent_ctx)
        .await
        .map_err(|e| format!("failed to inspect File('{file_id}'): {e}"))?;

    let has_content = response
        .state
        .booleans
        .get("has_content")
        .copied()
        .unwrap_or(false);
    let current_hash = response
        .state
        .fields
        .get("content_hash")
        .and_then(|value| value.as_str())
        .unwrap_or_default();

    Ok(decide_agent_soul_refresh(
        has_content,
        current_hash,
        desired_hash,
    ))
}

pub(in crate::os_apps) fn decide_agent_soul_refresh(
    has_content: bool,
    current_hash: &str,
    desired_hash: &str,
) -> AgentSoulRefreshDecision {
    if !has_content || current_hash.is_empty() {
        AgentSoulRefreshDecision::Upload
    } else if current_hash == desired_hash {
        AgentSoulRefreshDecision::AlreadyCurrent
    } else {
        AgentSoulRefreshDecision::PreserveCustomized
    }
}
