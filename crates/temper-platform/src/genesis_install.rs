//! Genesis app installation bridge.
//!
//! Specs own the public action (`App.Install`). This hook only runs after that
//! governed action has succeeded, then materializes the pinned Genesis commit
//! into the platform's app installer.

use std::path::PathBuf;

use serde_json::Value;
use temper_runtime::tenant::TenantId;
use temper_server::state::{BoundActionHook, BoundActionHookContext, ServerState};

use crate::genesis_materialize::{
    GenesisAppBundle, mark_installation, materialize_app_closure, string_field,
};
use crate::os_apps::{add_os_apps_dir, install_os_app};
use crate::state::PlatformState;

pub struct GenesisInstallHook {
    platform: PlatformState,
}

impl GenesisInstallHook {
    pub fn new(platform: PlatformState) -> Self {
        Self { platform }
    }
}

/// Rebuild Genesis app materialization cache roots from durable Genesis rows.
///
/// This runs during server boot before persisted app installs are replayed. It
/// keeps recovery spec-first: `AppInstallation` rows point at pinned Genesis
/// `App`/`Commit`/`Tree`/`Blob` state, and the local OS-app catalog is rebuilt
/// from those objects instead of from GitHub, submodules, or arbitrary app dirs.
pub async fn restore_genesis_app_cache_roots(platform: &PlatformState) -> usize {
    let source_tenants = genesis_source_tenants();
    let mut restored = 0usize;

    for source_tenant in source_tenants {
        let tenant = TenantId::new(&source_tenant);
        let installation_ids = platform
            .server
            .list_entity_ids_lazy(&tenant, "AppInstallation")
            .await;
        for installation_id in installation_ids {
            let Ok(installation) = platform
                .server
                .get_tenant_entity_state(&tenant, "AppInstallation", &installation_id)
                .await
            else {
                continue;
            };
            if installation.state.status != "Installed" {
                continue;
            }
            let Some(app_id) = string_field(&installation.state.fields, "AppId") else {
                continue;
            };
            let Ok(app) = platform
                .server
                .get_tenant_entity_state(&tenant, "App", &app_id)
                .await
            else {
                continue;
            };

            let fields = &app.state.fields;
            let Some(name) = string_field(fields, "Name") else {
                continue;
            };
            let Some(owner) = string_field(fields, "OwnerId") else {
                continue;
            };
            let Some(repository_id) = string_field(fields, "RepositoryId") else {
                continue;
            };
            let version_hash = string_field(&installation.state.fields, "VersionHash")
                .or_else(|| string_field(fields, "LatestVersionHash"));
            let Some(version_hash) = version_hash else {
                continue;
            };
            let app_ref = string_field(&installation.state.fields, "AppRef").unwrap_or_else(|| {
                format!("{owner}/{name}@{}", version_hash.trim_start_matches('@'))
            });
            let cache_root = genesis_cache_root(&platform.server, &app_ref);
            let root = GenesisAppBundle {
                owner,
                name,
                repository_id,
                version_hash,
            };
            match materialize_app_closure(&platform.server, &tenant, &cache_root, root).await {
                Ok(_) => {
                    add_os_apps_dir(cache_root);
                    restored += 1;
                }
                Err(error) => {
                    tracing::warn!(
                        source_tenant = %source_tenant,
                        app_id = %app_id,
                        app_ref = %app_ref,
                        error = %error,
                        "Failed to restore Genesis app cache root"
                    );
                }
            }
        }
    }

    restored
}

