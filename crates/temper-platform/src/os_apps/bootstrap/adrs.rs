//! ADR bootstrap into TemperFS under `/apps/{app-name}/adrs/`.

use temper_runtime::tenant::TenantId;

use super::fs::{
    APP_DOCS_ROOT_DIR_ID, APP_DOCS_ROOT_PATH, APP_DOCS_WORKSPACE_ID, DirectoryBootstrapTarget,
    MarkdownFileBootstrapTarget, ensure_app_docs_workspace, ensure_directory, ensure_markdown_file,
    slug_fragment,
};
use crate::os_apps::AdrEntry;
use crate::state::PlatformState;

/// Bootstrap app-local ADRs into TemperFS under `/apps/{app-name}/adrs/`.
pub(in crate::os_apps) async fn bootstrap_adrs(
    state: &PlatformState,
    tenant_id: &TenantId,
    tenant: &str,
    app_name: &str,
    adrs: &[AdrEntry],
) -> Vec<String> {
    if adrs.is_empty() {
        return Vec::new();
    }

    let has_workspace = {
        let registry = state.registry.read().unwrap(); // ci-ok: infallible lock
        registry.get_spec(tenant_id, "Workspace").is_some()
    };
    let has_directory = {
        let registry = state.registry.read().unwrap(); // ci-ok: infallible lock
        registry.get_spec(tenant_id, "Directory").is_some()
    };
    let has_file = {
        let registry = state.registry.read().unwrap(); // ci-ok: infallible lock
        registry.get_spec(tenant_id, "File").is_some()
    };
    if !(has_workspace && has_directory && has_file) {
        tracing::info!(
            tenant,
            app = %app_name,
            count = adrs.len(),
            "Skipping ADR bootstrap — TemperFS types not registered (install temper-fs first)"
        );
        return Vec::new();
    }

    let agent_ctx = temper_server::request_context::AgentContext::for_service("platform-bootstrap");
    if let Err(error) = ensure_app_docs_workspace(state, tenant_id, &agent_ctx).await {
        tracing::warn!(
            tenant,
            app = %app_name,
            error = %error,
            "Failed to ensure app docs workspace for ADR bootstrap"
        );
        return Vec::new();
    }

    let app_slug = slug_fragment(app_name);
    let app_dir_id = format!("os-app-docs-dir-{app_slug}");
    let app_dir_path = format!("{APP_DOCS_ROOT_PATH}/{app_name}");
    if let Err(error) = ensure_directory(
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
        tracing::warn!(
            tenant,
            app = %app_name,
            error = %error,
            "Failed to ensure app ADR directory"
        );
        return Vec::new();
    }

    let adrs_dir_id = format!("os-app-docs-dir-{app_slug}-adrs");
    let adrs_dir_path = format!("{app_dir_path}/adrs");
    if let Err(error) = ensure_directory(
        state,
        tenant_id,
        &agent_ctx,
        DirectoryBootstrapTarget {
            directory_id: &adrs_dir_id,
            name: "adrs",
            path: &adrs_dir_path,
            parent_id: Some(app_dir_id.as_str()),
            workspace_id: APP_DOCS_WORKSPACE_ID,
        },
    )
    .await
    {
        tracing::warn!(
            tenant,
            app = %app_name,
            error = %error,
            "Failed to ensure app ADR subdirectory"
        );
        return Vec::new();
    }

    let mut bootstrapped = Vec::new();
    for adr in adrs {
        let file_slug = slug_fragment(&adr.name);
        let file_id = format!("os-app-adr-{app_slug}-{file_slug}");
        let file_path = format!("{adrs_dir_path}/{}", adr.file_name);
        match ensure_markdown_file(
            state,
            tenant_id,
            &agent_ctx,
            MarkdownFileBootstrapTarget {
                file_id: &file_id,
                name: &adr.file_name,
                path: &file_path,
                directory_id: &adrs_dir_id,
                workspace_id: APP_DOCS_WORKSPACE_ID,
            },
            adr.content.as_bytes(),
        )
        .await
        {
            Ok(()) => bootstrapped.push(file_path),
            Err(error) => {
                tracing::warn!(
                    tenant,
                    app = %app_name,
                    adr = %adr.file_name,
                    error = %error,
                    "Failed to bootstrap ADR into TemperFS"
                );
            }
        }
    }

    bootstrapped
}
