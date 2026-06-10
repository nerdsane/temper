//! TemperFS bootstrap plumbing shared by the install-time bootstrap steps.
//!
//! Idempotently ensures the app docs workspace, directories, and markdown
//! files exist, plus small shared helpers (slugs, content hashing,
//! state-field access).

use sha2::{Digest, Sha256};
use temper_runtime::tenant::TenantId;

use crate::state::PlatformState;

pub(in crate::os_apps) const APP_DOCS_WORKSPACE_ID: &str = "os-app-docs";
const APP_DOCS_WORKSPACE_NAME: &str = "apps";
pub(in crate::os_apps) const APP_DOCS_ROOT_DIR_ID: &str = "os-app-docs-root";
pub(super) const APP_DOCS_ROOT_PATH: &str = "/apps";
const APP_DOCS_QUOTA_BYTES: i64 = 1_099_511_627_776;

pub(in crate::os_apps) fn state_field_str<'a>(
    fields: &'a serde_json::Value,
    keys: &[&str],
) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| fields.get(*key).and_then(|value| value.as_str()))
}

pub(in crate::os_apps) async fn ensure_app_docs_workspace(
    state: &PlatformState,
    tenant_id: &TenantId,
    agent_ctx: &temper_server::request_context::AgentContext,
) -> Result<(), String> {
    if !state
        .server
        .ensure_entity_loaded(tenant_id, "Workspace", APP_DOCS_WORKSPACE_ID)
        .await
    {
        state
            .server
            .get_or_create_tenant_entity(
                tenant_id,
                "Workspace",
                APP_DOCS_WORKSPACE_ID,
                serde_json::json!({}),
            )
            .await
            .map_err(|e| format!("failed to create app docs workspace entity: {e}"))?;
        state
            .server
            .dispatch(temper_server::state::DispatchCommand {
                tenant: tenant_id,
                entity_type: "Workspace",
                entity_id: APP_DOCS_WORKSPACE_ID,
                action: "Create",
                params: serde_json::json!({
                    "name": APP_DOCS_WORKSPACE_NAME,
                    "quota_limit": APP_DOCS_QUOTA_BYTES,
                }),
                agent_ctx,
                await_integration: false,
                await_reactions: true,
            })
            .await
            .map_err(|e| format!("failed to initialize app docs workspace: {e}"))?;
    }

    ensure_directory(
        state,
        tenant_id,
        agent_ctx,
        DirectoryBootstrapTarget {
            directory_id: APP_DOCS_ROOT_DIR_ID,
            name: APP_DOCS_WORKSPACE_NAME,
            path: APP_DOCS_ROOT_PATH,
            parent_id: None,
            workspace_id: APP_DOCS_WORKSPACE_ID,
        },
    )
    .await
}

pub(in crate::os_apps) struct DirectoryBootstrapTarget<'a> {
    pub(in crate::os_apps) directory_id: &'a str,
    pub(in crate::os_apps) name: &'a str,
    pub(in crate::os_apps) path: &'a str,
    pub(in crate::os_apps) parent_id: Option<&'a str>,
    pub(in crate::os_apps) workspace_id: &'a str,
}

pub(in crate::os_apps) async fn ensure_directory(
    state: &PlatformState,
    tenant_id: &TenantId,
    agent_ctx: &temper_server::request_context::AgentContext,
    target: DirectoryBootstrapTarget<'_>,
) -> Result<(), String> {
    if state
        .server
        .ensure_entity_loaded(tenant_id, "Directory", target.directory_id)
        .await
    {
        return Ok(());
    }

    state
        .server
        .get_or_create_tenant_entity(
            tenant_id,
            "Directory",
            target.directory_id,
            serde_json::json!({}),
        )
        .await
        .map_err(|e| {
            format!(
                "failed to create Directory('{}') actor: {e}",
                target.directory_id
            )
        })?;
    state
        .server
        .dispatch(temper_server::state::DispatchCommand {
            tenant: tenant_id,
            entity_type: "Directory",
            entity_id: target.directory_id,
            action: "Create",
            params: serde_json::json!({
                "name": target.name,
                "path": target.path,
                "parent_id": target.parent_id,
                "workspace_id": target.workspace_id,
            }),
            agent_ctx,
            await_integration: false,
            await_reactions: true,
        })
        .await
        .map_err(|e| {
            format!(
                "failed to initialize Directory('{}'): {e}",
                target.directory_id
            )
        })?;

    if let Some(parent_id) = target.parent_id {
        state
            .server
            .dispatch(temper_server::state::DispatchCommand {
                tenant: tenant_id,
                entity_type: "Directory",
                entity_id: parent_id,
                action: "AddChild",
                params: serde_json::json!({}),
                agent_ctx,
                await_integration: false,
                await_reactions: true,
            })
            .await
            .map_err(|e| {
                format!(
                    "failed to register child directory '{}': {e}",
                    target.directory_id
                )
            })?;
    }

    Ok(())
}

