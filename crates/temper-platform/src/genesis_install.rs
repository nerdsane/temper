//! Genesis app installation bridge.
//!
//! Specs own the public action (`App.Install`). This hook only runs after that
//! governed action has succeeded, then materializes the pinned Genesis commit
//! into the platform's app installer.

use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use temper_runtime::tenant::TenantId;
use temper_server::platform_store::InstalledAppRecord;
use temper_server::state::{BoundActionHook, BoundActionHookContext, DispatchCommand, ServerState};

use crate::os_apps::{
    AppManifest, InstallResult, OsAppReconcileResult, add_os_apps_dir_preferred,
    os_app_bundle_digest, reconcile_os_app, resolve_os_app_install_order,
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
        let registry_tenant = if record.registry_tenant.trim().is_empty() {
            "default"
        } else {
            record.registry_tenant.trim()
        };
        let materialized = match materialize_registry_app_closure_via_bundle(
            &record.registry_url,
            registry_tenant,
            root_ref.clone(),
            &cache_root,
        )
        .await
        {
            Ok(materialized) => Ok(materialized),
            Err(error) if genesis_git_fallback_enabled() => {
                tracing::warn!(
                    tenant = %tenant,
                    app = %app_name,
                    app_ref = %record.app_ref,
                    registry_url = %record.registry_url,
                    error = %error,
                    "Genesis bundle cache recovery failed; falling back to git clone because TEMPER_GENESIS_INSTALL_GIT_FALLBACK is enabled"
                );
                materialize_registry_app_closure(&record.registry_url, root_ref, &cache_root).await
            }
            Err(error) => Err(error),
        };
        match materialized {
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
        let registry_url = match normalize_registry_url(&record.registry_url) {
            Ok(url) => url,
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
            &registry_url,
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
            registry_url,
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
/// instance's storage backend.
pub async fn install_genesis_app_from_registry(
    platform: &PlatformState,
    request: GenesisRegistryInstallRequest,
) -> Result<GenesisRegistryInstallResult, String> {
    let install_started = Instant::now();
    let registry_url = normalize_registry_url(&request.registry_url)?;
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
    let cache_root = genesis_cache_root(&platform.server, &cache_key);
    std::fs::create_dir_all(&cache_root).map_err(|error| {
        format!(
            "create Genesis registry cache '{}': {error}",
            cache_root.display()
        )
    })?;

    let materialize_started = Instant::now();
    let materialized_refs = match materialize_registry_app_closure_via_bundle(
        &registry_url,
        &registry_tenant,
        root_ref.clone(),
        &cache_root,
    )
    .await
    {
        Ok(refs) => {
            log_genesis_install_phase(
                &request.app_ref,
                "materialize_bundle",
                materialize_started,
                refs.len(),
                0,
            );
            refs
        }
        Err(error) if genesis_git_fallback_enabled() => {
            tracing::warn!(
                app_ref = %request.app_ref,
                registry_url = %registry_url,
                error = %error,
                "Genesis bundle fetch failed; falling back to git clone because TEMPER_GENESIS_INSTALL_GIT_FALLBACK is enabled"
            );
            let git_started = Instant::now();
            let refs =
                materialize_registry_app_closure(&registry_url, root_ref.clone(), &cache_root)
                    .await?;
            log_genesis_install_phase(
                &request.app_ref,
                "materialize_git_fallback",
                git_started,
                refs.len(),
                0,
            );
            refs
        }
        Err(error) => {
            return Err(format!(
                "Genesis bundle fetch failed for {} from {}: {error}. Git fallback is disabled; set TEMPER_GENESIS_INSTALL_GIT_FALLBACK=1 only for admin/debug recovery.",
                request.app_ref, registry_url
            ));
        }
    };
    let materialized: Vec<String> = materialized_refs
        .iter()
        .map(|app_ref| app_ref.name.clone())
        .collect();

    add_os_apps_dir_preferred(cache_root.clone());

    let install_platform = platform.clone();
    let reconcile_started = Instant::now();
    let install =
        reconcile_materialized_app_closure(&install_platform, &request.tenant, &root_ref.name)
            .await?;
    log_genesis_install_phase(
        &request.app_ref,
        "install_reconcile",
        reconcile_started,
        materialized.len(),
        install.wasm_modules.len(),
    );
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
                follow_policy: &follow_policy,
            },
        )
        .await;
    }
    log_genesis_install_phase(
        &request.app_ref,
        "total",
        install_started,
        materialized.len(),
        0,
    );

    Ok(GenesisRegistryInstallResult {
        app_ref: request.app_ref,
        tenant: request.tenant,
        registry_url,
        registry_tenant,
        follow_policy,
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

async fn reconcile_materialized_app_closure(
    platform: &PlatformState,
    tenant: &str,
    root_app_name: &str,
) -> Result<InstallResult, String> {
    let order = resolve_os_app_install_order(&[root_app_name.to_string()])?;
    let mut root_result = InstallResult::default();

    for app_name in order {
        let started = Instant::now();
        match reconcile_os_app(platform, tenant, &app_name).await? {
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
                }
            }
        }
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

/// Deadline for any single registry/bundle HTTP request (connect and total).
const REGISTRY_HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// Cap on a registry JSON row body (App metadata) before decoding.
const MAX_REGISTRY_JSON_BYTES: usize = 4 * 1024 * 1024;
/// Cap on a bundle response body before decoding (a bundle carries base64 app files).
const MAX_BUNDLE_BODY_BYTES: usize = 64 * 1024 * 1024;

/// True only for globally-routable addresses. Loopback, private, link-local,
/// unspecified, broadcast, documentation, multicast, and CGNAT (100.64.0.0/10)
/// ranges are all treated as non-public so a registry can't point the installer
/// at internal infrastructure or a cloud metadata endpoint.
fn ipv4_is_public(v4: &Ipv4Addr) -> bool {
    let octets = v4.octets();
    let is_cgnat = octets[0] == 100 && (octets[1] & 0xc0) == 0x40;
    // 0.0.0.0/8 ("this host on this network") is non-routable and some stacks
    // treat it as localhost, so block the whole block, not just 0.0.0.0.
    let is_this_network = octets[0] == 0;
    !(v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_unspecified()
        || v4.is_broadcast()
        || v4.is_documentation()
        || v4.is_multicast()
        || is_cgnat
        || is_this_network)
}

fn ip_is_public(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => ipv4_is_public(v4),
        IpAddr::V6(v6) => {
            if v6.is_loopback() || v6.is_unspecified() || v6.is_multicast() {
                return false;
            }
            let segments = v6.segments();
            // Both IPv4-mapped (::ffff:a.b.c.d) and the deprecated IPv4-compatible
            // (::a.b.c.d) forms carry an embedded IPv4 host, so judge them by the
            // IPv4 rules — otherwise ::ffff:127.0.0.1 or ::7f00:1 (::127.0.0.1)
            // would slip past as "some v6 address". Handle each form explicitly
            // rather than via the dual-range `to_ipv4()`, so the classification
            // never depends on that method's exact semantics.
            if let Some(v4) = v6.to_ipv4_mapped() {
                return ipv4_is_public(&v4);
            }
            if segments[..6].iter().all(|&s| s == 0) {
                let v4 = Ipv4Addr::new(
                    (segments[6] >> 8) as u8,
                    (segments[6] & 0xff) as u8,
                    (segments[7] >> 8) as u8,
                    (segments[7] & 0xff) as u8,
                );
                return ipv4_is_public(&v4);
            }
            let is_unique_local = (segments[0] & 0xfe00) == 0xfc00;
            let is_link_local = (segments[0] & 0xffc0) == 0xfe80;
            !(is_unique_local || is_link_local)
        }
    }
}

fn registry_host_and_port(url: &str) -> Result<(String, u16), String> {
    let parsed =
        reqwest::Url::parse(url).map_err(|error| format!("parse registry URL '{url}': {error}"))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| format!("registry URL '{url}' has no host"))?;
    // `host_str()` brackets IPv6 literals (`[::1]`); strip them so the host parses
    // as an `IpAddr` and is classified by `ip_is_public`, rather than falling to a
    // resolution failure (which would also wrongly reject a public IPv6 literal).
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host)
        .to_string();
    let port = parsed.port_or_known_default().unwrap_or(0);
    Ok((host, port))
}

