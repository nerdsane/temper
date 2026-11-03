//! Bringing installs this server already made back: at boot, and for rollback.
//!
//! Restoring reproduces a stored install. It reuses the install's own cache
//! directory when that still holds the recorded version, and otherwise fetches
//! the same version again from where the install came from. Which registry
//! new installs come from is decided at install time, not here.

use std::collections::BTreeSet;

use temper_runtime::tenant::TenantId;
use temper_server::platform_store::{InstalledAppRecord, PlatformStore};

mod cache_recovery;
mod fetch;
mod rollback;

pub(crate) use cache_recovery::installed_version_files;
use cache_recovery::{
    cached_apps_matching_records, first_matching_install, install_cache_candidates,
};

pub use fetch::{CacheFetchSummary, CacheFetches};
pub(super) use rollback::rollback_to_prior;

use super::registry::recorded_registry;
use super::*;

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
            let target_tenant = string_field(&installation.state.fields, "TargetTenant")
                .filter(|target| !target.trim().is_empty())
                .unwrap_or_else(|| source_tenant.clone());
            let cache_root = genesis_cache_root(
                &platform.server,
                CacheOwner::Install {
                    tenant: &target_tenant,
                    source: InstallSource::Local(&source_tenant),
                },
                &app_ref,
            );
            let root = GenesisAppBundle {
                owner,
                name,
                repository_id,
                version_hash,
            };
            match materialize_app_closure(&platform.server, &tenant, &cache_root, root).await {
                Ok(written) => {
                    legacy_specs::convert_legacy_bundle_specs(&cache_root);
                    register_written_apps(&target_tenant, cache_root, written);
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
/// registry into this Temper instance, and return how many came from disk.
///
/// The installs whose files must be fetched again go on fetching after this
/// returns; see [`start_genesis_registry_cache_recovery`].
pub async fn restore_genesis_registry_cache_roots(platform: &PlatformState) -> usize {
    start_genesis_registry_cache_recovery(platform)
        .await
        .restored
}

/// What a boot's Genesis cache recovery restored from disk, and the fetches it
/// started for the rest.
pub struct GenesisCacheRecovery {
    /// Installs restored from their cache directories.
    pub restored: usize,
    /// One fetch for each install whose files were not on disk.
    pub fetching: CacheFetches,
}

/// Rebuild local cache roots for apps that were installed from a remote Genesis
/// registry into this Temper instance.
///
/// These rows live in the target instance's durable installed-app table. They
/// are distinct from Genesis service-side `AppInstallation` rows above. On
/// restart, recovering the pinned cache roots first lets the normal runtime
/// recovery/reconcile path validate digests without re-dispatching
/// spec-owned `App.Install` or rerunning seed data for unchanged refs.
///
/// Every stored install comes back, whatever the registry setting is now: from
/// its cache directory when that holds the recorded version, otherwise fetched
/// again from the registry it was installed from. The cache directories are
/// read before this returns. The fetches each run on their own and are not
/// waited for, so a slow or missing bundle holds neither the boot nor any other
/// install, and a fetched install brings its apps up itself. An install whose
/// files can be neither found nor fetched keeps its record, and the reason is
/// logged at WARN. Each app in a directory is taken only while it matches its
/// own record, so a dependency installed again separately keeps the version it
/// has now.
pub async fn start_genesis_registry_cache_recovery(
    platform: &PlatformState,
) -> GenesisCacheRecovery {
    let nothing = || GenesisCacheRecovery {
        restored: 0,
        fetching: CacheFetches::none(),
    };
    let Some(ps) = platform
        .server
        .storage_stack
        .as_ref()
        .and_then(|stack| stack.platform.clone())
    else {
        return nothing();
    };

    let installed = match ps.list_all_installed_apps().await {
        Ok(installed) => installed,
        Err(error) => {
            tracing::warn!(
                recovery = "genesis_cache",
                result = "error",
                error = %error,
                "Failed to list installed apps for Genesis cache recovery"
            );
            return nothing();
        }
    };

    let mut restored = 0usize;
    let mut seen = BTreeSet::new();
    let mut pending = Vec::new();
    for (tenant, app_name) in installed.iter().cloned() {
        let record = match ps.get_installed_app(&tenant, &app_name).await {
            Ok(Some(record)) => record,
            Ok(None) => continue,
            Err(error) => {
                tracing::warn!(
                    recovery = "genesis_cache",
                    result = "error",
                    tenant = %tenant,
                    app = %app_name,
                    error = %error,
                    "Failed to read installed app metadata for Genesis cache recovery"
                );
                continue;
            }
        };
        // A pushed install's files exist only in its own cache directory; a
        // Genesis install's can be fetched again from its registry.
        let pushed = record.source_kind == pushed::SOURCE_KIND;
        if !pushed && (record.source_kind != "genesis" || record.registry_url.trim().is_empty()) {
            continue;
        }
        let root_ref = match parse_registry_app_ref(&record.app_ref) {
            Ok(root_ref) => root_ref,
            Err(error) => {
                tracing::warn!(
                    recovery = "genesis_cache",
                    result = "error",
                    tenant = %tenant,
                    app = %app_name,
                    app_ref = %record.app_ref,
                    error = %error,
                    "Installed app has invalid Genesis app_ref"
                );
                continue;
            }
        };
        let registry_tenant = if record.registry_tenant.trim().is_empty() {
            "default"
        } else {
            record.registry_tenant.trim()
        };
        let candidates =
            install_cache_candidates(platform, &tenant, registry_tenant, &record.app_ref);
        let cache_root = candidates[0].path.clone();
        // Each tenant's install has its own directory, so records restore it
        // once per directory, from that install's own registry tenant.
        if !seen.insert(cache_root.clone()) {
            continue;
        }
        let recorded = std::slice::from_ref(&record);
        if let Some((stored, apps)) =
            first_matching_install(ps.as_ref(), &candidates, &record, recorded).await
        {
            register_written_apps(&tenant, stored, apps);
            restored += 1;
            continue;
        }
        let registry = match recorded_registry(&record.registry_url) {
            Ok(registry) => registry,
            Err(error) => {
                tracing::warn!(
                    recovery = "genesis_cache",
                    result = "error",
                    tenant = %tenant,
                    app = %app_name,
                    app_ref = %record.app_ref,
                    error = %error,
                    "Installed Genesis app's files are not on disk and its recorded registry cannot be fetched from; its install record is kept"
                );
                continue;
            }
        };
        pending.push(fetch::Pending {
            registry_tenant: registry_tenant.to_string(),
            tenant,
            record,
            registry,
            root_ref,
            cache_root,
        });
    }

    GenesisCacheRecovery {
        restored,
        fetching: fetch::spawn(platform, ps, pending, installed),
    }
}

/// Recover the root and each dependency independently, even when another bundle is missing.
pub(super) async fn ensure_prior_bundle_available(
    platform: &PlatformState,
    ps: &dyn PlatformStore,
    prior: &InstalledAppRecord,
    prior_records: &[InstalledAppRecord],
) -> Result<(), String> {
    let root = ensure_prior_root_available(platform, ps, prior, prior_records).await;
    let served = root.as_ref().cloned().unwrap_or_default();
    let dependencies =
        rollback::restore_prior_dependencies(platform, ps, prior, prior_records, &served).await;
    let errors: Vec<_> = [root.map(|_| ()), dependencies]
        .into_iter()
        .filter_map(Result::err)
        .collect();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

/// Make sure the prior version's bundle is resolvable from the app catalog before rollback.
///
/// Prefers the prior install's on-disk cache, whether a registry install or an `App.Install`
/// made it; if it was evicted, re-materializes the closure from the prior record's registry
/// coordinates. `prior_records` are the closure's records from before the failed install, which
/// has since overwritten the stored ones. Dependencies must be recovered at their recorded digests,
/// including from other install closures or their recorded registries. Best-effort for the root
/// when the prior ref is unpinned or carries no registry URL — rollback then relies on whatever
/// bundle the catalog already resolves for the app.
async fn ensure_prior_root_available(
    platform: &PlatformState,
    ps: &dyn PlatformStore,
    prior: &InstalledAppRecord,
    prior_records: &[InstalledAppRecord],
) -> Result<BTreeSet<String>, String> {
    let prior_ref = parse_registry_app_ref(&prior.app_ref)?;
    let Some(prior_hash) = prior_ref.version_hash.clone() else {
        return Ok(BTreeSet::new());
    };
    let cache_key = format!(
        "{}/{}@{}",
        prior_ref.owner,
        prior_ref.name,
        prior_hash.trim_start_matches('@')
    );
    let registry_tenant = if prior.registry_tenant.trim().is_empty() {
        "default".to_string()
    } else {
        prior.registry_tenant.trim().to_string()
    };
    let candidates =
        install_cache_candidates(platform, &prior.tenant, &registry_tenant, &cache_key);
    if let Some((stored, apps)) =
        first_matching_install(ps, &candidates, prior, prior_records).await
    {
        let names = apps.iter().map(|(app, _)| app.clone()).collect();
        register_written_apps(&prior.tenant, stored, apps);
        return Ok(names);
    }
    let cache_root = candidates[0].path.clone();
    if cache_root.is_dir() {
        tracing::warn!(
            app = %prior.app_name,
            app_ref = %prior.app_ref,
            "Prior Genesis cache did not yield the expected bundle; re-materializing for rollback"
        );
    }
    if prior.registry_url.trim().is_empty() {
        return Ok(BTreeSet::new());
    }
    let registry = recorded_registry(&prior.registry_url)?;
    std::fs::create_dir_all(&cache_root).map_err(|error| {
        format!(
            "create Genesis rollback cache '{}': {error}",
            cache_root.display()
        )
    })?;
    materialize_registry_app_closure_via_bundle(
        &registry,
        &registry_tenant,
        prior_ref,
        &cache_root,
    )
    .await
    .map_err(|error| {
        format!(
            "failed to re-materialize previous version {} for rollback: {error}",
            prior.app_ref
        )
    })?;
    let apps = cached_apps_matching_records(ps, &cache_root, prior, prior_records)
        .await
        .ok_or_else(|| {
            format!(
                "re-materialized previous version {} does not hold its recorded bundle",
                prior.app_ref
            )
        })?;
    let names = apps.iter().map(|(app, _)| app.clone()).collect();
    register_written_apps(&prior.tenant, cache_root, apps);
    Ok(names)
}

#[cfg(test)]
#[path = "restore/tests.rs"]
mod tests;
