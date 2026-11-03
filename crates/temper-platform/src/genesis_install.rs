//! Genesis app installation bridge.
//!
//! Specs own the public action (`App.Install`). This hook only runs after that
//! governed action has succeeded, then materializes the pinned Genesis commit
//! into the platform's app installer.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use temper_runtime::tenant::TenantId;
use temper_server::platform_store::InstalledAppRecord;
use temper_server::state::{BoundActionHook, BoundActionHookContext, DispatchCommand, ServerState};

mod blob_materialization;
mod bundle_transport;
mod bundles;
mod cache_paths;
mod closure_install;
mod legacy_specs;
mod object_lookup;
mod pushed;
mod registry;
mod restore;
mod written;
use blob_materialization::{
    MAX_GENESIS_TREE_CANONICAL_BYTES, blob_content_len, canonical_field_len, git_object_body,
    materialize_blob_content_field, read_canonical_field_bounded,
};
use bundles::{
    GenesisBundleBudget, MAX_GENESIS_BUNDLE_APPS, MAX_GENESIS_BUNDLE_FILE_BYTES,
    collect_bundle_files, materialize_registry_app_closure_via_bundle,
};
#[cfg(test)]
use bundles::{safe_bundle_relative_path, write_bundle_app};
use cache_paths::{
    CacheOwner, InstallSource, app_cache_dir, genesis_cache_root, replace_directory,
    sanitize_fragment, validate_git_object_id, validate_identity_component,
};
use object_lookup::load_genesis_object;
pub use pushed::{
    PushRefused, PushedFile, PushedInstall, adopt_catalog_app, install_pushed_app,
    pushed_install_recorded,
};
pub(crate) use registry::configured_registry;
use registry::{RegistryUrl, registry_client};
pub(crate) use restore::installed_version_files;
use restore::rollback_to_prior;
pub use restore::{
    CacheFetchSummary, CacheFetches, GenesisCacheRecovery, restore_genesis_app_cache_roots,
    restore_genesis_registry_cache_roots, start_genesis_registry_cache_recovery,
};
use written::Written;

use crate::genesis_install_verify::{
    InstallVerifyDecision, classify_install_verify, mark_install_failed, platform_store,
    restore_prior_install, verify_install_closure_runtime_ready, verify_install_runtime_ready,
};
use crate::os_apps::{
    AppFileSource, AppManifest, InstallResult, InstallWarning, OsAppReconcileResult, Swap,
    add_tenant_written_apps, add_written_apps_preferred, reconcile_os_app_swapping,
    resolve_tenant_os_app_install_order, tenant_os_app_bundle_digest,
};
use crate::state::PlatformState;

