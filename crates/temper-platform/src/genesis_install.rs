//! Genesis app installation bridge.
//!
//! Specs own the public action (`App.Install`). This hook only runs after that
//! governed action has succeeded, then materializes the pinned Genesis commit
//! into the platform's app installer.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use temper_runtime::tenant::TenantId;
use temper_server::platform_store::InstalledAppRecord;
use temper_server::state::{BoundActionHook, BoundActionHookContext, DispatchCommand, ServerState};

use crate::os_apps::{
    AppManifest, add_os_apps_dir_preferred, install_os_app, os_app_bundle_digest,
};
use crate::state::PlatformState;

const FIELD_OVERFLOW_REF_KEY: &str = "__temper_blob_ref";

#[derive(Debug, Clone, Deserialize)]
pub struct GenesisRegistryInstallRequest {
    pub tenant: String,
    pub app_ref: String,
    #[serde(default)]
    pub registry_url: String,
    #[serde(default)]
    pub registry_tenant: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct GenesisRegistryInstallResult {
    pub app_ref: String,
    pub tenant: String,
    pub registry_url: String,
    pub registry_tenant: String,
    pub closure_id: String,
    pub materialized_path: String,
    pub materialized_apps: Vec<String>,
    pub added: Vec<String>,
    pub updated: Vec<String>,
    pub skipped: Vec<String>,
    pub wasm_modules: Vec<String>,
    pub agents: Vec<String>,
    pub agent_skills: Vec<String>,
    pub adrs: Vec<String>,
    pub seed_instances: Vec<String>,
}

#[derive(Debug, Clone)]
struct RegistryAppRef {
    owner: String,
    name: String,
    version_hash: Option<String>,
}

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
                    add_os_apps_dir_preferred(cache_root);
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

/// Rebuild local cache roots for apps that were installed from a remote Genesis
/// registry into this Temper instance.
///
/// These rows live in the target instance's durable installed-app table. They
/// are distinct from Genesis service-side `AppInstallation` rows above. On
/// restart, recovering the pinned cache roots first lets the normal runtime
/// recovery/reconcile path validate digests without re-dispatching
/// spec-owned `App.Install` or rerunning seed data for unchanged refs.
pub async fn restore_genesis_registry_cache_roots(platform: &PlatformState) -> usize {
    let Some(ps) = platform
        .server
        .storage_stack
        .as_ref()
        .and_then(|stack| stack.platform.clone())
    else {
        return 0;
    };

    let installed = match ps.list_all_installed_apps().await {
        Ok(installed) => installed,
        Err(error) => {
            tracing::warn!(error = %error, "Failed to list installed apps for Genesis cache recovery");
            return 0;
        }
    };

    let mut restored = 0usize;
    let mut seen = BTreeSet::new();
    for (tenant, app_name) in installed {
        let record = match ps.get_installed_app(&tenant, &app_name).await {
            Ok(Some(record)) => record,
            Ok(None) => continue,
            Err(error) => {
                tracing::warn!(
                    tenant = %tenant,
                    app = %app_name,
                    error = %error,
                    "Failed to read installed app metadata for Genesis cache recovery"
                );
                continue;
            }
        };
        if record.source_kind != "genesis" || record.registry_url.trim().is_empty() {
            continue;
        }
        let seen_key = if record.closure_id.trim().is_empty() {
            record.app_ref.clone()
        } else {
            record.closure_id.clone()
        };
        if !seen.insert(seen_key) {
            continue;
        }
        let root_ref = match parse_registry_app_ref(&record.app_ref) {
            Ok(root_ref) => root_ref,
            Err(error) => {
                tracing::warn!(
                    tenant = %tenant,
                    app = %app_name,
                    app_ref = %record.app_ref,
                    error = %error,
                    "Installed app has invalid Genesis app_ref"
                );
                continue;
            }
        };
        let cache_root = genesis_cache_root(&platform.server, &record.app_ref);
        match materialize_registry_app_closure(&record.registry_url, root_ref, &cache_root).await {
            Ok(_) => {
                add_os_apps_dir_preferred(cache_root);
                restored += 1;
            }
            Err(error) => {
                tracing::warn!(
                    tenant = %tenant,
                    app = %app_name,
                    app_ref = %record.app_ref,
                    error = %error,
                    "Failed to restore Genesis registry app cache root"
                );
            }
        }
    }

    restored
}

/// Install a pinned Genesis app ref into this Temper instance from a registry URL.
///
/// This is the local-instance counterpart to spec-owned `App.Install`: agent,
/// CLI, and admin clients call one semantic install operation, while this
/// helper materializes the pinned Git-native Genesis closure into the local
/// app catalog and then runs the normal Temper installer against this
/// instance's storage backend.
pub async fn install_genesis_app_from_registry(
    platform: &PlatformState,
    request: GenesisRegistryInstallRequest,
) -> Result<GenesisRegistryInstallResult, String> {
    let registry_url = normalize_registry_url(&request.registry_url)?;
    let registry_tenant = if request.registry_tenant.trim().is_empty() {
        "default".to_string()
    } else {
        request.registry_tenant.trim().to_string()
    };
    let root_ref = parse_registry_app_ref(&request.app_ref)?;
    let root_hash = root_ref
        .version_hash
        .clone()
        .ok_or_else(|| "Genesis app install requires a pinned owner/app@hash ref".to_string())?;

    let cache_key = format!(
        "{}/{}@{}",
        root_ref.owner,
        root_ref.name,
        root_hash.trim_start_matches('@')
    );
    let cache_root = genesis_cache_root(&platform.server, &cache_key);
    std::fs::create_dir_all(&cache_root).map_err(|error| {
        format!(
            "create Genesis registry cache '{}': {error}",
            cache_root.display()
        )
    })?;

    let materialized_refs =
        materialize_registry_app_closure(&registry_url, root_ref.clone(), &cache_root).await?;
    let materialized: Vec<String> = materialized_refs
        .iter()
        .map(|app_ref| app_ref.name.clone())
        .collect();

    add_os_apps_dir_preferred(cache_root.clone());

    let install_platform = platform.clone();
    let install = install_os_app(&install_platform, &request.tenant, &root_ref.name).await?;
    let root_closure_id = format!(
        "genesis:{}:{}",
        request.app_ref,
        root_hash.trim_start_matches('@')
    );

    for materialized_ref in &materialized_refs {
        let Some(version_hash) = materialized_ref.version_hash.as_deref() else {
            continue;
        };
        let app_ref = format!(
            "{}/{}@{}",
            materialized_ref.owner,
            materialized_ref.name,
            version_hash.trim_start_matches('@')
        );
        let closure_id =
            if materialized_ref.owner == root_ref.owner && materialized_ref.name == root_ref.name {
                root_closure_id.clone()
            } else {
                format!(
                    "genesis:{}:{}",
                    app_ref,
                    version_hash.trim_start_matches('@')
                )
            };
        record_genesis_install_metadata(
            &install_platform,
            GenesisInstallMetadata {
                target_tenant: &request.tenant,
                app_name: &materialized_ref.name,
                app_ref: &app_ref,
                version_hash,
                closure_id: &closure_id,
                registry_url: &registry_url,
                registry_tenant: &registry_tenant,
            },
        )
        .await;
    }

    Ok(GenesisRegistryInstallResult {
        app_ref: request.app_ref,
        tenant: request.tenant,
        registry_url,
        registry_tenant,
        closure_id: root_closure_id,
        materialized_path: cache_root.display().to_string(),
        materialized_apps: materialized,
        added: install.added,
        updated: install.updated,
        skipped: install.skipped,
        wasm_modules: install.wasm_modules,
        agents: install.agents,
        agent_skills: install.skills,
        adrs: install.adrs_bootstrapped,
        seed_instances: install.seed_instances,
    })
}

fn normalize_registry_url(raw: &str) -> Result<String, String> {
    let fallback = std::env::var("TEMPER_GENESIS_REGISTRY_URL").unwrap_or_default();
    let raw = if raw.trim().is_empty() {
        fallback.as_str()
    } else {
        raw
    };
    let value = raw.trim().trim_end_matches('/');
    if value.is_empty() {
        return Err("registry_url is required for Genesis app install".to_string());
    }
    if !(value.starts_with("http://") || value.starts_with("https://")) {
        return Err("registry_url must start with http:// or https://".to_string());
    }
    Ok(value.to_string())
}

fn parse_registry_app_ref(app_ref: &str) -> Result<RegistryAppRef, String> {
    let trimmed = app_ref.trim();
    let (owner_and_name, version_hash) = trimmed
        .split_once('@')
        .map(|(left, right)| (left, Some(right.trim_start_matches('@').to_string())))
        .unwrap_or((trimmed, None));
    let (owner, name) = owner_and_name
        .split_once('/')
        .ok_or_else(|| "Genesis app ref must be owner/name@hash".to_string())?;
    let owner = owner.trim();
    let name = name.trim();
    if owner.is_empty() || name.is_empty() {
        return Err("Genesis app ref must include non-empty owner and app name".to_string());
    }
    let version_hash = match version_hash {
        Some(hash) if hash.trim().is_empty() => {
            return Err("Genesis app ref hash must not be empty".to_string());
        }
        Some(hash) => Some(hash.trim().to_string()),
        None => None,
    };
    Ok(RegistryAppRef {
        owner: owner.to_string(),
        name: name.to_string(),
        version_hash,
    })
}

async fn materialize_git_registry_app(
    registry_url: &str,
    owner: &str,
    name: &str,
    version_hash: Option<&str>,
    app_dir: &Path,
) -> Result<String, String> {
    let remote = registry_git_url(registry_url, owner, name);
    let git_dir = app_dir.join(".git");
    if app_dir.exists() && !git_dir.is_dir() {
        std::fs::remove_dir_all(app_dir).map_err(|error| {
            format!(
                "remove stale Genesis app cache '{}': {error}",
                app_dir.display()
            )
        })?;
    }
    if !git_dir.is_dir() {
        if let Some(parent) = app_dir.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                format!(
                    "create Genesis app cache parent '{}': {error}",
                    parent.display()
                )
            })?;
        }
        let app_dir_arg = app_dir.display().to_string();
        run_git(None, &["clone", &remote, &app_dir_arg]).await?;
    } else {
        run_git(Some(app_dir), &["remote", "set-url", "origin", &remote]).await?;
        run_git(Some(app_dir), &["fetch", "origin", "--tags", "--prune"]).await?;
    }

    if let Some(hash) = version_hash {
        run_git(Some(app_dir), &["checkout", "--detach", hash]).await?;
    }

    let resolved = run_git(Some(app_dir), &["rev-parse", "HEAD"]).await?;
    let resolved = resolved.trim();
    if resolved.is_empty() {
        Err(format!(
            "git rev-parse returned an empty hash for {owner}/{name}"
        ))
    } else {
        Ok(resolved.to_string())
    }
}