pub(in crate::os_apps) struct MarkdownFileBootstrapTarget<'a> {
    pub(in crate::os_apps) file_id: &'a str,
    pub(in crate::os_apps) name: &'a str,
    pub(in crate::os_apps) path: &'a str,
    pub(in crate::os_apps) directory_id: &'a str,
    pub(in crate::os_apps) workspace_id: &'a str,
}

pub(in crate::os_apps) async fn ensure_markdown_file(
    state: &PlatformState,
    tenant_id: &TenantId,
    agent_ctx: &temper_server::request_context::AgentContext,
    target: MarkdownFileBootstrapTarget<'_>,
    content: &[u8],
) -> Result<(), String> {
    let existed = state
        .server
        .ensure_entity_loaded(tenant_id, "File", target.file_id)
        .await;
    if !existed {
        state
            .server
            .get_or_create_tenant_entity(tenant_id, "File", target.file_id, serde_json::json!({}))
            .await
            .map_err(|e| format!("failed to create File('{}') actor: {e}", target.file_id))?;
        state
            .server
            .dispatch(temper_server::state::DispatchCommand {
                tenant: tenant_id,
                entity_type: "File",
                entity_id: target.file_id,
                action: "Create",
                params: serde_json::json!({
                    "name": target.name,
                    "path": target.path,
                    "directory_id": target.directory_id,
                    "workspace_id": target.workspace_id,
                    "mime_type": "text/markdown",
                }),
                agent_ctx,
                await_integration: false,
                await_reactions: true,
            })
            .await
            .map_err(|e| format!("failed to initialize File('{}'): {e}", target.file_id))?;
        state
            .server
            .dispatch(temper_server::state::DispatchCommand {
                tenant: tenant_id,
                entity_type: "Directory",
                entity_id: target.directory_id,
                action: "AddChild",
                params: serde_json::json!({}),
                agent_ctx,
                await_integration: false,
                await_reactions: true,
            })
            .await
            .map_err(|e| {
                format!(
                    "failed to register file '{}' with parent directory: {e}",
                    target.file_id
                )
            })?;
        state
            .server
            .dispatch(temper_server::state::DispatchCommand {
                tenant: tenant_id,
                entity_type: "Workspace",
                entity_id: target.workspace_id,
                action: "IncrementFileCount",
                params: serde_json::json!({}),
                agent_ctx,
                await_integration: false,
                await_reactions: true,
            })
            .await
            .map_err(|e| {
                format!(
                    "failed to increment file count for workspace '{}': {e}",
                    target.workspace_id
                )
            })?;
    }

    let desired_hash = content_sha256(content);
    if file_already_contains(state, tenant_id, target.file_id, &desired_hash).await? {
        return Ok(());
    }

    state
        .server
        .put_file_stream_content(
            tenant_id,
            target.file_id,
            content,
            "text/markdown",
            agent_ctx,
        )
        .await
        .map_err(|e| format!("failed to upload File('{}') content: {e}", target.file_id))?;

    Ok(())
}

async fn file_already_contains(
    state: &PlatformState,
    tenant_id: &TenantId,
    file_id: &str,
    desired_hash: &str,
) -> Result<bool, String> {
    let response = state
        .server
        .get_tenant_entity_state(tenant_id, "File", file_id)
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

pub(in crate::os_apps) fn content_sha256(content: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content);
    format!("sha256:{:x}", hasher.finalize())
}

pub(in crate::os_apps) fn slug_fragment(value: &str) -> String {
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
