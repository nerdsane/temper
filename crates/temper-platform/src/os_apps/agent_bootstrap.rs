use std::collections::BTreeMap;

use serde_json::json;
use temper_runtime::tenant::TenantId;

use crate::state::PlatformState;

use super::{AgentDefinition, content_sha256, state_field_str};

mod soul_refresh;

#[cfg(test)]
pub(super) use soul_refresh::decide_agent_soul_refresh;
pub(super) use soul_refresh::{AgentSoulRefreshDecision, bootstrapped_agent_soul_entity_id};
use soul_refresh::{inspect_agent_soul_refresh, slugify_bootstrapped_agent_name};

pub(super) async fn bootstrap_agents(
    state: &PlatformState,
    tenant_id: &TenantId,
    tenant: &str,
    agents: &[AgentDefinition],
    app_id: Option<&str>,
    agent_ctx: &temper_server::request_context::AgentContext,
) -> (Vec<String>, BTreeMap<String, String>) {
    let mut bootstrapped = Vec::new();
    let mut agent_uuid_map: BTreeMap<String, String> = BTreeMap::new();

    if agents.is_empty() {
        return (bootstrapped, agent_uuid_map);
    }

    let has_souls = {
        let registry = state.registry.read().unwrap(); // ci-ok: infallible lock
        registry.get_spec(tenant_id, "Soul").is_some()
    };
    if !has_souls {
        tracing::info!(
            tenant,
            count = agents.len(),
            "Skipping agent bootstrap — Soul entity type not registered (install paw-agent first)"
        );
        return (bootstrapped, agent_uuid_map);
    }

    let has_files = {
        let registry = state.registry.read().unwrap(); // ci-ok: infallible lock
        registry.get_spec(tenant_id, "File").is_some()
    };
    if !has_files {
        tracing::info!(
            tenant,
            "Skipping agent bootstrap — File entity type not registered (install temper-fs first)"
        );
        return (bootstrapped, agent_uuid_map);
    }

    let has_agents = {
        let registry = state.registry.read().unwrap(); // ci-ok: infallible lock
        registry.get_spec(tenant_id, "Agent").is_some()
    };

    for agent in agents {
        let stable_soul_id = bootstrapped_agent_soul_entity_id(&agent.name);

        if let Ok(resp) = state
            .server
            .get_tenant_entity_state_in_generation(tenant_id, "Soul", &stable_soul_id, agent_ctx)
            .await
        {
            if let Some(file_id) = resp
                .state
                .fields
                .get("ContentFileId")
                .or_else(|| resp.state.fields.get("content_file_id"))
                .and_then(|v| v.as_str())
            {
                let desired_hash = content_sha256(agent.content.as_bytes());
                match inspect_agent_soul_refresh(
                    state,
                    tenant_id,
                    file_id,
                    &desired_hash,
                    agent_ctx,
                )
                .await
                {
                    Ok(AgentSoulRefreshDecision::AlreadyCurrent) => {
                        tracing::debug!(
                            tenant,
                            agent = %agent.name,
                            soul_id = %stable_soul_id,
                            file_id = %file_id,
                            "Stable agent soul already matches app bundle"
                        );
                    }
                    Ok(AgentSoulRefreshDecision::PreserveCustomized) => {
                        tracing::info!(
                            tenant,
                            agent = %agent.name,
                            soul_id = %stable_soul_id,
                            file_id = %file_id,
                            "Preserving customized stable agent soul content"
                        );
                    }
                    Ok(AgentSoulRefreshDecision::Upload) => {
                        if let Err(error) = ensure_inline_file_uploaded(
                            state,
                            tenant_id,
                            agent_ctx,
                            file_id,
                            &format!("{}.soul.md", slugify_bootstrapped_agent_name(&agent.name)),
                            agent.content.as_bytes(),
                        )
                        .await
                        {
                            tracing::warn!(
                                tenant,
                                agent = %agent.name,
                                soul_id = %stable_soul_id,
                                file_id = %file_id,
                                error = %error,
                                "Failed to refresh stable agent soul content"
                            );
                        }
                    }
                    Err(error) => {
                        tracing::warn!(
                            tenant,
                            agent = %agent.name,
                            soul_id = %stable_soul_id,
                            file_id = %file_id,
                            error = %error,
                            "Failed to inspect stable agent soul content"
                        );
                    }
                }
            }

            agent_uuid_map.insert(agent.name.clone(), stable_soul_id.clone());
            bootstrapped.push(agent.name.clone());
            if has_agents && let Some(app_id) = app_id {
                set_agent_source_app(state, tenant_id, tenant, agent_ctx, &agent.name, app_id)
                    .await;
            }
            continue;
        }

        let existing_ids = state.server.list_entity_ids_lazy(tenant_id, "Soul").await;
        let mut found_soul_id: Option<String> = None;
        for id in &existing_ids {
            if let Ok(resp) = state
                .server
                .get_tenant_entity_state_in_generation(tenant_id, "Soul", id, agent_ctx)
                .await
                && let Some(name) = state_field_str(&resp.state.fields, &["Name", "name"])
                && name.eq_ignore_ascii_case(&agent.name)
            {
                if let Some(file_id) = resp
                    .state
                    .fields
                    .get("ContentFileId")
                    .or_else(|| resp.state.fields.get("content_file_id"))
                    .and_then(|v| v.as_str())
                {
                    let desired_hash = content_sha256(agent.content.as_bytes());
                    match inspect_agent_soul_refresh(
                        state,
                        tenant_id,
                        file_id,
                        &desired_hash,
                        agent_ctx,
                    )
                    .await
                    {
                        Ok(AgentSoulRefreshDecision::AlreadyCurrent) => {
                            tracing::debug!(
                                tenant,
                                agent = %agent.name,
                                file_id = %file_id,
                                "Existing agent soul content already matches app bundle"
                            );
                        }
                        Ok(AgentSoulRefreshDecision::PreserveCustomized) => {
                            tracing::info!(
                                tenant,
                                agent = %agent.name,
                                file_id = %file_id,
                                "Preserving customized existing agent soul content"
                            );
                        }
                        Ok(AgentSoulRefreshDecision::Upload) => {
                            if let Err(error) = ensure_inline_file_uploaded(
                                state,
                                tenant_id,
                                agent_ctx,
                                file_id,
                                &format!("{}.soul.md", agent.name.to_lowercase().replace(' ', "-")),
                                agent.content.as_bytes(),
                            )
                            .await
                            {
                                tracing::warn!(
                                    tenant,
                                    agent = %agent.name,
                                    file_id = %file_id,
                                    error = %error,
                                    "Failed to refresh existing agent soul content"
                                );
                            }
                        }
                        Err(error) => {
                            tracing::warn!(
                                tenant,
                                agent = %agent.name,
                                file_id = %file_id,
                                error = %error,
                                "Failed to inspect existing agent soul content"
                            );
                        }
                    }
                }
                tracing::debug!(tenant, agent = %agent.name, soul_id = %id, "Soul already exists — updating");
                found_soul_id = Some(id.clone());
                break;
            }
        }

        if let Some(soul_id) = found_soul_id {
            agent_uuid_map.insert(agent.name.clone(), soul_id.clone());
            bootstrapped.push(agent.name.clone());

            if has_agents && let Some(app_id) = app_id {
                set_agent_source_app(state, tenant_id, tenant, agent_ctx, &agent.name, app_id)
                    .await;
            }
            continue;
        }

        let file_name = format!("{}.soul.md", slugify_bootstrapped_agent_name(&agent.name));
        let temp_file_id = format!(
            "bootstrap-soul-file-{}",
            slugify_bootstrapped_agent_name(&agent.name)
        );
        if let Err(error) = ensure_inline_file_uploaded(
            state,
            tenant_id,
            agent_ctx,
            &temp_file_id,
            &file_name,
            agent.content.as_bytes(),
        )
        .await
        {
            tracing::warn!(
                tenant,
                agent = %agent.name,
                error = %error,
                "Failed to create TemperFS File for agent"
            );
            continue;
        }

        let new_soul_id = stable_soul_id;
        match state
            .server
            .get_or_create_tenant_entity_in_generation(
                tenant_id,
                "Soul",
                &new_soul_id,
                json!({}),
                agent_ctx,
            )
            .await
        {
            Ok(_) => {
                let soul_id = new_soul_id;
                if let Err(e) = state
                    .server
                    .dispatch(temper_server::state::DispatchCommand {
                        tenant: tenant_id,
                        entity_type: "Soul",
                        entity_id: &soul_id,
                        action: "Create",
                        params: json!({
                            "name": agent.name,
                            "description": agent.description,
                            "content_file_id": temp_file_id,
                        }),
                        agent_ctx,
                        await_integration: false,
                        await_reactions: true,
                    })
                    .await
                {
                    tracing::warn!(
                        tenant,
                        agent = %agent.name,
                        error = %e,
                        "Failed to register Soul entity"
                    );
                    continue;
                }
                let _ = state
                    .server
                    .dispatch(temper_server::state::DispatchCommand {
                        tenant: tenant_id,
                        entity_type: "Soul",
                        entity_id: &soul_id,
                        action: "Publish",
                        params: json!({}),
                        agent_ctx,
                        await_integration: false,
                        await_reactions: true,
                    })
                    .await;

                tracing::info!(tenant, agent = %agent.name, soul_id = %soul_id, "Agent soul bootstrapped");
                agent_uuid_map.insert(agent.name.clone(), soul_id);
                bootstrapped.push(agent.name.clone());

                if has_agents && let Some(app_id) = app_id {
                    set_agent_source_app(state, tenant_id, tenant, agent_ctx, &agent.name, app_id)
                        .await;
                }
            }
            Err(e) => {
                tracing::warn!(
                    tenant,
                    agent = %agent.name,
                    error = %e,
                    "Failed to create Soul entity"
                );
            }
        }
    }

    (bootstrapped, agent_uuid_map)
}