async fn materialize_registry_app_closure(
    registry_url: &str,
    root_ref: RegistryAppRef,
    cache_root: &Path,
) -> Result<Vec<RegistryAppRef>, String> {
    let mut stack = vec![root_ref];
    let mut seen = BTreeSet::new();
    let mut materialized_refs = Vec::new();

    while let Some(app_ref) = stack.pop() {
        let key = format!("{}/{}", app_ref.owner, app_ref.name);
        if !seen.insert(key) {
            continue;
        }

        let app_dir = cache_root.join(&app_ref.name);
        let resolved_hash = materialize_git_registry_app(
            registry_url,
            &app_ref.owner,
            &app_ref.name,
            app_ref.version_hash.as_deref(),
            &app_dir,
        )
        .await?;
        materialized_refs.push(RegistryAppRef {
            owner: app_ref.owner.clone(),
            name: app_ref.name.clone(),
            version_hash: Some(resolved_hash),
        });

        for dependency in read_manifest_dependencies(&app_dir)?.into_iter().rev() {
            let dependency = parse_dependency_ref(&dependency, &app_ref.owner);
            stack.push(RegistryAppRef {
                owner: dependency.owner.unwrap_or_else(|| app_ref.owner.clone()),
                name: dependency.name,
                version_hash: dependency.version_hash,
            });
        }
    }

    Ok(materialized_refs)
}

