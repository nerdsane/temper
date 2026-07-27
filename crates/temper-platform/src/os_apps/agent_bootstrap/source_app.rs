use serde_json::json;
use temper_runtime::tenant::TenantId;

use crate::state::PlatformState;

use super::state_field_str;

pub(super) async fn set_agent_source_app(
    state: &PlatformState,
    tenant_id: &TenantId,
    tenant: &str,
    agent_ctx: &temper_server::request_context::AgentContext,
    agent_name: &str,
    app_id: &str,
) {
    let agent_ids = match state.server.list_entity_ids_lazy(tenant_id, "Agent").await {
        Ok(ids) => ids,
        Err(error) => {
            tracing::error!(
                tenant,
                agent = %agent_name,
                error = %error,
                "failed to enumerate Agent entities while linking source app"
            );
            return;
        }
    };
    for id in &agent_ids {
        if let Ok(resp) = state
            .server
            .get_tenant_entity_state(tenant_id, "Agent", id)
            .await
            && let Some(name) = state_field_str(&resp.state.fields, &["Name", "name"])
            && name.eq_ignore_ascii_case(agent_name)
        {
            let current_app_id = resp
                .state
                .fields
                .get("SourceAppId")
                .or_else(|| resp.state.fields.get("source_app_id"))
                .and_then(|value| value.as_str())
                .unwrap_or_default();
            if current_app_id == app_id {
                return;
            }
            let result = state
                .server
                .dispatch(temper_server::state::DispatchCommand {
                    tenant: tenant_id,
                    entity_type: "Agent",
                    entity_id: id,
                    action: "Configure",
                    params: json!({ "source_app_id": app_id }),
                    agent_ctx,
                    await_integration: false,
                    await_reactions: true,
                })
                .await;
            match result {
                Ok(_) => tracing::debug!(
                    tenant,
                    agent = %agent_name,
                    app_id,
                    "Set source_app_id on Agent"
                ),
                Err(error) => tracing::warn!(
                    tenant,
                    agent = %agent_name,
                    error = %error,
                    "Failed to set source_app_id on Agent entity"
                ),
            }
            return;
        }
    }
    tracing::debug!(tenant, agent = %agent_name, "No Agent entity found to set source_app_id");
}
