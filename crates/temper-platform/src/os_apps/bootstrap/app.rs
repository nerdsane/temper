//! App entity bootstrap for installed OS apps.

use temper_runtime::tenant::TenantId;

use super::fs::{
    APP_DOCS_ROOT_DIR_ID, APP_DOCS_ROOT_PATH, APP_DOCS_WORKSPACE_ID, DirectoryBootstrapTarget,
    MarkdownFileBootstrapTarget, ensure_app_docs_workspace, ensure_directory, ensure_markdown_file,
    slug_fragment, state_field_str,
};
use crate::os_apps::catalog;
use crate::state::PlatformState;

/// Bootstrap an App entity for the installed OS app.
///
/// Creates (or updates) an App entity and writes APP.md to TemperFS.
/// Returns the App entity ID if successful.
pub(in crate::os_apps) async fn bootstrap_app_entity(
    state: &PlatformState,
    tenant_id: &TenantId,
    tenant: &str,
    app_name: &str,
) -> Option<String> {
    // Check if App entity type is registered.
    let has_apps = {
        let registry = state.registry.read().unwrap(); // ci-ok: infallible lock
        registry.get_spec(tenant_id, "App").is_some()
    };
    if !has_apps {
        tracing::debug!(
            tenant,
            app = %app_name,
            "Skipping App entity bootstrap — App entity type not registered"
        );
        return None;
    }

    let agent_ctx = temper_server::request_context::AgentContext::for_service("platform-bootstrap");

    // Look for an existing App entity with this name.
    let existing_ids = state.server.list_entity_ids_lazy(tenant_id, "App").await;
    let mut existing_app_id = None;
    for id in &existing_ids {
        if let Ok(resp) = state
            .server
            .get_tenant_entity_state(tenant_id, "App", id)
            .await
            && let Some(name) = state_field_str(&resp.state.fields, &["Name", "name"])
            && name.eq_ignore_ascii_case(app_name)
        {
            existing_app_id = Some(id.clone());
            break;
        }
    }

    // Read app manifest for metadata.
    let manifest = {
        let cat = catalog().read().unwrap(); // ci-ok: infallible lock
        cat.entries.iter().find(|e| e.name == app_name).cloned()
    };
    let description = manifest
        .as_ref()
        .map(|m| m.description.clone())
        .unwrap_or_default();
    let version = manifest
        .as_ref()
        .map(|m| m.version.clone())
        .unwrap_or_else(|| "0.1.0".to_string());

    // Bootstrap APP.md into TemperFS at /apps/{app-name}/APP.md.
    let mut app_guide_file_id = String::new();
    if let Some(guide) = manifest.as_ref().and_then(|m| m.app_guide.as_ref()) {
        // Ensure the app docs workspace and /apps/{app-name}/ directory exist.
        let has_fs = {
            let registry = state.registry.read().unwrap(); // ci-ok: infallible lock
            registry.get_spec(tenant_id, "File").is_some()
                && registry.get_spec(tenant_id, "Directory").is_some()
                && registry.get_spec(tenant_id, "Workspace").is_some()
        };
        if has_fs {
            if let Err(e) = ensure_app_docs_workspace(state, tenant_id, &agent_ctx).await {
                tracing::warn!(tenant, app = %app_name, error = %e, "Failed to ensure app docs workspace");
            } else {
                let app_slug = slug_fragment(app_name);
                let app_dir_id = format!("os-app-docs-dir-{app_slug}");
                let app_dir_path = format!("{APP_DOCS_ROOT_PATH}/{app_name}");
                if let Err(e) = ensure_directory(
                    state,
                    tenant_id,
                    &agent_ctx,
                    DirectoryBootstrapTarget {
                        directory_id: &app_dir_id,
                        name: app_name,
                        path: &app_dir_path,
                        parent_id: Some(APP_DOCS_ROOT_DIR_ID),
                        workspace_id: APP_DOCS_WORKSPACE_ID,
                    },
                )
                .await
                {
                    tracing::warn!(tenant, app = %app_name, error = %e, "Failed to create app directory for APP.md");
                } else {
                    let file_id_str = format!("os-app-guide-{app_slug}");
                    let file_path = format!("{app_dir_path}/APP.md");
                    if let Err(e) = ensure_markdown_file(
                        state,
                        tenant_id,
                        &agent_ctx,
                        MarkdownFileBootstrapTarget {
                            file_id: &file_id_str,
                            name: "APP.md",
                            path: &file_path,
                            directory_id: &app_dir_id,
                            workspace_id: APP_DOCS_WORKSPACE_ID,
                        },
                        guide.as_bytes(),
                    )
                    .await
                    {
                        tracing::warn!(tenant, app = %app_name, error = %e, "Failed to bootstrap APP.md");
                    } else {
                        app_guide_file_id = file_id_str;
                        tracing::info!(tenant, app = %app_name, path = %file_path, "APP.md bootstrapped into TemperFS");
                    }
                }
            }
        }
    }

    if let Some(ref id) = existing_app_id {
        // Update existing App entity.
        let _ = state
            .server
            .dispatch(temper_server::state::DispatchCommand {
                tenant: tenant_id,
                entity_type: "App",
                entity_id: id,
                action: "Install",
                params: serde_json::json!({
                    "name": app_name,
                    "description": description,
                    "version": version,
                    "app_guide_file_id": app_guide_file_id,
                }),
                agent_ctx: &agent_ctx,
                await_integration: false,
                await_reactions: true,
            })
            .await;
        tracing::info!(tenant, app = %app_name, id = %id, "App entity updated");
        return existing_app_id;
    }

    // Create new App entity with a prefixed UUID.
    let new_app_id = format!("ap-{}", temper_runtime::scheduler::sim_uuid());
    match state
        .server
        .get_or_create_tenant_entity(tenant_id, "App", &new_app_id, serde_json::json!({}))
        .await
    {
        Ok(_) => {
            let app_id = new_app_id;
            // Initialize with Install action.
            if let Err(e) = state
                .server
                .dispatch(temper_server::state::DispatchCommand {
                    tenant: tenant_id,
                    entity_type: "App",
                    entity_id: &app_id,
                    action: "Install",
                    params: serde_json::json!({
                        "name": app_name,
                        "description": description,
                        "version": version,
                        "app_guide_file_id": app_guide_file_id,
                    }),
                    agent_ctx: &agent_ctx,
                    await_integration: false,
                    await_reactions: true,
                })
                .await
            {
                tracing::warn!(
                    tenant,
                    app = %app_name,
                    error = %e,
                    "Failed to initialize App entity"
                );
                return None;
            }
            tracing::info!(tenant, app = %app_name, id = %app_id, "App entity created");
            Some(app_id)
        }
        Err(e) => {
            tracing::warn!(
                tenant,
                app = %app_name,
                error = %e,
                "Failed to create App entity"
            );
            None
        }
    }
}