/// Resolve `host` and return its addresses, failing closed if *any* resolved
/// address is non-public. Resolution runs on the blocking pool so it never
/// stalls the async runtime.
async fn resolve_public_addrs(host: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        if !ip_is_public(&ip) {
            return Err(format!(
                "registry host '{host}' is a non-public address {ip}; refusing to fetch (SSRF guard)"
            ));
        }
        return Ok(vec![SocketAddr::new(ip, port)]);
    }
    let host_owned = host.to_string();
    let addrs = tokio::task::spawn_blocking(move || {
        (host_owned.as_str(), port)
            .to_socket_addrs()
            .map(|iter| iter.collect::<Vec<_>>())
    })
    .await
    .map_err(|error| format!("resolve registry host '{host}' task failed: {error}"))?
    .map_err(|error| format!("resolve registry host '{host}': {error}"))?;
    if addrs.is_empty() {
        return Err(format!(
            "registry host '{host}' did not resolve to any address"
        ));
    }
    for addr in &addrs {
        if !ip_is_public(&addr.ip()) {
            return Err(format!(
                "registry host '{host}' resolves to non-public address {}; refusing to fetch (SSRF guard)",
                addr.ip()
            ));
        }
    }
    Ok(addrs)
}

/// SSRF guard for the git-clone fallback, which egresses through the git binary
/// rather than our HTTP client and so needs the host checked independently. Note
/// this only validates the resolved address at check time; git performs its own
/// DNS lookup at connect, so a rebinding attacker retains a narrow TOCTOU window
/// here (unlike the address-pinned HTTP client). The fallback is off by default
/// (`TEMPER_GENESIS_INSTALL_GIT_FALLBACK`, admin/debug recovery only); pinning
/// git's resolution is tracked as an ADR follow-up.
async fn assert_registry_host_is_public(url: &str) -> Result<(), String> {
    let (host, port) = registry_host_and_port(url)?;
    resolve_public_addrs(&host, port).await.map(|_| ())
}

