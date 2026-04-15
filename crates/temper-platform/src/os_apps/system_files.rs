//! System file discovery and TemperFS bootstrap.
//!
//! Files under `os-apps/{app}/system/` are bootstrapped into TemperFS under
//! `/system/{relative_path}` during app install. These are not skills (not
//! unconditionally injected per ADR-003) — they're loaded on demand by WASM
//! modules (e.g. plan-mode instructions by llm_caller).

use std::path::Path;

use temper_runtime::tenant::TenantId;

use super::{
    APP_DOCS_ROOT_DIR_ID, APP_DOCS_WORKSPACE_ID, DirectoryBootstrapTarget,
    MarkdownFileBootstrapTarget, ensure_directory, ensure_markdown_file, slug_fragment,
};
use crate::state::PlatformState;

/// A system file discovered from `system/` directory tree (e.g. mode-instructions).
/// Bootstrapped into TemperFS under `/system/{relative_path}`.
#[derive(Debug, Clone)]
pub struct SystemFileEntry {
    pub relative_path: String,
    pub file_name: String,
    pub content: Vec<u8>,
    pub mime_type: String,
}

/// Discover system files from the `system/` directory tree.
/// Recursively walks the directory and collects all files.
pub fn find_system_files(app_dir: &Path) -> Vec<SystemFileEntry> {
    let system_dir = app_dir.join("system");
    if !system_dir.is_dir() {
        return Vec::new();
    }
    let mut results = Vec::new();
    collect_recursive(&system_dir, &system_dir, &mut results);
    results.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    results
}

fn collect_recursive(base_dir: &Path, current_dir: &Path, results: &mut Vec<SystemFileEntry>) {
    let Ok(entries) = std::fs::read_dir(current_dir) else {
        return;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_dir() {
            collect_recursive(base_dir, &path, results);
        } else if path.is_file() {
            let Ok(content) = std::fs::read(&path) else {
                continue;
            };
            let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let relative = path
                .strip_prefix(base_dir)
                .unwrap_or(&path)
                .to_string_lossy()
                .to_string();
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            let mime_type = match ext {
                "md" => "text/markdown",
                "json" => "application/json",
                "toml" => "application/toml",
                "yaml" | "yml" => "text/yaml",
                "txt" => "text/plain",
                _ => "application/octet-stream",
            }
            .to_string();
            results.push(SystemFileEntry {
                relative_path: relative,
                file_name: file_name.to_string(),
                content,
                mime_type,
            });
        }
    }
}

/// Bootstrap system files into TemperFS under `/system/{relative_path}`.
pub async fn bootstrap_system_files(
    state: &PlatformState,
    tenant_id: &TenantId,
    tenant: &str,
    system_files: &[SystemFileEntry],
) {
    if system_files.is_empty() {
        return;
    }

    let has_fs = {
        let registry = state.registry.read().expect("infallible registry lock");
        registry.get_spec(tenant_id, "File").is_some()
            && registry.get_spec(tenant_id, "Directory").is_some()
    };
    if !has_fs {
        return;
    }

    let agent_ctx = temper_server::request_context::AgentContext::system();
    if let Err(e) = super::ensure_app_docs_workspace(state, tenant_id, &agent_ctx).await {
        tracing::warn!(tenant, error = %e, "Failed to ensure workspace for system file bootstrap");
        return;
    }

    // Ensure /system/ root exists.
    if let Err(e) = ensure_directory(
        state,
        tenant_id,
        &agent_ctx,
        DirectoryBootstrapTarget {
            directory_id: "os-system-root",
            name: "system",
            path: "/system",
            parent_id: Some(APP_DOCS_ROOT_DIR_ID),
            workspace_id: APP_DOCS_WORKSPACE_ID,
        },
    )
    .await
    {
        tracing::warn!(tenant, error = %e, "Failed to create /system/ directory for system files");
        return;
    }

    // Collect unique subdirectories from relative paths and ensure they exist.
    let mut ensured_dirs = std::collections::HashSet::new();
    for entry in system_files {
        if let Some(parent) = Path::new(&entry.relative_path).parent() {
            let parent_str = parent.to_string_lossy().to_string();
            if !parent_str.is_empty() && ensured_dirs.insert(parent_str.clone()) {
                let dir_slug = slug_fragment(&parent_str.replace('/', "-"));
                let dir_id = format!("os-system-{dir_slug}");
                let dir_path = format!("/system/{parent_str}");
                if let Err(e) = ensure_directory(
                    state,
                    tenant_id,
                    &agent_ctx,
                    DirectoryBootstrapTarget {
                        directory_id: &dir_id,
                        name: &parent_str,
                        path: &dir_path,
                        parent_id: Some("os-system-root"),
                        workspace_id: APP_DOCS_WORKSPACE_ID,
                    },
                )
                .await
                {
                    tracing::warn!(tenant, dir = %dir_path, error = %e, "Failed to create system file directory");
                    continue;
                }
            }
        }
    }

    // Bootstrap each file.
    for entry in system_files {
        let file_path = format!("/system/{}", entry.relative_path);
        let parent_dir = Path::new(&entry.relative_path)
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let dir_slug = slug_fragment(&parent_dir.replace('/', "-"));
        let dir_id = if parent_dir.is_empty() {
            "os-system-root".to_string()
        } else {
            format!("os-system-{dir_slug}")
        };
        let file_slug = slug_fragment(&entry.relative_path.replace('/', "-"));
        let file_id = format!("os-system-file-{file_slug}");
        match ensure_markdown_file(
            state,
            tenant_id,
            &agent_ctx,
            MarkdownFileBootstrapTarget {
                file_id: &file_id,
                name: &entry.file_name,
                path: &file_path,
                directory_id: &dir_id,
                workspace_id: APP_DOCS_WORKSPACE_ID,
            },
            &entry.content,
        )
        .await
        {
            Ok(()) => {
                tracing::info!(tenant, path = %file_path, "Bootstrapped system file");
            }
            Err(error) => {
                tracing::warn!(
                    tenant,
                    path = %file_path,
                    error = %error,
                    "Failed to bootstrap system file"
                );
            }
        }
    }
}