async fn set_agent_source_app(
    state: &PlatformState,
    tenant_id: &TenantId,
    tenant: &str,
    agent_ctx: &temper_server::request_context::AgentContext,
    agent_name: &str,
    app_id: &str,
) {
    let agent_ids = state.server.list_entity_ids_lazy(tenant_id, "Agent").await;
    for id in &agent_ids {
        if let Ok(resp) = state
            .server
            .get_tenant_entity_state_in_generation(tenant_id, "Agent", id, agent_ctx)
            .await
            && let Some(name) = state_field_str(&resp.state.fields, &["Name", "name"])
            && name.eq_ignore_ascii_case(agent_name)
        {
            let current_app_id = resp
                .state
                .fields
                .get("SourceAppId")
                .or_else(|| resp.state.fields.get("source_app_id"))
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            if current_app_id == app_id {
                return;
            }
            if let Err(e) = state
                .server
                .dispatch(temper_server::state::DispatchCommand {
                    tenant: tenant_id,
                    entity_type: "Agent",
                    entity_id: id,
                    action: "Configure",
                    params: json!({
                        "source_app_id": app_id,
                    }),
                    agent_ctx,
                    await_integration: false,
                    await_reactions: true,
                })
                .await
            {
                tracing::warn!(
                    tenant,
                    agent = %agent_name,
                    error = %e,
                    "Failed to set source_app_id on Agent entity"
                );
            } else {
                tracing::debug!(tenant, agent = %agent_name, app_id = %app_id, "Set source_app_id on Agent");
            }
            return;
        }
    }
    tracing::debug!(tenant, agent = %agent_name, "No Agent entity found to set source_app_id");
}