/// A hardened HTTP client for one registry/bundle URL: bounded deadlines,
/// redirects disabled, and — for DNS names — pinned to the exact public
/// addresses we just checked, so a rebinding second lookup can't swing the
/// connection onto an internal host after the check (no TOCTOU window).
async fn guarded_registry_client(url: &str) -> Result<reqwest::Client, String> {
    let (host, port) = registry_host_and_port(url)?;
    let addrs = resolve_public_addrs(&host, port).await?;
    let mut builder = reqwest::Client::builder()
        .connect_timeout(REGISTRY_HTTP_TIMEOUT)
        .timeout(REGISTRY_HTTP_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none());
    if host.parse::<IpAddr>().is_err() {
        // Pin every checked address in one call — a per-address `resolve` loop
        // would keep only the last, dropping failover across A/AAAA records.
        builder = builder.resolve_to_addrs(&host, &addrs);
    }
    builder
        .build()
        .map_err(|error| format!("build hardened registry client: {error}"))
}

/// Read a response body under a byte budget, rejecting an oversized
/// `Content-Length` up front and any body that streams past the cap.
async fn read_capped_body(
    mut response: reqwest::Response,
    max_bytes: usize,
    what: &str,
) -> Result<Vec<u8>, String> {
    if let Some(len) = response.content_length()
        && len > max_bytes as u64
    {
        return Err(format!(
            "{what} advertises {len} bytes, over the {max_bytes}-byte cap"
        ));
    }
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("read {what} body: {error}"))?
    {
        if buf.len().saturating_add(chunk.len()) > max_bytes {
            return Err(format!("{what} body exceeds the {max_bytes}-byte cap"));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
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
    registry_url: &str,
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
        registry_url.trim_end_matches('/'),
        app_id.replace('\'', "''")
    );
    let response = guarded_registry_client(&url)
        .await?
        .get(&url)
        .header("X-Tenant-Id", registry_tenant)
        .send()
        .await
        .map_err(|error| format!("request Genesis App row {url}: {error}"))?;
    let status = response.status();
    if !status.is_success() {
        let body = read_capped_body(response, MAX_REGISTRY_JSON_BYTES, "Genesis App row error")
            .await
            .unwrap_or_default();
        return Err(format!(
            "request Genesis App row {url} returned {status}: {}",
            String::from_utf8_lossy(&body).trim()
        ));
    }
    let bytes = read_capped_body(
        response,
        MAX_REGISTRY_JSON_BYTES,
        &format!("Genesis App row {url}"),
    )
    .await?;
    let row: Value = serde_json::from_slice(&bytes)
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
    assert_registry_host_is_public(registry_url).await?;
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

        let app_dir = bundle_app_dir(cache_root, &app_ref.name)?;
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

async fn materialize_registry_app_closure_via_bundle(
    registry_url: &str,
    registry_tenant: &str,
    root_ref: RegistryAppRef,
    cache_root: &Path,
) -> Result<Vec<RegistryAppRef>, String> {
    let Some(version_hash) = root_ref.version_hash.as_deref() else {
        return Err("bundle fetch requires a pinned root app ref".to_string());
    };
    let bundle_url = format!(
        "{}/api/genesis/apps/{}/{}/versions/{}/bundle",
        registry_url.trim_end_matches('/'),
        root_ref.owner,
        root_ref.name,
        version_hash.trim_start_matches('@')
    );
    let response = guarded_registry_client(&bundle_url)
        .await?
        .get(&bundle_url)
        .header("X-Tenant-Id", registry_tenant)
        .send()
        .await
        .map_err(|error| format!("request Genesis bundle {bundle_url}: {error}"))?;
    let status = response.status();
    if !status.is_success() {
        let body = read_capped_body(response, MAX_REGISTRY_JSON_BYTES, "Genesis bundle error")
            .await
            .unwrap_or_default();
        return Err(format!(
            "request Genesis bundle {bundle_url} returned {status}: {}",
            String::from_utf8_lossy(&body).trim()
        ));
    }
    let bytes = read_capped_body(
        response,
        MAX_BUNDLE_BODY_BYTES,
        &format!("Genesis bundle {bundle_url}"),
    )
    .await?;
    let bundle: GenesisRegistryBundleResponse = serde_json::from_slice(&bytes)
        .map_err(|error| format!("decode Genesis bundle {bundle_url}: {error}"))?;

    if cache_root.exists() {
        std::fs::remove_dir_all(cache_root).map_err(|error| {
            format!(
                "clear Genesis registry bundle cache '{}': {error}",
                cache_root.display()
            )
        })?;
    }
    std::fs::create_dir_all(cache_root).map_err(|error| {
        format!(
            "create Genesis registry bundle cache '{}': {error}",
            cache_root.display()
        )
    })?;

    let mut refs = Vec::new();
    for app in bundle.apps {
        let app_dir = bundle_app_dir(cache_root, &app.name)?;
        write_bundle_app(&app_dir, &app)?;
        refs.push(RegistryAppRef {
            owner: app.owner,
            name: app.name,
            version_hash: Some(app.version_hash),
        });
    }
    Ok(refs)
}

fn write_bundle_app(app_dir: &Path, app: &GenesisRegistryBundleApp) -> Result<(), String> {
    if app_dir.exists() {
        std::fs::remove_dir_all(app_dir).map_err(|error| {
            format!("clear Genesis bundle app '{}': {error}", app_dir.display())
        })?;
    }
    std::fs::create_dir_all(app_dir)
        .map_err(|error| format!("create Genesis bundle app '{}': {error}", app_dir.display()))?;

    for file in &app.files {
        let rel = safe_bundle_relative_path(&file.path)?;
        let path = app_dir.join(&rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                format!("create bundle file parent '{}': {error}", parent.display())
            })?;
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&file.content_base64)
            .map_err(|error| format!("decode bundle file '{}': {error}", file.path))?;
        std::fs::write(&path, bytes)
            .map_err(|error| format!("write bundle file '{}': {error}", path.display()))?;
    }
    Ok(())
}

