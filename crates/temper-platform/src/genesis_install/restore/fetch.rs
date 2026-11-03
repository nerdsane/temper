//! Fetching the installs a boot found neither on disk nor in a cache, without
//! holding the boot.
//!
//! Each stored install is independent of the others, and a registry can be
//! slow for one bundle (a large closure) or not answer at all. So each fetch
//! is its own task: one that stalls costs no other, and the boot goes on while
//! they run. A fetch that lands after the boot has recovered its apps brings
//! its own apps up, so an app whose files arrive late still runs. A fetch that
//! fails says so at WARN, and when all are done a summary says how many were
//! restored and how many were not. Every arrival retries the tenant's apps
//! still awaiting replay, regardless of their install source.

use std::path::PathBuf;
use std::sync::Arc;

use temper_server::platform_store::{InstalledAppRecord, PlatformStore};
use tokio::sync::Mutex;
use tokio::task::{JoinHandle, JoinSet};

use super::super::registry::RegistryUrl;
use super::super::*;
use super::cached_apps_matching_records;
use crate::recovery::{InstalledAppRuntimeRecoveryOutcome, recover_installed_app_runtime_state};

/// A stored install whose files must be fetched again from its registry.
pub(super) struct Pending {
    pub(super) tenant: String,
    pub(super) record: InstalledAppRecord,
    pub(super) registry: RegistryUrl,
    pub(super) registry_tenant: String,
    pub(super) root_ref: RegistryAppRef,
    pub(super) cache_root: PathBuf,
}

/// What the fetches a boot started came to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CacheFetchSummary {
    /// Installs whose files were fetched again. Work superseded by a newer
    /// install is discarded before publication.
    pub fetched: usize,
    /// Installs whose files could not be fetched, or did not hold the
    /// recorded version. Their records are kept.
    pub failed: usize,
}

/// The fetches a boot started. They run whether or not anyone waits.
pub struct CacheFetches(Option<JoinHandle<CacheFetchSummary>>);

impl CacheFetches {
    /// No fetches.
    pub(super) fn none() -> Self {
        Self(None)
    }

    /// Wait for every fetch to end.
    pub async fn finished(self) -> CacheFetchSummary {
        match self.0 {
            Some(handle) => handle.await.unwrap_or_else(|error| {
                tracing::warn!(
                    recovery = "genesis_cache", result = "error", error = %error,
                    "Genesis cache recovery task did not finish"
                );
                CacheFetchSummary::default()
            }),
            None => CacheFetchSummary::default(),
        }
    }
}

/// Start a task for each of `pending`, and one that reports when all end.
pub(super) fn spawn(
    platform: &PlatformState,
    ps: Arc<dyn PlatformStore>,
    pending: Vec<Pending>,
    installed: Vec<(String, String)>,
) -> CacheFetches {
    if pending.is_empty() {
        return CacheFetches::none();
    }
    let platform = platform.clone();
    CacheFetches(Some(tokio::spawn(async move {
        let mut fetches = JoinSet::new();
        let mut replays: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        // Local apps can also fail boot replay while a registry dependency
        // is still fetching. Seed every persisted install, whatever its source.
        for (tenant, app) in installed {
            replays.entry(tenant).or_default().insert(app);
        }
        let mut replays: BTreeMap<_, Arc<Mutex<_>>> = replays
            .into_iter()
            .map(|(tenant, apps)| (tenant, Arc::new(Mutex::new(apps))))
            .collect();
        for install in pending {
            let retrying = replays.entry(install.tenant.clone()).or_default().clone();
            fetches.spawn(fetch_and_bring_up(
                platform.clone(),
                ps.clone(),
                install,
                retrying,
            ));
        }
        let mut summary = CacheFetchSummary::default();
        while let Some(done) = fetches.join_next().await {
            match done {
                Ok(true) => summary.fetched += 1,
                Ok(false) => summary.failed += 1,
                Err(error) => {
                    summary.failed += 1;
                    tracing::warn!(
                        recovery = "genesis_cache",
                        result = "error",
                        error = %error,
                        "Genesis cache fetch task ended without finishing"
                    );
                }
            }
        }
        if summary.failed > 0 {
            tracing::warn!(
                recovery = "genesis_cache",
                result = "error",
                fetched = summary.fetched,
                failed = summary.failed,
                "Genesis cache recovery could not fetch every installed app; their install records are kept"
            );
        } else {
            tracing::info!(
                recovery = "genesis_cache",
                result = "ok",
                fetched = summary.fetched,
                "Genesis cache recovery fetched every installed app"
            );
        }
        summary
    })))
}

