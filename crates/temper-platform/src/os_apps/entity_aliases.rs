use sha2::{Digest, Sha256};
use temper_runtime::tenant::TenantId;

use crate::state::PlatformState;

pub(super) async fn ensure_created_file_initialized(
    state: &PlatformState,
    tenant_id: &TenantId,
    agent_ctx: &temper_server::request_context::AgentContext,
    file_id: &str,
    create_fields: serde_json::Value,
) -> Result<(), String> {
    let response = state
        .server
        .get_tenant_entity_state_in_generation(tenant_id, "File", file_id, agent_ctx)
        .await
        .map_err(|e| format!("failed to inspect File('{file_id}') status: {e}"))?;
    if response.state.status != "Created" {
        return Ok(());
    }

    state
        .server
        .dispatch(temper_server::state::DispatchCommand {
            tenant: tenant_id,
            entity_type: "File",
            entity_id: file_id,
            action: "Create",
            params: create_fields,
            agent_ctx,
            await_integration: false,
            await_reactions: true,
        })
        .await
        .map(|_| ())
        .map_err(|e| format!("failed to initialize File('{file_id}'): {e}"))
}

pub(super) async fn ensure_entity_field_aliases(
    state: &PlatformState,
    tenant_id: &TenantId,
    entity_type: &str,
    entity_id: &str,
    aliases: serde_json::Value,
    agent_ctx: &temper_server::request_context::AgentContext,
) -> Result<(), String> {
    let current = state
        .server
        .get_tenant_entity_state_in_generation(tenant_id, entity_type, entity_id, agent_ctx)
        .await
        .map_err(|e| format!("failed to inspect {entity_type}('{entity_id}') aliases: {e}"))?;

    let Some(alias_map) = aliases.as_object() else {
        return Ok(());
    };
    let mut updates = serde_json::Map::new();
    for (key, expected) in alias_map {
        if current.state.fields.get(key) != Some(expected) {
            updates.insert(key.clone(), expected.clone());
        }
    }
    if updates.is_empty() {
        return Ok(());
    }

    state
        .server
        .update_tenant_entity_fields_in_generation(
            tenant_id,
            entity_type,
            entity_id,
            serde_json::Value::Object(updates),
            false,
            None,
            agent_ctx,
        )
        .await
        .map(|_| ())
        .map_err(|e| format!("failed to repair {entity_type}('{entity_id}') aliases: {e}"))
}

pub(super) async fn file_already_contains(
    state: &PlatformState,
    tenant_id: &TenantId,
    file_id: &str,
    desired_hash: &str,
    agent_ctx: &temper_server::request_context::AgentContext,
) -> Result<bool, String> {
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
    Ok(has_content && current_hash == desired_hash)
}

pub(super) fn content_sha256(content: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content);
    format!("sha256:{:x}", hasher.finalize())
}

pub(super) fn slug_fragment(value: &str) -> String {
    let mut slug = String::new();
    let mut last_was_sep = true;
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            last_was_sep = false;
        } else if !last_was_sep {
            slug.push('-');
            last_was_sep = true;
        }
    }
    slug.trim_matches('-').to_string()
}