fn safe_bundle_relative_path(path: &str) -> Result<PathBuf, String> {
    let rel = PathBuf::from(path);
    if rel.as_os_str().is_empty() {
        return Err("bundle file path must not be empty".to_string());
    }
    let mut safe = PathBuf::new();
    for component in rel.components() {
        match component {
            Component::Normal(part) => {
                if part == "target" || part == ".git" {
                    return Err(format!(
                        "bundle file path '{}' contains forbidden component '{}'",
                        path,
                        part.to_string_lossy()
                    ));
                }
                safe.push(part);
            }
            _ => {
                return Err(format!(
                    "bundle file path '{}' must be relative and must not contain '..'",
                    path
                ));
            }
        }
    }
    Ok(safe)
}

/// Resolve the per-app cache directory for a registry app. The app name comes
/// from a remote registry response, so it must be a single safe path component:
/// an unvalidated name like `../../etc` or `/etc` would let a malicious registry
/// escape the cache root and drive `remove_dir_all` + writes at an arbitrary
/// filesystem location (ARN-210).
fn bundle_app_dir(cache_root: &Path, app_name: &str) -> Result<PathBuf, String> {
    // The name must be exactly one `Normal` path component: `..` parses as
    // `ParentDir`, an absolute path starts with `RootDir`/`Prefix`, and a nested
    // name yields more than one component — all rejected, so the result can only
    // ever be a direct child of `cache_root`.
    let mut components = Path::new(app_name).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(part)), None) => Ok(cache_root.join(part)),
        _ => Err(format!(
            "registry app name '{app_name}' must be a single relative path component \
             (no empty, '/', '..', or absolute paths)"
        )),
    }
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
        let app_dir = bundle_app_dir(&cache_root, &name)?;
        add_os_apps_dir_preferred(cache_root);

        let mut platform = self.platform.clone();
        platform.server = state.clone();
        match reconcile_materialized_app_closure(&platform, &target_tenant, &name).await {
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
                        follow_policy: &follow_policy,
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
        source_kind: "genesis".to_string(),
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

pub async fn export_genesis_registry_bundle(
    platform: &PlatformState,
    registry_tenant: &str,
    owner: &str,
    name: &str,
    version_hash: &str,
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
    let cache_root = genesis_cache_root(&platform.server, &app_ref);
    let closure = resolve_genesis_app_closure(&platform.server, &tenant, root).await?;
    let mut apps = Vec::new();

    for app in closure {
        let app_dir = bundle_app_dir(&cache_root, &app.name)?;
        let started = Instant::now();
        materialize_commit_tree(
            &platform.server,
            &tenant,
            &app.repository_id,
            &app.version_hash,
            &app_dir,
        )
        .await?;
        let files = collect_bundle_files(&app_dir)?;
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
) -> Result<Vec<GenesisAppBundle>, String> {
    let mut stack = vec![root];
    let mut seen = BTreeSet::new();
    let mut closure = Vec::new();

    while let Some(app) = stack.pop() {
        let key = format!("{}/{}", app.owner, app.name);
        if !seen.insert(key) {
            continue;
        }
        let cache_root = genesis_cache_root(
            state,
            &format!(
                "{}-{}-dependency-read-{}",
                app.owner,
                app.name,
                app.version_hash.trim_start_matches('@')
            ),
        );
        let app_dir = bundle_app_dir(&cache_root, &app.name)?;
        materialize_commit_tree(
            state,
            tenant,
            &app.repository_id,
            &app.version_hash,
            &app_dir,
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
            if !seen.contains(&format!("{}/{}", dependency.owner, dependency.name)) {
                stack.push(dependency);
            }
        }
        closure.push(app);
    }

    Ok(closure)
}

fn collect_bundle_files(app_dir: &Path) -> Result<Vec<GenesisRegistryBundleFile>, String> {
    let mut paths = Vec::new();
    collect_bundle_file_paths(app_dir, app_dir, &mut paths)?;
    paths.sort();
    let mut files = Vec::new();
    for path in paths {
        let rel = path
            .strip_prefix(app_dir)
            .map_err(|error| format!("strip bundle path '{}': {error}", path.display()))?
            .to_string_lossy()
            .replace('\\', "/");
        let bytes = std::fs::read(&path)
            .map_err(|error| format!("read bundle file '{}': {error}", path.display()))?;
        files.push(GenesisRegistryBundleFile {
            path: rel,
            content_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
        });
    }
    Ok(files)
}

fn collect_bundle_file_paths(
    root: &Path,
    dir: &Path,
    paths: &mut Vec<PathBuf>,
) -> Result<(), String> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .map_err(|error| format!("read bundle directory '{}': {error}", dir.display()))?
        .filter_map(|entry| entry.ok())
        .collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .map_err(|error| format!("strip bundle path '{}': {error}", path.display()))?;
        if rel.components().any(|component| {
            matches!(component, Component::Normal(part) if part == "target" || part == ".git")
        }) {
            if path.is_dir() {
                tracing::warn!(
                    path = %path.display(),
                    "Skipping forbidden generated directory in Genesis bundle export"
                );
            }
            continue;
        }
        if path.is_dir() {
            collect_bundle_file_paths(root, &path, paths)?;
        } else if path.is_file() {
            paths.push(path);
        }
    }
    Ok(())
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

        let app_dir = bundle_app_dir(cache_root, &app.name)?;
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

/// Recompute the durable entity id Genesis assigns to a git object.
///
/// Git objects are persisted keyed by `{sanitized_repository_id}-{git_sha}`.
/// This MUST stay byte-identical to `object_entity_id`, the writer, in the
/// genesis app bundle at `wasm/scm_ingest_pack/src/lib.rs` (arni-labs/genesis);
/// any divergence makes the keyed lookup miss and reintroduces the bundle 404.
/// The contract is exercised end-to-end by the genesis repo's
/// `scripts/live-genesis-install-e2e-smoke.sh` push→bundle round-trip.
fn genesis_object_entity_id(repository_id: &str, git_sha: &str) -> String {
    let mut repo = String::with_capacity(repository_id.len());
    let mut last_dash = false;
    for ch in repository_id.chars() {
        if ch.is_ascii_alphanumeric() {
            repo.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            repo.push('-');
            last_dash = true;
        }
    }
    let repo = repo.trim_matches('-');
    if repo.is_empty() {
        format!("obj-{git_sha}")
    } else {
        format!("{repo}-{git_sha}")
    }
}

/// Resolve a git object (Commit/Tree/Blob) by its durable entity key.
///
/// Objects are content-addressed under `{repository_id}-{git_sha}`, so we load
/// that key directly (hydrating from the event store when the actor is cold).
/// A bare-sha fallback covers any legacy object stored before the composite-key
/// scheme. The previous implementation looked up the bare sha — which is never
/// the real key — and then scanned `list_entity_ids_lazy`, whose partially
/// populated in-memory index could omit durable objects; that made the Genesis
/// bundle export 404 with "blob not found" for objects that existed and cloned
/// cleanly. Keyed lookup is both correct and O(1) instead of O(objects).
async fn load_genesis_object(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    repository_id: &str,
    git_sha: &str,
) -> Result<Option<temper_server::EntityResponse>, String> {
    debug_assert!(!git_sha.is_empty(), "git object sha must not be empty");

    let composite_id = genesis_object_entity_id(repository_id, git_sha);
    if let Some(found) = load_genesis_object_by_key(
        state,
        tenant,
        entity_type,
        repository_id,
        git_sha,
        &composite_id,
    )
    .await?
    {
        return Ok(Some(found));
    }

    // Legacy objects predating the composite-key scheme were keyed by bare sha.
    // `composite_id` is `{repo}-{sha}` or `obj-{sha}`, so it never equals a
    // non-empty bare sha; the guard only avoids a redundant duplicate lookup.
    if composite_id != git_sha
        && let Some(found) =
            load_genesis_object_by_key(state, tenant, entity_type, repository_id, git_sha, git_sha)
                .await?
    {
        return Ok(Some(found));
    }

    Ok(None)
}

/// Load one candidate entity id and confirm it is the requested git object.
async fn load_genesis_object_by_key(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    repository_id: &str,
    git_sha: &str,
    entity_id: &str,
) -> Result<Option<temper_server::EntityResponse>, String> {
    if !state
        .ensure_entity_loaded(tenant, entity_type, entity_id)
        .await
    {
        return Ok(None);
    }
    let found = state
        .get_tenant_entity_state(tenant, entity_type, entity_id)
        .await
        .map_err(|e| format!("read Genesis {entity_type} {entity_id}: {e}"))?;
    let fields = &found.state.fields;
    let object_repo = string_field(fields, "RepositoryId").unwrap_or_default();
    let object_sha = string_field(fields, "Id").unwrap_or_default();
    if object_repo == repository_id && object_sha == git_sha {
        Ok(Some(found))
    } else {
        Ok(None)
    }
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
    use base64::Engine as _;

    use super::*;

    #[test]
    fn bundle_app_dir_rejects_traversal_and_absolute_names() {
        // ARN-210: `app.name` comes from a remote registry bundle. A traversal
        // or absolute name would escape the cache root and drive
        // remove_dir_all + writes at an arbitrary filesystem location.
        let root = Path::new("/var/cache/genesis");
        bundle_app_dir(root, "../../etc").expect_err("traversal app name must be rejected");
        bundle_app_dir(root, "..").expect_err("parent app name must be rejected");
        bundle_app_dir(root, "/etc/passwd").expect_err("absolute app name must be rejected");
        bundle_app_dir(root, "a/b").expect_err("nested app name must be rejected");
        bundle_app_dir(root, "").expect_err("empty app name must be rejected");
        // A normal single-component name is accepted and stays under the root.
        assert_eq!(
            bundle_app_dir(root, "my-app").expect("normal name accepted"),
            root.join("my-app")
        );
    }

    #[test]
    fn ip_is_public_rejects_internal_ranges() {
        // ARN-210 SSRF: a registry_url must not be able to point the installer at
        // loopback, private, link-local, CGNAT, or the cloud metadata endpoint.
        for blocked in [
            "127.0.0.1",
            "10.0.0.5",
            "172.16.4.4",
            "192.168.1.1",
            "169.254.169.254", // AWS/GCP instance metadata
            "100.64.0.1",      // CGNAT / shared address space
            "0.0.0.0",
            "0.1.2.3",          // 0.0.0.0/8 "this network"
            "::1",              // ipv6 loopback (classified, not resolution failure)
            "::ffff:127.0.0.1", // IPv4-mapped loopback
            "::7f00:1",         // deprecated IPv4-compatible ::127.0.0.1
            "fe80::1",          // link-local
            "fc00::1",          // unique local
        ] {
            let ip: IpAddr = blocked.parse().expect("parse test ip");
            assert!(
                !ip_is_public(&ip),
                "{blocked} must be treated as non-public"
            );
        }
        for allowed in ["8.8.8.8", "1.1.1.1", "93.184.216.34", "2606:2800:220:1::"] {
            let ip: IpAddr = allowed.parse().expect("parse test ip");
            assert!(ip_is_public(&ip), "{allowed} must be treated as public");
        }
    }

    #[tokio::test]
    async fn ssrf_guard_rejects_internal_registry_hosts() {
        // IP-literal hosts are checked without any DNS lookup, so these are hermetic.
        assert_registry_host_is_public("http://127.0.0.1:8080/tdata")
            .await
            .expect_err("loopback registry host must be rejected");
        assert_registry_host_is_public("http://169.254.169.254/latest/meta-data")
            .await
            .expect_err("metadata endpoint must be rejected");
        assert_registry_host_is_public("http://[::1]:9000/")
            .await
            .expect_err("ipv6 loopback registry host must be rejected");
        assert_registry_host_is_public("http://10.1.2.3/")
            .await
            .expect_err("private registry host must be rejected");
        // A public IP literal passes the guard (no egress happens in the check).
        assert_registry_host_is_public("https://8.8.8.8/tdata")
            .await
            .expect("public registry host must pass the guard");
    }

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
    fn parses_pinned_registry_app_refs() {
        let parsed = parse_registry_app_ref("temperpaw/paw-agent@abc123").expect("valid app ref");
        assert_eq!(parsed.owner, "temperpaw");
        assert_eq!(parsed.name, "paw-agent");
        assert_eq!(parsed.version_hash.as_deref(), Some("abc123"));
        assert!(parse_registry_app_ref("paw-agent").is_err());
        assert!(parse_registry_app_ref("temperpaw/paw-agent@").is_err());
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
            registry_git_url("https://genesis.example/", "temperpaw", "paw-agent"),
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

        write_bundle_app(&temp_dir, &app).expect("bundle should materialize");
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
}