/// Fetch one install, publish it only while current, and retry the tenant's
/// pending runtime recovery. Returns whether the fetch succeeded.
async fn fetch_and_bring_up(
    platform: PlatformState,
    ps: Arc<dyn PlatformStore>,
    install: Pending,
    retrying: Arc<Mutex<BTreeSet<String>>>,
) -> bool {
    let Pending {
        tenant,
        record,
        registry,
        registry_tenant,
        root_ref,
        cache_root,
    } = install;
    let app_name = record.app_name.clone();
    // A fetch owns its files until it can check and publish under the install
    // locks. Even a repeated install of the same ref cannot have its active
    // directory overwritten by a stale recovery in the meantime.
    let fetched = match fetch_directory(&cache_root) {
        Ok(fetched) => fetched,
        Err(error) => {
            tracing::warn!(
                recovery = "genesis_cache", result = "error", tenant = %tenant,
                app = %app_name, error = %error,
                "Could not prepare Genesis cache fetch; its install record is kept"
            );
            return false;
        }
    };
    let fetched_root = fetched.path().join("bundle");
    let materialized = match materialize_registry_app_closure_via_bundle(
        &registry,
        &registry_tenant,
        root_ref.clone(),
        &fetched_root,
    )
    .await
    {
        Ok(_) => Ok(()),
        Err(error) if genesis_git_fallback_enabled() => {
            tracing::warn!(
                recovery = "genesis_cache",
                result = "error",
                tenant = %tenant,
                app = %app_name,
                app_ref = %record.app_ref,
                registry_url = %registry.as_str(),
                error = %error,
                "Genesis bundle cache recovery failed; falling back to git clone because TEMPER_GENESIS_INSTALL_GIT_FALLBACK is enabled"
            );
            materialize_registry_app_closure(&registry, root_ref, &fetched_root)
                .await
                .map(|_| ())
        }
        Err(error) => Err(error),
    };
    if let Err(error) = materialized {
        tracing::warn!(
            recovery = "genesis_cache",
            result = "error",
            tenant = %tenant,
            app = %app_name,
            app_ref = %record.app_ref,
            error = %error,
            "Failed to fetch installed Genesis app files again; its install record is kept"
        );
        return false;
    }
    legacy_specs::convert_legacy_bundle_specs(&fetched_root);
    // Keep failed replays pending before another arrival can retry them. The
    // network work above holds neither this tenant's queue nor an install turn.
    let mut retrying = retrying.lock().await;
    let _turn = match temper_server::host_turn::take().await {
        Ok(turn) => turn,
        Err(error) => {
            tracing::warn!(
                recovery = "genesis_cache", result = "error", tenant = %tenant,
                app = %app_name, error = %error,
                "Could not take the install turn for Genesis cache recovery"
            );
            return false;
        }
    };
    let names =
        match publish_if_current(&platform, ps.as_ref(), &record, &fetched_root, &cache_root).await
        {
            Ok(names) => names,
            Err(error) => {
                tracing::warn!(
                    recovery = "genesis_cache",
                    result = "error",
                    tenant = %tenant,
                    app = %app_name,
                    app_ref = %record.app_ref,
                    error = %error,
                    "Fetched Genesis app files could not be published; its install record is kept"
                );
                return false;
            }
        };
    retrying.extend(names);
    let mut failed = BTreeSet::new();
    for name in retrying.iter() {
        if !bring_up(&platform, ps.as_ref(), &tenant, name).await {
            failed.insert(name.clone());
        }
    }
    let installed: Vec<(String, String)> = retrying
        .iter()
        .map(|name| (tenant.clone(), name.clone()))
        .collect();
    *retrying = failed;
    let _install = crate::os_apps::install_guard::tenant(&platform.server, &tenant).await;
    crate::os_apps::converge_installed_schemas(&platform, &installed).await;
    true
}