const MAX_GENESIS_MANIFEST_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Deserialize)]
pub struct GenesisRegistryInstallRequest {
    pub tenant: String,
    pub app_ref: String,
    #[serde(default)]
    pub registry_url: String,
    #[serde(default)]
    pub registry_tenant: String,
    #[serde(default)]
    pub follow_policy: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct GenesisRegistryInstallResult {
    pub app_ref: String,
    pub tenant: String,
    pub registry_url: String,
    pub registry_tenant: String,
    pub follow_policy: String,
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
    /// What the install's checks found and did not refuse, for every app
    /// installed with the root.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<InstallWarning>,
    /// The modules Jev reviewed, of every app installed with the root.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub modules_reviewed: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenesisRegistryBundleResponse {
    pub app_ref: String,
    pub registry_tenant: String,
    pub apps: Vec<GenesisRegistryBundleApp>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenesisRegistryBundleApp {
    pub owner: String,
    pub name: String,
    pub version_hash: String,
    pub files: Vec<GenesisRegistryBundleFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenesisRegistryBundleFile {
    pub path: String,
    pub content_base64: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct GenesisFollowLatestUpdate {
    pub tenant: String,
    pub app_name: String,
    pub app_ref: String,
    pub registry_url: String,
    pub registry_tenant: String,
    pub pinned_version_hash: String,
    pub current_version_hash: String,
    pub latest_version_hash: String,
    pub latest_app_ref: String,
    pub rollout_state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
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

/// Return read-only staged-follow status for Genesis installs.
///
/// This intentionally does not mutate running tenants. A caller that wants to
/// roll forward can take `latest_app_ref` and call the normal install endpoint,
/// preserving a visible promotion step.
pub async fn list_genesis_follow_latest_updates(
    platform: &PlatformState,
) -> Vec<GenesisFollowLatestUpdate> {
    let Some(ps) = platform
        .server
        .storage_stack
        .as_ref()
        .and_then(|stack| stack.platform.clone())
    else {
        return Vec::new();
    };

    let installed = match ps.list_all_installed_apps().await {
        Ok(installed) => installed,
        Err(error) => {
            tracing::warn!(error = %error, "Failed to list installed apps for Genesis follow status");
            return Vec::new();
        }
    };

    let mut updates = Vec::new();
    for (tenant, app_name) in installed {
        let record = match ps.get_installed_app(&tenant, &app_name).await {
            Ok(Some(record)) => record,
            Ok(None) => continue,
            Err(error) => {
                tracing::warn!(
                    tenant = %tenant,
                    app = %app_name,
                    error = %error,
                    "Failed to read installed app metadata for Genesis follow status"
                );
                continue;
            }
        };
        if record.source_kind != "genesis" || record.follow_policy != "follow_latest" {
            continue;
        }

        let parsed = match parse_registry_app_ref(&record.app_ref) {
            Ok(parsed) => parsed,
            Err(error) => {
                updates.push(follow_latest_error_update(record, error));
                continue;
            }
        };
        let registry = match configured_registry(&record.registry_url) {
            Ok(registry) => registry,
            Err(error) => {
                updates.push(follow_latest_error_update(record, error));
                continue;
            }
        };
        let registry_tenant = if record.registry_tenant.trim().is_empty() {
            "default".to_string()
        } else {
            record.registry_tenant.trim().to_string()
        };
        let latest = match fetch_registry_latest_version_hash(
            &registry,
            &registry_tenant,
            &parsed.owner,
            &parsed.name,
        )
        .await
        {
            Ok(hash) => hash.trim_start_matches('@').to_string(),
            Err(error) => {
                updates.push(follow_latest_error_update(record, error));
                continue;
            }
        };
        let current = record
            .current_version_hash
            .trim_start_matches('@')
            .to_string();
        let latest_app_ref = format!("{}/{}@{}", parsed.owner, parsed.name, latest);
        updates.push(GenesisFollowLatestUpdate {
            tenant: record.tenant,
            app_name: record.app_name,
            app_ref: record.app_ref,
            registry_url: registry.as_str().to_string(),
            registry_tenant,
            pinned_version_hash: record.pinned_version_hash,
            current_version_hash: current.clone(),
            latest_version_hash: latest.clone(),
            latest_app_ref,
            rollout_state: if latest == current {
                "current".to_string()
            } else {
                "pending".to_string()
            },
            error: None,
        });
    }

    updates
}

/// Install a pinned Genesis app ref into this Temper instance from a registry URL.
///
/// This is the local-instance counterpart to spec-owned `App.Install`: agent,
/// CLI, and admin clients call one semantic install operation, while this
/// helper materializes the pinned Git-native Genesis closure into the local
/// app catalog and then runs the normal Temper installer against this
/// instance's storage backend. The install is verified-or-reverted: after
/// reconciling the pinned closure it checks the root app is runtime-ready
/// (incl. required-wasm compile) and, if not, rolls back to the previously
/// installed pinned version so a bad publish never leaves the tenant broken.
pub async fn install_genesis_app_from_registry(
    platform: &PlatformState,
    request: GenesisRegistryInstallRequest,
) -> Result<GenesisRegistryInstallResult, String> {
    let install_started = Instant::now();
    let registry = configured_registry(&request.registry_url)?;
    let registry_url = registry.as_str().to_string();
    let registry_tenant = if request.registry_tenant.trim().is_empty() {
        "default".to_string()
    } else {
        request.registry_tenant.trim().to_string()
    };
    let follow_policy = normalize_follow_policy(&request.follow_policy)?;
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
    let cache_root = genesis_cache_root(
        &platform.server,
        CacheOwner::Install {
            tenant: &request.tenant,
            source: InstallSource::Registry(&registry_tenant),
        },
        &cache_key,
    );
    std::fs::create_dir_all(&cache_root).map_err(|error| {
        format!(
            "create Genesis registry cache '{}': {error}",
            cache_root.display()
        )
    })?;

    let materialize_started = Instant::now();
    let materialized_apps = match materialize_registry_app_closure_via_bundle(
        &registry,
        &registry_tenant,
        root_ref.clone(),
        &cache_root,
    )
    .await
    {
        Ok(apps) => {
            log_genesis_install_phase(
                &request.app_ref,
                "materialize_bundle",
                materialize_started,
                apps.len(),
                0,
            );
            apps
        }
        Err(error) if genesis_git_fallback_enabled() => {
            tracing::warn!(
                app_ref = %request.app_ref,
                registry_url = %registry_url,
                error = %error,
                "Genesis bundle fetch failed; falling back to git clone because TEMPER_GENESIS_INSTALL_GIT_FALLBACK is enabled"
            );
            let git_started = Instant::now();
            let apps =
                materialize_registry_app_closure(&registry, root_ref.clone(), &cache_root).await?;
            log_genesis_install_phase(
                &request.app_ref,
                "materialize_git_fallback",
                git_started,
                apps.len(),
                0,
            );
            apps
        }
        Err(error) => {
            return Err(format!(
                "Genesis bundle fetch failed for {} from {}: {error}. Git fallback is disabled; set TEMPER_GENESIS_INSTALL_GIT_FALLBACK=1 only for admin/debug recovery.",
                request.app_ref, registry_url
            ));
        }
    };
    // Every source ends in the same install: parse, register, reconcile,
    // verify, then commit or go back to the version before.
    let turn = temper_server::host_turn::take().await?;
    let installed = closure_install::install_materialized_closure(
        platform,
        &request.tenant,
        &cache_root,
        closure_install::MaterializedClosure {
            app_ref: &request.app_ref,
            root: root_ref.clone(),
            apps: materialized_apps,
            source: closure_install::ClosureSource {
                source_kind: "genesis",
                registry_url: &registry_url,
                registry_tenant: &registry_tenant,
                follow_policy: &follow_policy,
            },
        },
    )
    .await;
    drop(turn);
    let installed = match installed {
        Ok(installed) => installed,
        Err(
            closure_install::ClosureRefused::Failed(error)
            | closure_install::ClosureRefused::Inactive(error),
        ) => return Err(error),
        Err(closure_install::ClosureRefused::Unparsed(error)) => {
            return Err(format!(
                "Genesis install of {} refused: {error}",
                request.app_ref
            ));
        }
    };
    log_genesis_install_phase(
        &request.app_ref,
        "total",
        install_started,
        installed.materialized.len(),
        0,
    );
    // This answer says what Jev found in every app it installed, so it waits for him.
    let install = installed.install.with_reviews_read().await;
    Ok(GenesisRegistryInstallResult {
        app_ref: request.app_ref,
        tenant: request.tenant,
        registry_url,
        registry_tenant,
        follow_policy,
        closure_id: installed.closure_id,
        materialized_path: cache_root.display().to_string(),
        materialized_apps: installed.materialized,
        added: install.added,
        updated: install.updated,
        skipped: install.skipped,
        wasm_modules: install.wasm_modules,
        agents: install.agents,
        agent_skills: install.skills,
        adrs: install.adrs_bootstrapped,
        seed_instances: install.seed_instances,
        warnings: install.warnings,
        modules_reviewed: install.modules_reviewed,
    })
}

pub(crate) async fn reconcile_materialized_app_closure(
    platform: &PlatformState,
    tenant: &str,
    root_app_name: &str,
    swap: Swap,
) -> Result<InstallResult, String> {
    let order = resolve_tenant_os_app_install_order(tenant, &[root_app_name.to_string()])?;
    reconcile_app_install_order(platform, tenant, root_app_name, &order, swap).await
}

/// Reconcile exactly the closure resolved before its checks and snapshots.
async fn reconcile_app_install_order(
    platform: &PlatformState,
    tenant: &str,
    root_app_name: &str,
    order: &[String],
    swap: Swap,
) -> Result<InstallResult, String> {
    let mut root_result = InstallResult::default();
    let mut dependencies = Vec::new();

    for app_name in order {
        let started = Instant::now();
        match reconcile_os_app_swapping(platform, tenant, app_name, swap).await? {
            OsAppReconcileResult::Skipped { bundle_digest, .. } => {
                tracing::info!(
                    tenant = %tenant,
                    app = %app_name,
                    bundle_digest = %bundle_digest,
                    duration_ms = started.elapsed().as_millis() as u64,
                    "Genesis materialized app unchanged; skipped reconcile"
                );
            }
            OsAppReconcileResult::Installed { install, .. } => {
                tracing::info!(
                    tenant = %tenant,
                    app = %app_name,
                    duration_ms = started.elapsed().as_millis() as u64,
                    wasm_modules = install.wasm_modules.len(),
                    agents = install.agents.len(),
                    skills = install.skills.len(),
                    "Genesis materialized app reconciled"
                );
                if app_name == root_app_name {
                    root_result = *install;
                } else {
                    dependencies.push(*install);
                }
            }
        }
    }
    for dependency in dependencies.into_iter().rev() {
        root_result.put_first(dependency);
    }

    Ok(root_result)
}

fn log_genesis_install_phase(
    app_ref: &str,
    phase: &str,
    started: Instant,
    count: usize,
    bytes: usize,
) {
    tracing::info!(
        app_ref = %app_ref,
        phase = %phase,
        duration_ms = started.elapsed().as_millis() as u64,
        count,
        bytes,
        "Genesis install phase complete"
    );
}

fn genesis_git_fallback_enabled() -> bool {
    matches!(
        std::env::var("TEMPER_GENESIS_INSTALL_GIT_FALLBACK")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "yes"
    )
}

fn follow_latest_error_update(
    record: InstalledAppRecord,
    error: String,
) -> GenesisFollowLatestUpdate {
    GenesisFollowLatestUpdate {
        tenant: record.tenant,
        app_name: record.app_name,
        app_ref: record.app_ref,
        registry_url: record.registry_url,
        registry_tenant: if record.registry_tenant.trim().is_empty() {
            "default".to_string()
        } else {
            record.registry_tenant
        },
        pinned_version_hash: record.pinned_version_hash,
        current_version_hash: record.current_version_hash,
        latest_version_hash: String::new(),
        latest_app_ref: String::new(),
        rollout_state: "error".to_string(),
        error: Some(error),
    }
}

async fn fetch_registry_latest_version_hash(
    registry: &RegistryUrl,
    registry_tenant: &str,
    owner: &str,
    name: &str,
) -> Result<String, String> {
    let app_id = format!(
        "app-{}-{}",
        sanitize_registry_id_component(owner),
        sanitize_registry_id_component(name)
    );
    let url = format!(
        "{}/tdata/Apps('{}')",
        registry.as_str(),
        app_id.replace('\'', "''")
    );
    let response = registry_client()?
        .get(&url)
        .header("X-Tenant-Id", registry_tenant)
        .send()
        .await
        .map_err(|error| format!("request Genesis App row {url}: {error}"))?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(format!(
            "request Genesis App row {url} returned {status}: {}",
            body.trim()
        ));
    }
    let row: Value = response
        .json()
        .await
        .map_err(|error| format!("decode Genesis App row {url}: {error}"))?;
    string_field(row.get("fields").unwrap_or(&row), "LatestVersionHash")
        .filter(|hash| !hash.trim().is_empty())
        .ok_or_else(|| format!("Genesis App row {app_id} is missing LatestVersionHash"))
}

fn sanitize_registry_id_component(input: &str) -> String {
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
    validate_identity_component("owner", owner)?;
    validate_identity_component("app name", name)?;
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
    registry: &RegistryUrl,
    owner: &str,
    name: &str,
    version_hash: Option<&str>,
    app_dir: &Path,
) -> Result<String, String> {
    validate_identity_component("owner", owner)?;
    validate_identity_component("app name", name)?;
    let remote = registry_git_url(registry, owner, name);
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
        // Like the registry HTTP client, git takes objects from the configured
        // registry itself and does not follow redirects.
        let clone = [
            GIT_NO_REDIRECTS,
            &["clone", remote.as_str(), app_dir_arg.as_str()],
        ];
        run_git(None, &clone.concat()).await?;
    } else {
        run_git(Some(app_dir), &["remote", "set-url", "origin", &remote]).await?;
        let fetch = [GIT_NO_REDIRECTS, &["fetch", "origin", "--tags", "--prune"]];
        run_git(Some(app_dir), &fetch.concat()).await?;
    }

    if let Some(hash) = version_hash {
        let hash = validate_git_object_id(hash)?;
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

/// Clone `root_ref`'s closure from the registry's git into `cache_root`, and
/// keep its list ([`written`]): each app's files as its commit names them.
async fn materialize_registry_app_closure(
    registry: &RegistryUrl,
    root_ref: RegistryAppRef,
    cache_root: &Path,
) -> Result<Vec<closure_install::MaterializedApp>, String> {
    let mut stack = vec![root_ref];
    let mut seen = BTreeSet::new();
    let mut materialized_refs = Vec::new();
    let mut materialized = Written::new();

    while let Some(app_ref) = stack.pop() {
        let key = format!("{}/{}", app_ref.owner, app_ref.name);
        if !seen.insert(key) {
            continue;
        }

        let app_dir = app_cache_dir(cache_root, &app_ref.name)?;
        let resolved_hash = materialize_git_registry_app(
            registry,
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
        // What the checked-out commit holds, from git itself, not a listing.
        let tree = run_git(
            Some(app_dir.as_path()),
            &["ls-tree", "-r", "-z", "--name-only", "HEAD"],
        )
        .await?;
        let paths = tree.split('\0').filter(|path| !path.is_empty());
        materialized.insert(app_ref.name.clone(), paths.map(str::to_string).collect());

        for dependency in read_manifest_dependencies(&app_dir)?.into_iter().rev() {
            let dependency = parse_dependency_ref(&dependency, &app_ref.owner);
            stack.push(RegistryAppRef {
                owner: dependency.owner.unwrap_or_else(|| app_ref.owner.clone()),
                name: dependency.name,
                version_hash: dependency.version_hash,
            });
        }
    }

    written::keep(cache_root, &materialized)?;
    let mut files: BTreeMap<_, _> = written::app_files(cache_root, materialized)?
        .into_iter()
        .collect();
    materialized_refs
        .into_iter()
        .map(|version| {
            let files = files.remove(&version.name);
            let files = files.ok_or_else(|| format!("{} has no files", version.name))?;
            Ok::<_, String>(closure_install::MaterializedApp { version, files })
        })
        .collect()
}

fn registry_git_url(registry: &RegistryUrl, owner: &str, name: &str) -> String {
    format!("{}/{}/{}.git", registry.as_str(), owner, name)
}

/// Git options for registry clones and fetches: do not follow HTTP redirects.
const GIT_NO_REDIRECTS: &[&str] = &["-c", "http.followRedirects=false"];

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
        let latest_version_hash = string_field(fields, "LatestVersionHash")
            .ok_or_else(|| "App.Install requires App.LatestVersionHash".to_string())?;
        let target_tenant = string_field(params, "TargetTenant")
            .or_else(|| string_field(params, "tenant"))
            .unwrap_or_else(|| tenant.as_str().to_string());
        let install_ref = resolve_install_app_ref(
            &owner,
            &name,
            &latest_version_hash,
            string_field(params, "AppRef").as_deref(),
        )?;
        let app_ref = install_ref.app_ref;
        let version_hash = install_ref.version_hash;
        let registry_url = string_field(params, "RegistryUrl")
            .or_else(|| string_field(params, "registry_url"))
            .unwrap_or_default();
        let registry_tenant = string_field(params, "RegistryTenant")
            .or_else(|| string_field(params, "registry_tenant"))
            .unwrap_or_else(|| tenant.as_str().to_string());
        let follow_policy = normalize_follow_policy(
            &string_field(params, "FollowPolicy")
                .or_else(|| string_field(params, "follow_policy"))
                .unwrap_or_default(),
        )?;
        let installation_id = installation_id(entity_id, &target_tenant, &version_hash);

        let cache_root = genesis_cache_root(
            state,
            CacheOwner::Install {
                tenant: &target_tenant,
                source: InstallSource::Local(tenant.as_str()),
            },
            &app_ref,
        );
        let written = materialize_app_closure(
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
        let materialized_apps: Vec<String> = written.iter().map(|(app, _)| app.clone()).collect();
        let app_dir = app_cache_dir(&cache_root, &name)?;
        // Late boot recovery uses the same turn when checking current records
        // and publishing files. Keep publication and provenance in one turn.
        let _turn = temper_server::host_turn::take().await?;
        register_written_apps(&target_tenant, cache_root, written);

        let mut platform = self.platform.clone();
        platform.server = state.clone();
        match reconcile_materialized_app_closure(
            &platform,
            &target_tenant,
            &name,
            Swap::ModulesFirst,
        )
        .await
        {
            Ok(result) => {
                let closure_id = format!(
                    "genesis:{}:{}",
                    app_ref,
                    version_hash.trim_start_matches('@')
                );
                if let Err(error) = record_genesis_install_metadata(
                    &platform,
                    GenesisInstallMetadata {
                        target_tenant: &target_tenant,
                        app_name: &name,
                        app_ref: &app_ref,
                        version_hash: &version_hash,
                        closure_id: &closure_id,
                        source_kind: "genesis",
                        registry_url: &registry_url,
                        registry_tenant: &registry_tenant,
                        follow_policy: &follow_policy,
                    },
                )
                .await
                {
                    tracing::warn!(
                        app = %name,
                        error = %error,
                        "Failed to persist Genesis provenance during follow-latest reconcile"
                    );
                }
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
                result.publish_reviews();
                Ok(Some(serde_json::json!({
                    "kind": "genesis_app_install",
                    "appRef": app_ref,
                    "targetTenant": target_tenant,
                    "followPolicy": follow_policy,
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
    /// `genesis` or `pushed`: where the version came from.
    source_kind: &'a str,
    registry_url: &'a str,
    registry_tenant: &'a str,
    follow_policy: &'a str,
}

#[derive(Debug)]
struct ResolvedInstallAppRef {
    app_ref: String,
    version_hash: String,
}

fn resolve_install_app_ref(
    owner: &str,
    name: &str,
    latest_version_hash: &str,
    requested_app_ref: Option<&str>,
) -> Result<ResolvedInstallAppRef, String> {
    validate_identity_component("owner", owner)?;
    validate_identity_component("app name", name)?;
    let latest = latest_version_hash.trim_start_matches('@');
    let Some(raw_app_ref) = requested_app_ref
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(ResolvedInstallAppRef {
            app_ref: format!("{owner}/{name}@{latest}"),
            version_hash: latest.to_string(),
        });
    };

    let parsed = parse_registry_app_ref(raw_app_ref)?;
    if parsed.owner != owner || parsed.name != name {
        return Err(format!(
            "App.Install AppRef '{}' does not match App row {}/{}",
            raw_app_ref, owner, name
        ));
    }
    let version_hash = parsed
        .version_hash
        .as_deref()
        .unwrap_or(latest)
        .trim_start_matches('@')
        .to_string();
    Ok(ResolvedInstallAppRef {
        app_ref: format!("{owner}/{name}@{version_hash}"),
        version_hash,
    })
}

fn normalize_follow_policy(raw: &str) -> Result<String, String> {
    let normalized = raw.trim().to_ascii_lowercase();
    if normalized.is_empty() || normalized == "pinned" {
        return Ok("pinned".to_string());
    }
    if normalized == "follow_latest" || normalized == "follow-latest" {
        return Ok("follow_latest".to_string());
    }
    Err(format!(
        "Genesis install follow_policy must be 'pinned' or 'follow_latest', got '{raw}'"
    ))
}

async fn record_genesis_install_metadata(
    platform: &PlatformState,
    metadata: GenesisInstallMetadata<'_>,
) -> Result<(), String> {
    let Some(ps) = platform
        .server
        .storage_stack
        .as_ref()
        .and_then(|stack| stack.platform.clone())
    else {
        return Ok(());
    };
    let Some(digest) = tenant_os_app_bundle_digest(metadata.target_tenant, metadata.app_name)
    else {
        return Err(format!(
            "installed Genesis app {} but could not compute bundle digest for durable provenance",
            metadata.app_name
        ));
    };

    let existing_record = match ps
        .get_installed_app(metadata.target_tenant, metadata.app_name)
        .await
    {
        Ok(record) => record,
        Err(error) => {
            tracing::warn!(
                tenant = %metadata.target_tenant,
                app = %metadata.app_name,
                app_ref = %metadata.app_ref,
                error = %error,
                "Failed to read existing Genesis app provenance before update"
            );
            None
        }
    };
    let (pinned_version_hash, current_version_hash) = provenance_hashes_for_policy(
        metadata.follow_policy,
        metadata.version_hash,
        existing_record.as_ref(),
    );

    let record = InstalledAppRecord {
        tenant: metadata.target_tenant.to_string(),
        app_name: digest.app_name,
        source_kind: metadata.source_kind.to_string(),
        app_ref: metadata.app_ref.to_string(),
        version_hash: current_version_hash.clone(),
        pinned_version_hash,
        current_version_hash,
        follow_policy: metadata.follow_policy.to_string(),
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

    ps.record_installed_app_metadata(&record)
        .await
        .map_err(|error| {
            format!(
                "failed to persist Genesis app provenance for {}: {error}",
                metadata.app_name
            )
        })
}

fn provenance_hashes_for_policy(
    follow_policy: &str,
    version_hash: &str,
    existing: Option<&InstalledAppRecord>,
) -> (String, String) {
    let current = version_hash.trim_start_matches('@').to_string();
    let pinned = if follow_policy == "follow_latest" {
        existing
            .filter(|record| record.source_kind == "genesis")
            .map(|record| record.pinned_version_hash.trim_start_matches('@'))
            .filter(|hash| !hash.is_empty())
            .unwrap_or(current.as_str())
            .to_string()
    } else {
        current.clone()
    };
    (pinned, current)
}

#[derive(Debug, Clone)]
struct GenesisAppBundle {
    owner: String,
    name: String,
    repository_id: String,
    version_hash: String,
}

#[derive(Default)]
struct GenesisClosureAdmission {
    versions_by_app: BTreeMap<(String, String), String>,
    owners_by_directory: BTreeMap<String, String>,
}

impl GenesisClosureAdmission {
    fn admit(&mut self, app: &GenesisAppBundle) -> Result<bool, String> {
        let app_key = (app.owner.clone(), app.name.clone());
        if let Some(version) = self.versions_by_app.get(&app_key) {
            if version != &app.version_hash {
                return Err(format!(
                    "Genesis dependency {}/{} resolves to conflicting versions '{}' and '{}'",
                    app.owner, app.name, version, app.version_hash
                ));
            }
            return Ok(false);
        }
        if let Some(owner) = self.owners_by_directory.get(&app.name)
            && owner != &app.owner
        {
            return Err(format!(
                "Genesis dependencies {owner}/{} and {}/{} collide on cache directory '{}'",
                app.name, app.owner, app.name, app.name
            ));
        }
        if self.versions_by_app.len() >= MAX_GENESIS_BUNDLE_APPS {
            return Err(format!(
                "Genesis dependency closure exceeds app budget {MAX_GENESIS_BUNDLE_APPS}"
            ));
        }
        self.owners_by_directory
            .insert(app.name.clone(), app.owner.clone());
        self.versions_by_app
            .insert(app_key, app.version_hash.clone());
        Ok(true)
    }
}

/// Who is asking for a bundle, and therefore what the closure may contain.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BundleAudience {
    /// A caller the kernel authenticated. Cedar governs what it may read.
    Authenticated,
    /// No credential was presented. Every repository in the closure must be
    /// public, not merely the one that was named.
    Anonymous,
}

/// Export a pinned app bundle for an authenticated caller.
pub async fn export_genesis_registry_bundle(
    platform: &PlatformState,
    registry_tenant: &str,
    owner: &str,
    name: &str,
    version_hash: &str,
) -> Result<GenesisRegistryBundleResponse, String> {
    export_genesis_registry_bundle_for(
        platform,
        registry_tenant,
        owner,
        name,
        version_hash,
        BundleAudience::Authenticated,
    )
    .await
}

/// Refuse unless this repository is public.
///
/// Called for each app as the closure is walked, BEFORE its tree is
/// materialized. An app's bundle is its whole dependency closure, so "is this
/// public?" is a question about every repository in it — and the check has to
/// happen before the content is read, not after: resolving the closure
/// materializes each tree in order to read its manifest, so a check that ran
/// afterwards would already have written a private repository to disk.
async fn refuse_unless_repository_is_public(
    state: &ServerState,
    tenant: &TenantId,
    app: &GenesisAppBundle,
) -> Result<(), String> {
    if !state
        .ensure_entity_loaded(tenant, "Repository", &app.repository_id)
        .await
    {
        return Err(format!(
            "Genesis bundle refused: repository {} for {}/{} could not be read",
            app.repository_id, app.owner, app.name
        ));
    }
    let repository = state
        .get_tenant_entity_state(tenant, "Repository", &app.repository_id)
        .await
        .map_err(|error| {
            format!(
                "Genesis bundle refused: repository {} could not be read: {error}",
                app.repository_id
            )
        })?;
    let visibility = repository
        .state
        .fields
        .get("Visibility")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if visibility != "public" {
        tracing::warn!(
            tenant = %tenant,
            repository_id = %app.repository_id,
            app = %app.name,
            visibility,
            "anonymous Genesis bundle refused: a repository in the closure is not public"
        );
        return Err(format!(
            "Genesis bundle refused: {}/{} is not public",
            app.owner, app.name
        ));
    }
    Ok(())
}

/// Export a pinned app bundle, bounded by who is asking.
///
/// An app's bundle is its whole dependency closure, so "is this app public?" is
/// a question about every repository in that closure, not about the one the
/// caller named. A public app that depends on a private one must not become a
/// way to read the private one anonymously.
pub async fn export_genesis_registry_bundle_for(
    platform: &PlatformState,
    registry_tenant: &str,
    owner: &str,
    name: &str,
    version_hash: &str,
    audience: BundleAudience,
) -> Result<GenesisRegistryBundleResponse, String> {
    let tenant = TenantId::new(registry_tenant);
    let root = resolve_genesis_app_by_ref(
        &platform.server,
        &tenant,
        owner,
        name,
        version_hash.trim_start_matches('@'),
    )
    .await?;
    let app_ref = format!(
        "{}/{}@{}",
        root.owner,
        root.name,
        root.version_hash.trim_start_matches('@')
    );
    let cache_root = genesis_cache_root(
        &platform.server,
        CacheOwner::Export { registry_tenant },
        &app_ref,
    );
    let closure = resolve_genesis_app_closure(&platform.server, &tenant, root, audience).await?;
    if closure.len() > MAX_GENESIS_BUNDLE_APPS {
        return Err(format!(
            "Genesis bundle closure contains {} apps; budget is {MAX_GENESIS_BUNDLE_APPS}",
            closure.len()
        ));
    }
    let mut apps = Vec::new();
    let mut bundle_budget = GenesisBundleBudget::new();
    let mut materialization_budget = GenesisBundleBudget::new();

    for app in closure {
        let app_dir = app_cache_dir(&cache_root, &app.name)?;
        let started = Instant::now();
        let paths = materialize_commit_tree(
            &platform.server,
            &tenant,
            &app.repository_id,
            &app.version_hash,
            &app_dir,
            &mut materialization_budget,
        )
        .await?;
        let files = collect_bundle_files(&app_dir, &paths, &mut bundle_budget)?;
        tracing::info!(
            registry_tenant = %registry_tenant,
            app = %app.name,
            version_hash = %app.version_hash,
            duration_ms = started.elapsed().as_millis() as u64,
            files = files.len(),
            "Exported Genesis app bundle files"
        );
        apps.push(GenesisRegistryBundleApp {
            owner: app.owner,
            name: app.name,
            version_hash: app.version_hash.trim_start_matches('@').to_string(),
            files,
        });
    }

    Ok(GenesisRegistryBundleResponse {
        app_ref,
        registry_tenant: registry_tenant.to_string(),
        apps,
    })
}

async fn resolve_genesis_app_by_ref(
    state: &ServerState,
    tenant: &TenantId,
    owner: &str,
    name: &str,
    version_hash: &str,
) -> Result<GenesisAppBundle, String> {
    let ids = state.list_entity_ids_lazy(tenant, "App").await;
    for entity_id in ids {
        let candidate = state
            .get_tenant_entity_state(tenant, "App", &entity_id)
            .await
            .map_err(|error| format!("read Genesis App {entity_id}: {error}"))?;
        if candidate.state.status != "Active" {
            continue;
        }
        let fields = &candidate.state.fields;
        let Some(candidate_name) = string_field(fields, "Name") else {
            continue;
        };
        let Some(candidate_owner) = string_field(fields, "OwnerId") else {
            continue;
        };
        if candidate_owner != owner || candidate_name != name {
            continue;
        }
        let Some(repository_id) = string_field(fields, "RepositoryId") else {
            continue;
        };
        return Ok(GenesisAppBundle {
            owner: candidate_owner,
            name: candidate_name,
            repository_id,
            version_hash: version_hash.trim_start_matches('@').to_string(),
        });
    }

    Err(format!(
        "no active Genesis App found for {owner}/{name}@{}",
        version_hash.trim_start_matches('@')
    ))
}

async fn resolve_genesis_app_closure(
    state: &ServerState,
    tenant: &TenantId,
    root: GenesisAppBundle,
    audience: BundleAudience,
) -> Result<Vec<GenesisAppBundle>, String> {
    let mut stack = vec![root];
    let mut admission = GenesisClosureAdmission::default();
    let mut closure = Vec::new();
    let mut materialization_budget = GenesisBundleBudget::new();

    while let Some(app) = stack.pop() {
        if !admission.admit(&app)? {
            continue;
        }
        if audience == BundleAudience::Anonymous {
            // Before materialize_commit_tree, which writes this repository's
            // content to the cache in order to read its manifest.
            refuse_unless_repository_is_public(state, tenant, &app).await?;
        }
        let cache_root = genesis_cache_root(
            state,
            CacheOwner::DependencyRead {
                tenant: tenant.as_str(),
            },
            &format!(
                "{}-{}-dependency-read-{}",
                app.owner,
                app.name,
                app.version_hash.trim_start_matches('@')
            ),
        );
        let app_dir = app_cache_dir(&cache_root, &app.name)?;
        materialize_commit_tree(
            state,
            tenant,
            &app.repository_id,
            &app.version_hash,
            &app_dir,
            &mut materialization_budget,
        )
        .await?;
        for dependency in read_manifest_dependencies(&app_dir)?.into_iter().rev() {
            let dependency = resolve_genesis_dependency(state, tenant, &app.owner, &dependency)
                .await
                .map_err(|error| {
                    format!(
                        "resolve dependency '{}' for Genesis app '{}': {error}",
                        dependency, app.name
                    )
                })?;
            stack.push(dependency);
        }
        closure.push(app);
    }

    Ok(closure)
}

/// Write `root`'s closure from this server's Genesis objects into
/// `cache_root`, each app at `<cache_root>/<name>`, and keep its list
/// ([`written`]). Returns each app with the files it was written with.
async fn materialize_app_closure(
    state: &ServerState,
    tenant: &TenantId,
    cache_root: &Path,
    root: GenesisAppBundle,
) -> Result<Vec<(String, AppFileSource)>, String> {
    let mut stack = vec![root];
    let mut admission = GenesisClosureAdmission::default();
    let mut materialized = Written::new();
    let mut materialization_budget = GenesisBundleBudget::new();

    while let Some(app) = stack.pop() {
        if !admission.admit(&app)? {
            continue;
        }

        let app_dir = app_cache_dir(cache_root, &app.name)?;
        let paths = materialize_commit_tree(
            state,
            tenant,
            &app.repository_id,
            &app.version_hash,
            &app_dir,
            &mut materialization_budget,
        )
        .await?;
        materialized.insert(app.name.clone(), paths);

        for dependency in read_manifest_dependencies(&app_dir)?.into_iter().rev() {
            let dependency = resolve_genesis_dependency(state, tenant, &app.owner, &dependency)
                .await
                .map_err(|error| {
                    format!(
                        "resolve dependency '{}' for Genesis app '{}': {error}",
                        dependency, app.name
                    )
                })?;
            stack.push(dependency);
        }
    }

    written::keep(cache_root, &materialized)?;
    written::app_files(cache_root, materialized)
}

fn read_manifest_dependencies(app_dir: &Path) -> Result<Vec<String>, String> {
    let path = app_dir.join("app.toml");
    let metadata = std::fs::symlink_metadata(&path)
        .map_err(|error| format!("stat Genesis app manifest '{}': {error}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "Genesis app manifest '{}' must be a regular file",
            path.display()
        ));
    }
    if metadata.len() > MAX_GENESIS_MANIFEST_BYTES {
        return Err(format!(
            "Genesis app manifest '{}' is {} bytes; budget is {MAX_GENESIS_MANIFEST_BYTES}",
            path.display(),
            metadata.len()
        ));
    }
    let mut content = String::with_capacity(metadata.len() as usize);
    std::fs::File::open(&path)
        .map_err(|error| format!("open Genesis app manifest '{}': {error}", path.display()))?
        .take(MAX_GENESIS_MANIFEST_BYTES.saturating_add(1))
        .read_to_string(&mut content)
        .map_err(|error| format!("read Genesis app manifest '{}': {error}", path.display()))?;
    if content.len() as u64 > MAX_GENESIS_MANIFEST_BYTES {
        return Err(format!(
            "Genesis app manifest '{}' exceeded its byte budget while reading",
            path.display()
        ));
    }
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
    budget: &mut GenesisBundleBudget,
) -> Result<BTreeSet<String>, String> {
    let commit_id = validate_git_object_id(version_hash)?;
    let commit = load_genesis_object(state, tenant, "Commit", repository_id, commit_id)
        .await?
        .ok_or_else(|| format!("Genesis commit {commit_id} not found for {repository_id}"))?;
    let tree_sha = string_field(&commit.state.fields, "TreeSha")
        .ok_or_else(|| format!("Genesis commit {commit_id} is missing TreeSha"))?;
    let parent = app_dir.parent().ok_or_else(|| {
        format!(
            "Genesis app cache '{}' has no parent directory",
            app_dir.display()
        )
    })?;
    std::fs::create_dir_all(parent).map_err(|error| {
        format!(
            "create Genesis app cache parent '{}': {error}",
            parent.display()
        )
    })?;
    let staged = tempfile::Builder::new()
        .prefix(".genesis-tree-")
        .tempdir_in(parent)
        .map_err(|error| format!("create staged Genesis app cache: {error}"))?;
    let paths = materialize_tree(
        state,
        tenant,
        repository_id,
        &tree_sha,
        staged.path(),
        budget,
    )
    .await?;
    replace_directory(staged.keep(), app_dir)?;
    Ok(paths)
}

async fn materialize_tree(
    state: &ServerState,
    tenant: &TenantId,
    repository_id: &str,
    tree_sha: &str,
    dir: &Path,
    budget: &mut GenesisBundleBudget,
) -> Result<BTreeSet<String>, String> {
    let mut written = BTreeSet::new();
    let mut stack = vec![(tree_sha.to_string(), dir.to_path_buf(), 0usize)];
    while let Some((current_tree, current_dir, depth)) = stack.pop() {
        std::fs::create_dir_all(&current_dir)
            .map_err(|e| format!("create directory '{}': {e}", current_dir.display()))?;
        let tree = load_genesis_object(state, tenant, "Tree", repository_id, &current_tree)
            .await?
            .ok_or_else(|| format!("Genesis tree {current_tree} not found for {repository_id}"))?;
        let canonical_bytes = canonical_field_len(&tree.state.fields, "tree")?;
        budget.consume_tree(&current_dir, canonical_bytes)?;
        let canonical = read_canonical_field_bounded(
            state,
            tenant,
            &tree.state.fields,
            "tree",
            MAX_GENESIS_TREE_CANONICAL_BYTES,
        )
        .await
        .map_err(|error| format!("read Genesis tree {current_tree}: {error}"))?;
        for entry in parse_tree_entries(git_object_body(&canonical, "tree")?)? {
            validate_tree_entry_name(&entry.name)?;
            let path = current_dir.join(&entry.name);
            let entry_depth = depth
                .checked_add(1)
                .ok_or_else(|| "Genesis tree depth overflowed usize".to_string())?;
            budget.consume_tree_entry(&path, entry_depth)?;
            if entry.is_tree() {
                stack.push((entry.object_sha, path, entry_depth));
                continue;
            }
            let relative = path.strip_prefix(dir).map_err(|e| e.to_string())?;
            written.insert(relative.to_string_lossy().replace('\\', "/"));
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
            let content_bytes = blob_content_len(&blob.state.fields)?;
            budget.consume_file(&path, content_bytes)?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("create directory '{}': {e}", parent.display()))?;
            }
            materialize_blob_content_field(
                state,
                tenant,
                &blob.state.fields,
                &path,
                MAX_GENESIS_BUNDLE_FILE_BYTES,
            )
            .await
            .map_err(|error| {
                format!(
                    "materialize Genesis blob {} to '{}': {error}",
                    entry.object_sha,
                    path.display()
                )
            })?;
        }
    }
    Ok(written)
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

fn string_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .or_else(|| value.get("fields").and_then(|fields| fields.get(key)))
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

/// Make `apps` of an install's cache directory resolve for the tenant it was made for.
///
/// The platform tenant's installs are the shared catalog; every other
/// tenant's are its own, so they never change what another tenant or a
/// shared app resolves. Each app is registered with the files its install
/// wrote ([`written`]), so nothing lists the directory.
fn register_written_apps(tenant: &str, cache_root: PathBuf, apps: Vec<(String, AppFileSource)>) {
    if tenant == TenantId::default().as_str() {
        add_written_apps_preferred(cache_root, apps);
    } else {
        add_tenant_written_apps(tenant, cache_root, apps);
    }
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

#[cfg(test)]
mod tests {
    use super::object_lookup::genesis_object_entity_id;
    use base64::Engine as _;
    use sha2::Digest as _;
    use temper_runtime::ActorSystem;
    use temper_spec::csdl::CsdlDocument;

    use super::*;

    #[test]
    fn genesis_object_entity_id_matches_ingest_scheme() {
        // Byte-identical to scm_ingest_pack::object_entity_id, the writer of
        // these keys. Real durable ids observed in prod Genesis:
        assert_eq!(
            genesis_object_entity_id(
                "rp-katagami-katagami-curation",
                "5a7ae8c0224769fdfc27106329f21a8fcb7b8441"
            ),
            "rp-katagami-katagami-curation-5a7ae8c0224769fdfc27106329f21a8fcb7b8441"
        );
        // Already-canonical repository ids round-trip unchanged (idempotent).
        assert_eq!(
            genesis_object_entity_id("rp-temperpaw-paw-foresight", "013224e7"),
            "rp-temperpaw-paw-foresight-013224e7"
        );
        // Non-canonical input is sanitized: lowercased, non-alphanumeric runs
        // collapse to a single dash, leading/trailing dashes trimmed.
        assert_eq!(
            genesis_object_entity_id("Katagami/Katagami Curation", "abc"),
            "katagami-katagami-curation-abc"
        );
        // Empty repository id falls back to the `obj-` prefix, never bare sha.
        assert_eq!(genesis_object_entity_id("", "abc"), "obj-abc");
        assert_ne!(genesis_object_entity_id("rp-x", "abc"), "abc");
    }

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
    fn genesis_cache_roots_do_not_collide_after_sanitizing() {
        let data_dir = tempfile::tempdir().expect("data dir");
        let mut state = ServerState::new(
            ActorSystem::new("genesis-cache-key-test"),
            CsdlDocument {
                version: "4.0".to_string(),
                schemas: Vec::new(),
            },
            String::new(),
        );
        state.data_dir = data_dir.path().to_path_buf();

        let owner = CacheOwner::Install {
            tenant: "tenant-a",
            source: InstallSource::Registry("default"),
        };
        assert_ne!(
            genesis_cache_root(&state, owner, "owner/app@a-b"),
            genesis_cache_root(&state, owner, "owner/app@a/b")
        );
    }

    #[test]
    fn each_tenants_install_of_one_ref_has_its_own_cache_directory() {
        let data_dir = tempfile::tempdir().expect("data dir");
        let mut state = ServerState::new(
            ActorSystem::new("genesis-cache-owner-test"),
            CsdlDocument {
                version: "4.0".to_string(),
                schemas: Vec::new(),
            },
            String::new(),
        );
        state.data_dir = data_dir.path().to_path_buf();
        let app_ref = "acme/notes@0123456789abcdef0123456789abcdef01234567";
        let install = |tenant, source| CacheOwner::Install { tenant, source };

        let roots = [
            genesis_cache_root(
                &state,
                install("tenant-a", InstallSource::Registry("default")),
                app_ref,
            ),
            genesis_cache_root(
                &state,
                install("tenant-b", InstallSource::Registry("default")),
                app_ref,
            ),
            genesis_cache_root(
                &state,
                install("tenant-a", InstallSource::Registry("other")),
                app_ref,
            ),
            genesis_cache_root(
                &state,
                install("tenant-a", InstallSource::Local("default")),
                app_ref,
            ),
            genesis_cache_root(
                &state,
                CacheOwner::Export {
                    registry_tenant: "default",
                },
                app_ref,
            ),
            genesis_cache_root(
                &state,
                CacheOwner::DependencyRead { tenant: "default" },
                app_ref,
            ),
        ];
        let distinct: BTreeSet<_> = roots.iter().collect();
        assert_eq!(distinct.len(), roots.len(), "{roots:?}");
        assert_eq!(
            roots[0],
            genesis_cache_root(
                &state,
                install("tenant-a", InstallSource::Registry("default")),
                app_ref
            ),
            "the same install resolves to the same directory"
        );
    }

    /// `app_name` written into `root`, as an install names it.
    fn write_catalog_app(
        root: &Path,
        app_name: &str,
        marker: &str,
    ) -> Vec<(String, AppFileSource)> {
        let app_dir = root.join(app_name);
        std::fs::create_dir_all(&app_dir).expect("app dir");
        std::fs::write(app_dir.join("app.toml"), format!("name = \"{app_name}\"\n"))
            .expect("app.toml");
        std::fs::write(
            app_dir.join("APP.md"),
            format!("# {app_name}\n\n{marker}\n"),
        )
        .expect("APP.md");
        let paths = ["app.toml".to_string(), "APP.md".to_string()];
        vec![(app_name.to_string(), AppFileSource::written(app_dir, paths))]
    }

    #[test]
    fn the_platform_tenants_installs_are_the_shared_catalog() {
        let app_name = format!("shared-install-{}", uuid::Uuid::new_v4());
        let root = tempfile::tempdir().expect("cache root");
        let written = write_catalog_app(root.path(), &app_name, "PLATFORM-INSTALL");

        register_written_apps(
            TenantId::default().as_str(),
            root.path().to_path_buf(),
            written,
        );

        let other_tenant = format!("tenant-{}", uuid::Uuid::new_v4());
        let guide = crate::os_apps::tenant_app_guide(&other_tenant, &app_name)
            .expect("every tenant resolves the platform tenant's install");
        assert!(guide.contains("PLATFORM-INSTALL"));
        assert!(crate::os_apps::get_os_app(&app_name).is_some());
    }

    #[test]
    fn a_tenant_install_does_not_change_another_tenants_app() {
        let app_name = format!("tenant-install-{}", uuid::Uuid::new_v4());
        let shared = tempfile::tempdir().expect("shared root");
        let written = write_catalog_app(shared.path(), &app_name, "SHARED");
        add_written_apps_preferred(shared.path().to_path_buf(), written);
        let tenant_a = format!("tenant-a-{}", uuid::Uuid::new_v4());
        let tenant_b = format!("tenant-b-{}", uuid::Uuid::new_v4());
        let root_a = tempfile::tempdir().expect("tenant a root");
        let written_a = write_catalog_app(root_a.path(), &app_name, "TENANT-A");
        let root_b = tempfile::tempdir().expect("tenant b root");
        let written_b = write_catalog_app(root_b.path(), &app_name, "TENANT-B");

        register_written_apps(&tenant_a, root_a.path().to_path_buf(), written_a);

        let guide =
            |tenant: &str| crate::os_apps::tenant_app_guide(tenant, &app_name).expect("guide");
        assert!(guide(&tenant_a).contains("TENANT-A"));
        assert!(guide(&tenant_b).contains("SHARED"));
        assert!(guide(TenantId::default().as_str()).contains("SHARED"));
        assert!(
            crate::os_apps::list_os_apps()
                .iter()
                .filter(|entry| entry.name == app_name)
                .all(|entry| entry
                    .app_guide
                    .as_deref()
                    .is_some_and(|g| g.contains("SHARED")))
        );

        register_written_apps(&tenant_b, root_b.path().to_path_buf(), written_b);

        assert!(guide(&tenant_a).contains("TENANT-A"));
        assert!(guide(&tenant_b).contains("TENANT-B"));
        let listed: Vec<_> = crate::os_apps::list_tenant_os_apps(&tenant_b)
            .into_iter()
            .filter(|entry| entry.name == app_name)
            .collect();
        assert_eq!(listed.len(), 1);
        assert!(
            listed[0]
                .app_guide
                .as_deref()
                .is_some_and(|g| g.contains("TENANT-B"))
        );
    }

    #[test]
    fn parses_pinned_registry_app_refs() {
        let parsed = parse_registry_app_ref("temperpaw/paw-agent@abc123").expect("valid app ref");
        assert_eq!(parsed.owner, "temperpaw");
        assert_eq!(parsed.name, "paw-agent");
        assert_eq!(parsed.version_hash.as_deref(), Some("abc123"));
        assert!(parse_registry_app_ref("paw-agent").is_err());
        assert!(parse_registry_app_ref("temperpaw/paw-agent@").is_err());
        assert!(parse_registry_app_ref("../paw-agent@abc123").is_err());
        assert!(parse_registry_app_ref("temperpaw/../../target@abc123").is_err());
        assert!(parse_registry_app_ref("temperpaw//tmp/target@abc123").is_err());
    }

    #[test]
    fn closure_admission_rejects_version_and_directory_collisions() {
        let app = |owner: &str, name: &str, version: &str| GenesisAppBundle {
            owner: owner.to_string(),
            name: name.to_string(),
            repository_id: format!("repo-{owner}-{name}"),
            version_hash: version.to_string(),
        };
        let mut admission = GenesisClosureAdmission::default();
        assert!(admission.admit(&app("acme", "notes", "v1")).unwrap());
        assert!(!admission.admit(&app("acme", "notes", "v1")).unwrap());

        let version_error = admission
            .admit(&app("acme", "notes", "v2"))
            .expect_err("one dependency cannot resolve to two versions");
        assert!(version_error.contains("conflicting versions"));

        let directory_error = admission
            .admit(&app("other", "notes", "v1"))
            .expect_err("different owners cannot share one cache directory");
        assert!(directory_error.contains("collide on cache directory"));
    }

    #[test]
    fn install_ref_honors_pinned_app_ref_over_latest() {
        let resolved = resolve_install_app_ref(
            "nerdsane",
            "agent-answers",
            "latest123",
            Some("nerdsane/agent-answers@variant456"),
        )
        .expect("pinned ref should resolve");

        assert_eq!(resolved.app_ref, "nerdsane/agent-answers@variant456");
        assert_eq!(resolved.version_hash, "variant456");
    }

    #[test]
    fn install_ref_rejects_mismatched_app_ref() {
        let error = resolve_install_app_ref(
            "nerdsane",
            "agent-answers",
            "latest123",
            Some("nerdsane/other-app@variant456"),
        )
        .expect_err("mismatched app ref should fail");

        assert!(error.contains("does not match App row"));
    }

    #[test]
    fn install_ref_defaults_to_latest_when_absent_or_unpinned() {
        let absent = resolve_install_app_ref("owner", "app", "@latest123", None)
            .expect("absent app ref should use latest");
        assert_eq!(absent.app_ref, "owner/app@latest123");
        assert_eq!(absent.version_hash, "latest123");

        let unpinned = resolve_install_app_ref("owner", "app", "@latest123", Some("owner/app"))
            .expect("unpinned app ref should use latest");
        assert_eq!(unpinned.app_ref, "owner/app@latest123");
        assert_eq!(unpinned.version_hash, "latest123");
    }

    #[test]
    fn genesis_install_follow_policy_defaults_to_pinned() {
        assert_eq!(normalize_follow_policy("").unwrap(), "pinned");
        assert_eq!(normalize_follow_policy("pinned").unwrap(), "pinned");
        assert_eq!(
            normalize_follow_policy("follow-latest").unwrap(),
            "follow_latest"
        );
        assert!(normalize_follow_policy("auto_everywhere").is_err());
    }

    #[test]
    fn follow_latest_preserves_original_pinned_hash() {
        let existing = InstalledAppRecord {
            tenant: "tenant-a".to_string(),
            app_name: "notes".to_string(),
            source_kind: "genesis".to_string(),
            app_ref: "acme/notes@1111".to_string(),
            version_hash: "2222".to_string(),
            pinned_version_hash: "1111".to_string(),
            current_version_hash: "2222".to_string(),
            follow_policy: "follow_latest".to_string(),
            closure_id: "genesis:acme/notes@2222:2222".to_string(),
            registry_url: "https://genesis.example".to_string(),
            registry_tenant: "default".to_string(),
            app_version: "0.1.0".to_string(),
            bundle_digest: "sha256:bundle".to_string(),
            spec_digest: "sha256:spec".to_string(),
            policy_digest: "sha256:policy".to_string(),
            wasm_digest: "sha256:wasm".to_string(),
            content_digest: "sha256:content".to_string(),
            seed_digest: "sha256:seed".to_string(),
            installed_at: None,
            last_reconciled_at: None,
            status: "installed".to_string(),
        };

        let (pinned, current) =
            provenance_hashes_for_policy("follow_latest", "@3333", Some(&existing));
        assert_eq!(pinned, "1111");
        assert_eq!(current, "3333");
    }

    #[test]
    fn pinned_policy_resets_pinned_and_current_hashes() {
        let existing = InstalledAppRecord {
            tenant: "tenant-a".to_string(),
            app_name: "notes".to_string(),
            source_kind: "genesis".to_string(),
            app_ref: "acme/notes@1111".to_string(),
            version_hash: "2222".to_string(),
            pinned_version_hash: "1111".to_string(),
            current_version_hash: "2222".to_string(),
            follow_policy: "follow_latest".to_string(),
            closure_id: "genesis:acme/notes@2222:2222".to_string(),
            registry_url: "https://genesis.example".to_string(),
            registry_tenant: "default".to_string(),
            app_version: "0.1.0".to_string(),
            bundle_digest: "sha256:bundle".to_string(),
            spec_digest: "sha256:spec".to_string(),
            policy_digest: "sha256:policy".to_string(),
            wasm_digest: "sha256:wasm".to_string(),
            content_digest: "sha256:content".to_string(),
            seed_digest: "sha256:seed".to_string(),
            installed_at: None,
            last_reconciled_at: None,
            status: "installed".to_string(),
        };

        let (pinned, current) = provenance_hashes_for_policy("pinned", "4444", Some(&existing));
        assert_eq!(pinned, "4444");
        assert_eq!(current, "4444");
    }

    #[test]
    fn registry_git_urls_are_stable() {
        assert_eq!(
            registry_git_url(
                &registry::registry_for_test("https://genesis.example/"),
                "temperpaw",
                "paw-agent"
            ),
            "https://genesis.example/temperpaw/paw-agent.git"
        );
    }

    #[test]
    fn registry_app_id_components_match_genesis_convention() {
        assert_eq!(sanitize_registry_id_component("Acme Labs"), "acme-labs");
        assert_eq!(
            sanitize_registry_id_component("katagami_commons"),
            "katagami-commons"
        );
        assert_eq!(sanitize_registry_id_component("../"), "item");
    }

    #[test]
    fn bundle_paths_must_be_safe_package_files() {
        assert_eq!(
            safe_bundle_relative_path("wasm/echo/echo.wasm").unwrap(),
            PathBuf::from("wasm").join("echo").join("echo.wasm")
        );
        assert!(safe_bundle_relative_path("../app.toml").is_err());
        assert!(safe_bundle_relative_path("/tmp/app.toml").is_err());
        assert!(safe_bundle_relative_path("wasm/echo/target/debug/echo.wasm").is_err());
        assert!(safe_bundle_relative_path(".git/config").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn bundle_collection_rejects_symbolic_links() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("bundle root");
        let outside = tempfile::NamedTempFile::new().expect("outside file");
        symlink(outside.path(), root.path().join("leak")).expect("symlink");

        let named = BTreeSet::from(["leak".to_string()]);
        let error = collect_bundle_files(root.path(), &named, &mut GenesisBundleBudget::new())
            .expect_err("symlink must be rejected");
        assert!(error.contains("symbolic link"));
    }

    #[test]
    fn manifest_read_rejects_oversized_file_before_allocation() {
        let root = tempfile::tempdir().expect("app root");
        let manifest = std::fs::File::create(root.path().join("app.toml")).expect("manifest");
        manifest
            .set_len(MAX_GENESIS_MANIFEST_BYTES + 1)
            .expect("oversized manifest");

        let error = read_manifest_dependencies(root.path()).expect_err("manifest budget");
        assert!(error.contains("budget"));
    }

    #[test]
    fn write_bundle_app_materializes_base64_files() {
        let temp_dir = std::env::temp_dir().join(format!(
            "temper-genesis-bundle-write-{}",
            uuid::Uuid::new_v4()
        ));
        let app = GenesisRegistryBundleApp {
            owner: "owner".to_string(),
            name: "notes".to_string(),
            version_hash: "abc123".to_string(),
            files: vec![GenesisRegistryBundleFile {
                path: "app.toml".to_string(),
                content_base64: base64::engine::general_purpose::STANDARD
                    .encode(b"name = \"notes\"\n"),
            }],
        };

        let written = write_bundle_app(&temp_dir, &app).expect("bundle should materialize");
        assert_eq!(written, vec!["app.toml".to_string()]);
        assert_eq!(
            std::fs::read_to_string(temp_dir.join("app.toml")).unwrap(),
            "name = \"notes\"\n"
        );
        let _ = std::fs::remove_dir_all(&temp_dir);
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

    fn blob_stream_test_state(data_dir: &Path) -> ServerState {
        let mut state = ServerState::new(
            ActorSystem::new("genesis-stream-test"),
            CsdlDocument {
                version: "4.0".to_string(),
                schemas: Vec::new(),
            },
            String::new(),
        );
        state.data_dir = data_dir.to_path_buf();
        state
    }

    async fn write_overflow_object(
        data_dir: &Path,
        key: &str,
        serialized: &[u8],
    ) -> std::path::PathBuf {
        let path = data_dir.join("blobs").join(key);
        tokio::fs::create_dir_all(path.parent().expect("overflow parent"))
            .await
            .expect("create overflow parent");
        tokio::fs::write(&path, serialized)
            .await
            .expect("write overflow object");
        path
    }

    #[tokio::test]
    async fn genesis_materializes_large_blob_content_from_stream() {
        let data_dir = tempfile::tempdir().expect("Genesis data dir");
        let output_dir = tempfile::tempdir().expect("Genesis output dir");
        let state = blob_stream_test_state(data_dir.path());
        let content = vec![0x6bu8; 2 * 1024 * 1024];
        let serialized =
            serde_json::to_vec(&base64::engine::general_purpose::STANDARD.encode(&content))
                .expect("serialize overflow JSON string");
        let key = format!(
            "field-overflow/sha256/{:x}.json",
            sha2::Sha256::digest(&serialized)
        );
        write_overflow_object(data_dir.path(), &key, &serialized).await;
        let fields = serde_json::json!({
            "Size": content.len() as u64,
            "Content": {
                "__temper_blob_ref": key,
                "__temper_blob_size": serialized.len() as u64,
                "__temper_blob_encoding": "json",
                "__temper_blob_encoding": "json",
            }
        });
        let destination = output_dir.path().join("large.bin");

        materialize_blob_content_field(
            &state,
            &TenantId::default(),
            &fields,
            &destination,
            MAX_GENESIS_BUNDLE_FILE_BYTES,
        )
        .await
        .expect("stream materialization");

        assert_eq!(
            tokio::fs::read(&destination)
                .await
                .expect("materialized file"),
            content
        );
    }

    #[tokio::test]
    async fn genesis_malformed_stream_never_replaces_destination() {
        let data_dir = tempfile::tempdir().expect("Genesis data dir");
        let output_dir = tempfile::tempdir().expect("Genesis output dir");
        let state = blob_stream_test_state(data_dir.path());
        let serialized = b"\"!!!!\"";
        let key = format!(
            "field-overflow/sha256/{:x}.json",
            sha2::Sha256::digest(serialized)
        );
        write_overflow_object(data_dir.path(), &key, serialized).await;
        let fields = serde_json::json!({
            "Size": 3,
            "Content": {
                "__temper_blob_ref": key,
                "__temper_blob_size": 6,
                "__temper_blob_encoding": "json",
                "__temper_blob_encoding": "json",
            }
        });
        let destination = output_dir.path().join("existing.bin");
        tokio::fs::write(&destination, b"existing")
            .await
            .expect("seed destination");

        let error = materialize_blob_content_field(
            &state,
            &TenantId::default(),
            &fields,
            &destination,
            MAX_GENESIS_BUNDLE_FILE_BYTES,
        )
        .await
        .expect_err("malformed stream must fail");

        assert!(error.contains("decode Genesis Blob.Content"));
        assert_eq!(
            tokio::fs::read(&destination)
                .await
                .expect("existing destination"),
            b"existing"
        );
        let staged = std::fs::read_dir(output_dir.path())
            .expect("list output dir")
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".genesis-blob-")
            })
            .count();
        assert_eq!(staged, 0, "RAII removes failed staged files");
    }
}