fn registry_git_url(registry_url: &str, owner: &str, name: &str) -> String {
    format!(
        "{}/{}/{}.git",
        registry_url.trim_end_matches('/'),
        owner,
        name
    )
}

async fn run_git(cwd: Option<&Path>, args: &[&str]) -> Result<String, String> {
    let mut command = tokio::process::Command::new("git");
    command.args(args);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let output = command
        .output()
        .await
        .map_err(|error| format!("failed to run git {}: {error}", args.join(" ")))?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).to_string());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    Err(format!(
        "git {} failed with status {}: {}{}{}",
        args.join(" "),
        output.status,
        stderr.trim(),
        if stderr.trim().is_empty() || stdout.trim().is_empty() {
            ""
        } else {
            "\n"
        },
        stdout.trim()
    ))
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
        let registry_url = string_field(params, "RegistryUrl")
            .or_else(|| string_field(params, "registry_url"))
            .unwrap_or_default();
        let registry_tenant = string_field(params, "RegistryTenant")
            .or_else(|| string_field(params, "registry_tenant"))
            .unwrap_or_else(|| tenant.as_str().to_string());
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
        add_os_apps_dir_preferred(cache_root);

        let mut platform = self.platform.clone();
        platform.server = state.clone();
        match install_os_app(&platform, &target_tenant, &name).await {
            Ok(result) => {
                let closure_id = format!(
                    "genesis:{}:{}",
                    app_ref,
                    version_hash.trim_start_matches('@')
                );
                record_genesis_install_metadata(
                    &platform,
                    GenesisInstallMetadata {
                        target_tenant: &target_tenant,
                        app_name: &name,
                        app_ref: &app_ref,
                        version_hash: &version_hash,
                        closure_id: &closure_id,
                        registry_url: &registry_url,
                        registry_tenant: &registry_tenant,
                    },
                )
                .await;
                mark_installation(
                    state,
                    tenant,
                    &installation_id,
                    "MarkInstalled",
                    serde_json::json!({
                        "ClosureId": closure_id,
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
                    "wasmModules": result.wasm_modules,
                    "agents": result.agents,
                    "agentSkills": result.skills,
                    "adrs": result.adrs_bootstrapped,
                    "seedInstances": result.seed_instances,
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

struct GenesisInstallMetadata<'a> {
    target_tenant: &'a str,
    app_name: &'a str,
    app_ref: &'a str,
    version_hash: &'a str,
    closure_id: &'a str,
    registry_url: &'a str,
    registry_tenant: &'a str,
}

async fn record_genesis_install_metadata(
    platform: &PlatformState,
    metadata: GenesisInstallMetadata<'_>,
) {
    let Some(ps) = platform
        .server
        .storage_stack
        .as_ref()
        .and_then(|stack| stack.platform.clone())
    else {
        return;
    };
    let Some(digest) = os_app_bundle_digest(metadata.app_name) else {
        tracing::warn!(
            tenant = %metadata.target_tenant,
            app = %metadata.app_name,
            app_ref = %metadata.app_ref,
            "Installed Genesis app but could not compute bundle digest for durable provenance"
        );
        return;
    };

    let record = InstalledAppRecord {
        tenant: metadata.target_tenant.to_string(),
        app_name: digest.app_name,
        source_kind: "genesis".to_string(),
        app_ref: metadata.app_ref.to_string(),
        version_hash: metadata.version_hash.trim_start_matches('@').to_string(),
        closure_id: metadata.closure_id.to_string(),
        registry_url: metadata.registry_url.to_string(),
        registry_tenant: metadata.registry_tenant.to_string(),
        app_version: digest.app_version,
        bundle_digest: digest.bundle_digest,
        spec_digest: digest.spec_digest,
        policy_digest: digest.policy_digest,
        wasm_digest: digest.wasm_digest,
        content_digest: digest.content_digest,
        seed_digest: digest.seed_digest,
        installed_at: None,
        last_reconciled_at: None,
        status: "installed".to_string(),
    };

    if let Err(error) = ps.record_installed_app_metadata(&record).await {
        tracing::warn!(
            tenant = %metadata.target_tenant,
            app = %metadata.app_name,
            app_ref = %metadata.app_ref,
            error = %error,
            "Failed to persist Genesis app provenance"
        );
    }
}

#[derive(Debug, Clone)]
struct GenesisAppBundle {
    owner: String,
    name: String,
    repository_id: String,
    version_hash: String,
}

async fn materialize_app_closure(
    state: &ServerState,
    tenant: &TenantId,
    cache_root: &Path,
    root: GenesisAppBundle,
) -> Result<Vec<String>, String> {
    let mut stack = vec![root];
    let mut seen = BTreeSet::new();
    let mut materialized = Vec::new();

    while let Some(app) = stack.pop() {
        if !seen.insert(app.name.clone()) {
            continue;
        }

        let app_dir = cache_root.join(&app.name);
        materialize_commit_tree(
            state,
            tenant,
            &app.repository_id,
            &app.version_hash,
            &app_dir,
        )
        .await?;
        materialized.push(app.name.clone());

        for dependency in read_manifest_dependencies(&app_dir)?.into_iter().rev() {
            let dependency = resolve_genesis_dependency(state, tenant, &app.owner, &dependency)
                .await
                .map_err(|error| {
                    format!(
                        "resolve dependency '{}' for Genesis app '{}': {error}",
                        dependency, app.name
                    )
                })?;
            if !seen.contains(&dependency.name) {
                stack.push(dependency);
            }
        }
    }

    Ok(materialized)
}

fn read_manifest_dependencies(app_dir: &Path) -> Result<Vec<String>, String> {
    let path = app_dir.join("app.toml");
    let content = std::fs::read_to_string(&path)
        .map_err(|error| format!("read Genesis app manifest '{}': {error}", path.display()))?;
    let manifest: AppManifest = toml::from_str(&content)
        .map_err(|error| format!("parse Genesis app manifest '{}': {error}", path.display()))?;
    Ok(manifest.dependencies)
}

async fn resolve_genesis_dependency(
    state: &ServerState,
    tenant: &TenantId,
    preferred_owner: &str,
    dependency: &str,
) -> Result<GenesisAppBundle, String> {
    let requested = parse_dependency_ref(dependency, preferred_owner);
    let ids = state.list_entity_ids_lazy(tenant, "App").await;
    let mut matches = Vec::new();

    for entity_id in ids {
        let candidate = state
            .get_tenant_entity_state(tenant, "App", &entity_id)
            .await
            .map_err(|error| format!("read Genesis App {entity_id}: {error}"))?;
        if candidate.state.status != "Active" {
            continue;
        }
        let fields = &candidate.state.fields;
        let Some(name) = string_field(fields, "Name") else {
            continue;
        };
        if name != requested.name {
            continue;
        }
        let Some(owner) = string_field(fields, "OwnerId") else {
            continue;
        };
        if let Some(requested_owner) = requested.owner.as_deref()
            && owner != requested_owner
        {
            continue;
        }
        let Some(repository_id) = string_field(fields, "RepositoryId") else {
            continue;
        };
        let version_hash = requested
            .version_hash
            .clone()
            .or_else(|| string_field(fields, "LatestVersionHash"))
            .ok_or_else(|| format!("Genesis App {entity_id} is missing LatestVersionHash"))?;
        matches.push(GenesisAppBundle {
            owner,
            name,
            repository_id,
            version_hash,
        });
    }

    if matches.len() == 1 {
        return Ok(matches.remove(0));
    }
    if matches.is_empty() {
        return Err(format!(
            "no active Genesis App row found for '{}'",
            dependency
        ));
    }

    matches
        .into_iter()
        .find(|app| app.owner == preferred_owner)
        .ok_or_else(|| format!("multiple Genesis App rows match '{}'", dependency))
}

#[derive(Debug, PartialEq, Eq)]
struct DependencyRef {
    owner: Option<String>,
    name: String,
    version_hash: Option<String>,
}

fn parse_dependency_ref(input: &str, preferred_owner: &str) -> DependencyRef {
    let trimmed = input.trim();
    let (owner_and_name, version_hash) = trimmed
        .split_once('@')
        .map(|(left, right)| (left, Some(right.trim_start_matches('@').to_string())))
        .unwrap_or((trimmed, None));
    let (owner, name) = owner_and_name
        .split_once('/')
        .map(|(owner, name)| (Some(owner.to_string()), name.to_string()))
        .unwrap_or_else(|| {
            let owner = if preferred_owner.is_empty() {
                None
            } else {
                Some(preferred_owner.to_string())
            };
            (owner, owner_and_name.to_string())
        });

    DependencyRef {
        owner,
        name,
        version_hash,
    }
}

async fn mark_installation(
    state: &ServerState,
    tenant: &TenantId,
    installation_id: &str,
    action: &str,
    params: Value,
) {
    let agent_ctx = temper_server::request_context::AgentContext::for_service("genesis-install");
    let _ = state
        .dispatch(DispatchCommand {
            tenant,
            entity_type: "AppInstallation",
            entity_id: installation_id,
            action,
            params,
            agent_ctx: &agent_ctx,
            await_integration: false,
            await_reactions: true,
        })
        .await;
}

async fn materialize_commit_tree(
    state: &ServerState,
    tenant: &TenantId,
    repository_id: &str,
    version_hash: &str,
    app_dir: &Path,
) -> Result<(), String> {
    let commit_id = version_hash.trim_start_matches('@');
    let commit = load_genesis_object(state, tenant, "Commit", repository_id, commit_id)
        .await?
        .ok_or_else(|| format!("Genesis commit {commit_id} not found for {repository_id}"))?;
    let tree_sha = string_field(&commit.state.fields, "TreeSha")
        .ok_or_else(|| format!("Genesis commit {commit_id} is missing TreeSha"))?;

    if app_dir.exists() {
        std::fs::remove_dir_all(app_dir)
            .map_err(|e| format!("clear Genesis app cache '{}': {e}", app_dir.display()))?;
    }
    std::fs::create_dir_all(app_dir)
        .map_err(|e| format!("create Genesis app cache '{}': {e}", app_dir.display()))?;
    materialize_tree(state, tenant, repository_id, &tree_sha, app_dir).await
}

async fn materialize_tree(
    state: &ServerState,
    tenant: &TenantId,
    repository_id: &str,
    tree_sha: &str,
    dir: &Path,
) -> Result<(), String> {
    let mut stack = vec![(tree_sha.to_string(), dir.to_path_buf())];
    while let Some((current_tree, current_dir)) = stack.pop() {
        std::fs::create_dir_all(&current_dir)
            .map_err(|e| format!("create directory '{}': {e}", current_dir.display()))?;
        let tree = load_genesis_object(state, tenant, "Tree", repository_id, &current_tree)
            .await?
            .ok_or_else(|| format!("Genesis tree {current_tree} not found for {repository_id}"))?;
        let canonical = string_field_resolved(state, tenant, &tree.state.fields, "CanonicalBytes")
            .await?
            .ok_or_else(|| format!("Genesis tree {current_tree} is missing CanonicalBytes"))?;
        for entry in parse_tree_entries(&decode_git_object_body(&canonical, "tree")?)? {
            validate_tree_entry_name(&entry.name)?;
            let path = current_dir.join(&entry.name);
            if entry.is_tree() {
                stack.push((entry.object_sha, path));
                continue;
            }
            let blob = load_genesis_object(state, tenant, "Blob", repository_id, &entry.object_sha)
                .await?
                .ok_or_else(|| {
                    format!(
                        "Genesis blob {} not found for {}",
                        entry.object_sha, repository_id
                    )
                })?;
            let blob_repository = string_field(&blob.state.fields, "RepositoryId")
                .unwrap_or_else(|| repository_id.to_string());
            if blob_repository != repository_id {
                return Err(format!(
                    "blob {} belongs to repository {}, expected {}",
                    entry.object_sha, blob_repository, repository_id
                ));
            }
            let content = string_field_resolved(state, tenant, &blob.state.fields, "Content")
                .await?
                .ok_or_else(|| format!("Genesis blob {} is missing Content", entry.object_sha))?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("create directory '{}': {e}", parent.display()))?;
            }
            std::fs::write(&path, decode_blob_content(&content))
                .map_err(|e| format!("write Genesis app file '{}': {e}", path.display()))?;
        }
    }
    Ok(())
}

async fn load_genesis_object(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    repository_id: &str,
    git_sha: &str,
) -> Result<Option<temper_server::EntityResponse>, String> {
    if state.entity_exists(tenant, entity_type, git_sha)
        && let Ok(found) = state
            .get_tenant_entity_state(tenant, entity_type, git_sha)
            .await
    {
        let fields = &found.state.fields;
        let object_repo = string_field(fields, "RepositoryId").unwrap_or_default();
        let object_sha = string_field(fields, "Id").unwrap_or_default();
        if object_repo == repository_id && object_sha == git_sha {
            return Ok(Some(found));
        }
    }

    let ids = state.list_entity_ids_lazy(tenant, entity_type).await;
    for entity_id in ids {
        let candidate = state
            .get_tenant_entity_state(tenant, entity_type, &entity_id)
            .await
            .map_err(|e| format!("read Genesis {entity_type} {entity_id}: {e}"))?;
        let fields = &candidate.state.fields;
        let object_repo = string_field(fields, "RepositoryId").unwrap_or_default();
        let object_sha = string_field(fields, "Id").unwrap_or_default();
        if object_repo == repository_id && object_sha == git_sha {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

#[derive(Debug)]
struct TreeEntry {
    mode: String,
    name: String,
    object_sha: String,
}

impl TreeEntry {
    fn is_tree(&self) -> bool {
        self.mode == "40000" || self.mode == "040000"
    }
}

fn parse_tree_entries(body: &[u8]) -> Result<Vec<TreeEntry>, String> {
    let mut entries = Vec::new();
    let mut offset = 0usize;
    while offset < body.len() {
        let mode_start = offset;
        while offset < body.len() && body[offset] != b' ' {
            offset += 1;
        }
        if offset >= body.len() {
            return Err("malformed tree entry mode".to_string());
        }
        let mode = std::str::from_utf8(&body[mode_start..offset])
            .map_err(|e| format!("tree mode is not UTF-8: {e}"))?
            .to_string();
        offset += 1;

        let name_start = offset;
        while offset < body.len() && body[offset] != 0 {
            offset += 1;
        }
        if offset >= body.len() {
            return Err("malformed tree entry name".to_string());
        }
        let name = std::str::from_utf8(&body[name_start..offset])
            .map_err(|e| format!("tree path is not UTF-8: {e}"))?
            .to_string();
        offset += 1;

        if offset + 20 > body.len() {
            return Err("malformed tree entry object id".to_string());
        }
        let object_sha = body[offset..offset + 20]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        offset += 20;
        entries.push(TreeEntry {
            mode,
            name,
            object_sha,
        });
    }
    Ok(entries)
}

fn validate_tree_entry_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\\') {
        return Err(format!("unsafe Genesis tree entry path '{name}'"));
    }
    Ok(())
}

fn decode_blob_content(value: &str) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(value)
        .unwrap_or_else(|_| value.as_bytes().to_vec())
}

fn decode_git_object_body(value: &str, expected_kind: &str) -> Result<Vec<u8>, String> {
    let canonical = base64::engine::general_purpose::STANDARD
        .decode(value)
        .map_err(|e| format!("CanonicalBytes must be base64: {e}"))?;
    let Some(nul) = canonical.iter().position(|byte| *byte == 0) else {
        return Err("CanonicalBytes missing git object header terminator".to_string());
    };
    let header = std::str::from_utf8(&canonical[..nul])
        .map_err(|e| format!("CanonicalBytes header is not UTF-8: {e}"))?;
    let expected_prefix = format!("{expected_kind} ");
    if !header.starts_with(&expected_prefix) {
        return Err(format!(
            "CanonicalBytes header must start with '{expected_prefix}'"
        ));
    }
    Ok(canonical[nul + 1..].to_vec())
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .or_else(|| value.get("fields").and_then(|fields| fields.get(key)))
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

async fn string_field_resolved(
    state: &ServerState,
    tenant: &TenantId,
    value: &Value,
    key: &str,
) -> Result<Option<String>, String> {
    let Some(field) = value
        .get(key)
        .or_else(|| value.get("fields").and_then(|fields| fields.get(key)))
    else {
        return Ok(None);
    };
    if let Some(value) = field.as_str() {
        return Ok(Some(value.to_string()));
    }

    let Some(blob_key) = field
        .as_object()
        .and_then(|object| object.get(FIELD_OVERFLOW_REF_KEY))
        .and_then(Value::as_str)
    else {
        return Ok(None);
    };
    let Some(bytes) = state
        .get_blob_with_legacy_fallback(tenant, blob_key)
        .await
        .map_err(|error| format!("read Genesis field overflow blob {blob_key}: {error}"))?
    else {
        return Err(format!("Genesis field overflow blob {blob_key} not found"));
    };
    let restored: Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("decode Genesis field overflow blob {blob_key}: {error}"))?;
    restored
        .as_str()
        .map(ToString::to_string)
        .ok_or_else(|| format!("Genesis field overflow blob {blob_key} is not a string"))
        .map(Some)
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
    fn parses_git_tree_entries() {
        let mut body = Vec::new();
        body.extend_from_slice(b"100644 app.toml\0");
        body.extend_from_slice(&[0x11; 20]);
        body.extend_from_slice(b"40000 specs\0");
        body.extend_from_slice(&[0x22; 20]);

        let entries = parse_tree_entries(&body).expect("tree should parse");

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].mode, "100644");
        assert_eq!(entries[0].name, "app.toml");
        assert_eq!(
            entries[0].object_sha,
            "1111111111111111111111111111111111111111"
        );
        assert!(!entries[0].is_tree());
        assert_eq!(entries[1].name, "specs");
        assert!(entries[1].is_tree());
    }

    #[test]
    fn rejects_unsafe_tree_entry_names() {
        for name in [
            "",
            ".",
            "..",
            "../app.toml",
            "nested/app.toml",
            "nested\\app.toml",
        ] {
            assert!(
                validate_tree_entry_name(name).is_err(),
                "{name:?} should be rejected"
            );
        }
        validate_tree_entry_name("app.toml").expect("plain file names are safe");
    }

    #[test]
    fn install_ids_and_cache_fragments_are_stable() {
        assert_eq!(
            installation_id("app-Acme Notes", "tenant/a", "@abcdef0123456789"),
            "ai-app-acme-notes-tenant-a-abcdef0123456789"
        );
        assert_eq!(sanitize_fragment("../"), "item");
    }

    #[test]
    fn parses_pinned_registry_app_refs() {
        let parsed = parse_registry_app_ref("temperpaw/paw-agent@abc123").expect("valid app ref");
        assert_eq!(parsed.owner, "temperpaw");
        assert_eq!(parsed.name, "paw-agent");
        assert_eq!(parsed.version_hash.as_deref(), Some("abc123"));
        assert!(parse_registry_app_ref("paw-agent").is_err());
        assert!(parse_registry_app_ref("temperpaw/paw-agent@").is_err());
    }

    #[test]
    fn registry_git_urls_are_stable() {
        assert_eq!(
            registry_git_url("https://genesis.example/", "temperpaw", "paw-agent"),
            "https://genesis.example/temperpaw/paw-agent.git"
        );
    }

    #[test]
    fn parses_dependency_refs() {
        assert_eq!(
            parse_dependency_ref("paw-agent", "temperpaw"),
            DependencyRef {
                owner: Some("temperpaw".to_string()),
                name: "paw-agent".to_string(),
                version_hash: None,
            }
        );
        assert_eq!(
            parse_dependency_ref("katagami/katagami-commons@abc123", "temperpaw"),
            DependencyRef {
                owner: Some("katagami".to_string()),
                name: "katagami-commons".to_string(),
                version_hash: Some("abc123".to_string()),
            }
        );
    }

    #[test]
    fn source_tenants_default_to_default() {
        assert!(genesis_source_tenants().contains(&"default".to_string()));
    }
}