fn fetch_directory(cache_root: &Path) -> Result<tempfile::TempDir, String> {
    let parent = cache_root.parent().ok_or("Genesis cache has no parent")?;
    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    tempfile::Builder::new()
        .prefix(".genesis-recovery-")
        .tempdir_in(parent)
        .map_err(|error| error.to_string())
}

/// The caller holds the host install turn through publication and recovery,
/// as registry/pushed installs do through publication and provenance writes.
async fn publish_if_current(
    platform: &PlatformState,
    ps: &dyn PlatformStore,
    captured: &InstalledAppRecord,
    fetched_root: &Path,
    cache_root: &Path,
) -> Result<Vec<String>, String> {
    let _install = crate::os_apps::install_guard::tenant(&platform.server, &captured.tenant).await;
    let current = ps
        .get_installed_app(&captured.tenant, &captured.app_name)
        .await?;
    let current = current.filter(|current| {
        current.source_kind == captured.source_kind
            && current.app_ref == captured.app_ref
            && current.version_hash == captured.version_hash
            && current.bundle_digest == captured.bundle_digest
            && current.registry_url == captured.registry_url
            && current.registry_tenant == captured.registry_tenant
    });
    let Some(current) = current else {
        tracing::info!(
            recovery = "genesis_cache", result = "superseded",
            tenant = %captured.tenant, app = %captured.app_name,
            app_ref = %captured.app_ref,
            "Discarded Genesis cache recovery superseded by a changed install record"
        );
        return Ok(Vec::new());
    };
    let apps =
        cached_apps_matching_records(ps, fetched_root, &current, std::slice::from_ref(&current))
            .await
            .ok_or("fetched files do not hold the recorded version")?;
    let names: BTreeSet<String> = apps.into_iter().map(|(name, _)| name).collect();
    replace_directory(fetched_root.to_path_buf(), cache_root)?;
    // Rebase the exact saved paths onto their published directory, retaining
    // only apps that matched their current records under the same locks.
    let apps = written::read(cache_root)?
        .into_iter()
        .filter(|(name, _)| names.contains(name))
        .collect();
    register_written_apps(&current.tenant, cache_root.to_path_buf(), apps);
    Ok(names.into_iter().collect())
}

/// Bring installed app `app_name` up from the catalog: restore
/// its runtime state when its stored install matches them, and replay its
/// install otherwise. An app in the fetched closure with no install record of
/// its own is left to whoever installs it.
async fn bring_up(
    platform: &PlatformState,
    ps: &dyn PlatformStore,
    tenant: &str,
    app_name: &str,
) -> bool {
    let install = crate::os_apps::install_guard::tenant(&platform.server, tenant).await;
    match ps.get_installed_app(tenant, app_name).await {
        Ok(Some(_)) => {}
        Ok(None) => return true,
        Err(error) => {
            tracing::warn!(
                recovery = "genesis_cache",
                result = "error",
                tenant,
                app = %app_name,
                error = %error,
                "Could not read the install record of a pending app; it is not brought up"
            );
            return false;
        }
    }
    let outcome = recover_installed_app_runtime_state(platform, ps, tenant, app_name).await;
    // Replay takes the tenant lock itself; the host turn still excludes a
    // concurrent catalog/provenance replacement while it runs.
    drop(install);
    match outcome {
        InstalledAppRuntimeRecoveryOutcome::Ready | InstalledAppRuntimeRecoveryOutcome::Healed => {
            true
        }
        InstalledAppRuntimeRecoveryOutcome::NeedsReconcile => {
            if let Err(error) = crate::os_apps::restore_os_app(platform, tenant, app_name).await {
                tracing::warn!(
                    recovery = "genesis_cache",
                    result = "error",
                    tenant,
                    app = %app_name,
                    error = %error,
                    "A pending app's stored install did not replay"
                );
                return false;
            }
            true
        }
        InstalledAppRuntimeRecoveryOutcome::MissingBundle
        | InstalledAppRuntimeRecoveryOutcome::StoreError => {
            tracing::warn!(
                recovery = "genesis_cache",
                result = "error",
                tenant,
                app = %app_name,
                outcome = ?outcome,
                "A pending app could not be brought up"
            );
            false
        }
    }
}