#[async_trait::async_trait]
impl BoundActionHook for GenesisInstallHook {
    async fn after_bound_action(
        &self,
        ctx: BoundActionHookContext<'_>,
    ) -> Result<Option<Value>, String> {
        let BoundActionHookContext {
            state,
            tenant,
            entity_type,
            entity_id,
            action,
            params,
            state_json,
        } = ctx;

        if entity_type != "App" || action.rsplit('.').next().unwrap_or(action) != "Install" {
            return Ok(None);
        }

        let fields = state_json.get("fields").unwrap_or(state_json);
        let owner = string_field(fields, "OwnerId")
            .ok_or_else(|| "App.Install requires App.OwnerId".to_string())?;
        let name = string_field(fields, "Name")
            .ok_or_else(|| "App.Install requires App.Name".to_string())?;
        let repository_id = string_field(fields, "RepositoryId")
            .ok_or_else(|| "App.Install requires App.RepositoryId".to_string())?;
        let version_hash = string_field(fields, "LatestVersionHash")
            .ok_or_else(|| "App.Install requires App.LatestVersionHash".to_string())?;
        let target_tenant = string_field(params, "TargetTenant")
            .or_else(|| string_field(params, "tenant"))
            .unwrap_or_else(|| tenant.as_str().to_string());
        let app_ref = string_field(params, "AppRef")
            .unwrap_or_else(|| format!("{owner}/{name}@{}", version_hash.trim_start_matches('@')));
        let installation_id = installation_id(entity_id, &target_tenant, &version_hash);

        let cache_root = genesis_cache_root(state, &app_ref);
        let materialized_apps = materialize_app_closure(
            state,
            tenant,
            &cache_root,
            GenesisAppBundle {
                owner,
                name: name.clone(),
                repository_id,
                version_hash: version_hash.clone(),
            },
        )
        .await?;
        let app_dir = cache_root.join(&name);
        add_os_apps_dir(cache_root);

        let mut platform = self.platform.clone();
        platform.server = state.clone();
        match install_os_app(&platform, &target_tenant, &name).await {
            Ok(result) => {
                mark_installation(
                    state,
                    tenant,
                    &installation_id,
                    "MarkInstalled",
                    serde_json::json!({
                        "ClosureId": format!("genesis:{}:{}", app_ref, version_hash.trim_start_matches('@')),
                        "Message": format!(
                            "Installed {} into {} ({} added, {} updated, {} skipped)",
                            app_ref,
                            target_tenant,
                            result.added.len(),
                            result.updated.len(),
                            result.skipped.len()
                        ),
                        "InstalledAt": chrono::Utc::now().to_rfc3339(),
                    }),
                )
                .await;
                Ok(Some(serde_json::json!({
                    "kind": "genesis_app_install",
                    "appRef": app_ref,
                    "targetTenant": target_tenant,
                    "installationId": installation_id,
                    "materializedPath": app_dir,
                    "materializedApps": materialized_apps,
                    "added": result.added,
                    "updated": result.updated,
                    "skipped": result.skipped,
                })))
            }
            Err(error) => {
                let message = error.to_string();
                mark_installation(
                    state,
                    tenant,
                    &installation_id,
                    "MarkFailed",
                    serde_json::json!({
                        "Message": message,
                        "InstalledAt": chrono::Utc::now().to_rfc3339(),
                    }),
                )
                .await;
                Err(format!("Genesis App.Install failed for {app_ref}: {error}"))
            }
        }
    }
}

fn genesis_cache_root(state: &ServerState, app_ref: &str) -> PathBuf {
    let root = if state.data_dir.as_os_str().is_empty() {
        std::env::temp_dir().join("temper-genesis-app-cache")
    } else {
        state.data_dir.join("genesis-app-cache")
    };
    root.join(sanitize_fragment(app_ref))
}

fn genesis_source_tenants() -> Vec<String> {
    let configured = std::env::var("TEMPER_GENESIS_SOURCE_TENANTS").unwrap_or_default();
    let mut tenants: Vec<String> = configured
        .split(',')
        .map(str::trim)
        .filter(|tenant| !tenant.is_empty())
        .map(ToString::to_string)
        .collect();
    if tenants.is_empty() {
        tenants.push("default".to_string());
    }
    tenants.sort();
    tenants.dedup();
    tenants
}

fn installation_id(app_id: &str, tenant: &str, version_hash: &str) -> String {
    format!(
        "ai-{}-{}-{}",
        sanitize_fragment(app_id),
        sanitize_fragment(tenant),
        sanitize_fragment(version_hash)
            .chars()
            .take(16)
            .collect::<String>()
    )
}

fn sanitize_fragment(input: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in input.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "item".to_string()
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_ids_and_cache_fragments_are_stable() {
        assert_eq!(
            installation_id("app-Acme Notes", "tenant/a", "@abcdef0123456789"),
            "ai-app-acme-notes-tenant-a-abcdef0123456789"
        );
        assert_eq!(sanitize_fragment("../"), "item");
    }

    #[test]
    fn source_tenants_default_to_default() {
        assert!(genesis_source_tenants().contains(&"default".to_string()));
    }
}