async fn ensure_inline_file_uploaded(
    state: &PlatformState,
    tenant_id: &TenantId,
    agent_ctx: &temper_server::request_context::AgentContext,
    file_id: &str,
    file_name: &str,
    content: &[u8],
) -> Result<(), String> {
    if !state
        .server
        .ensure_entity_loaded_in_generation(tenant_id, "File", file_id, agent_ctx)
        .await
    {
        state
            .server
            .create_file_with_initial_stream_content(
                tenant_id,
                file_id,
                json!({ "name": file_name, "path": "", "directory_id": "", "workspace_id": "", "mime_type": "text/markdown" }),
                content,
                "text/markdown",
                agent_ctx,
            )
            .await
            .map_err(|e| format!("failed to create File('{file_id}') with initial content: {e}"))?;
        return Ok(());
    }

    let response = state
        .server
        .get_tenant_entity_state_in_generation(tenant_id, "File", file_id, agent_ctx)
        .await
        .map_err(|e| format!("failed to load File('{file_id}') actor: {e}"))?;

    if response.state.status == "Created" {
        state
            .server
            .dispatch(temper_server::state::DispatchCommand {
                tenant: tenant_id,
                entity_type: "File",
                entity_id: file_id,
                action: "Create",
                params: json!({
                    "name": file_name,
                    "path": "",
                    "directory_id": "",
                    "workspace_id": "",
                    "mime_type": "text/markdown",
                }),
                agent_ctx,
                await_integration: false,
                await_reactions: true,
            })
            .await
            .map_err(|e| format!("failed to initialize File('{file_id}'): {e}"))?;
    }

    state
        .server
        .put_file_stream_content(tenant_id, file_id, content, "text/markdown", agent_ctx)
        .await
        .map_err(|e| format!("failed to upload File('{file_id}') content: {e}"))?;

    Ok(())
}
