//! Find the cache directory that still holds an install's recorded versions.

use super::super::cache_paths::legacy_genesis_cache_root;
use super::super::written::{self, Written};
use super::*;
use crate::os_apps::{list_install_dir, written_apps_bundle_digests};

#[path = "legacy_cache.rs"]
mod legacy_cache;

/// A tenant-owned cache, or a shared pre-tenant cache requiring migration.
pub(super) struct InstallCacheCandidate {
    /// Filesystem location of this recovery candidate.
    pub(super) path: PathBuf,
    legacy: bool,
}

/// The cache directories an install of `cache_key` into `tenant` may have, the registry's first.
///
/// A registry install caches under its registry tenant. An `App.Install` from a Genesis service
/// on this server caches under the service's tenant, which its record names as the registry
/// tenant unless the request named another one, so every Genesis source tenant is a candidate too.
/// A pushed install caches under its tenant alone, after every Genesis source.
/// A candidate is used only when it holds the recorded versions. Pre-tenant
/// shared caches require recorded digests and migrate into the first private root.
pub(super) fn install_cache_candidates(
    platform: &PlatformState,
    tenant: &str,
    registry_tenant: &str,
    cache_key: &str,
) -> Vec<InstallCacheCandidate> {
    let mut sources = vec![
        InstallSource::Registry(registry_tenant),
        InstallSource::Local(registry_tenant),
    ];
    let source_tenants = genesis_source_tenants();
    sources.extend(
        source_tenants
            .iter()
            .filter(|source| source.as_str() != registry_tenant)
            .map(|source| InstallSource::Local(source.as_str())),
    );
    sources.push(InstallSource::Pushed);
    let mut candidates: Vec<_> = sources
        .into_iter()
        .map(|source| InstallCacheCandidate {
            path: genesis_cache_root(
                &platform.server,
                CacheOwner::Install { tenant, source },
                cache_key,
            ),
            legacy: false,
        })
        .collect();
    candidates.push(InstallCacheCandidate {
        path: legacy_genesis_cache_root(&platform.server, cache_key),
        legacy: true,
    });
    candidates
}

/// The first of `candidates` that holds `record`'s app at its recorded version,
/// with the apps to take from it.
pub(super) async fn first_matching_install(
    ps: &dyn PlatformStore,
    candidates: &[InstallCacheCandidate],
    record: &InstalledAppRecord,
    recorded: &[InstalledAppRecord],
) -> Option<(PathBuf, Vec<(String, AppFileSource)>)> {
    for candidate in candidates {
        let Some(apps) =
            matching_cached_apps(ps, &candidate.path, record, recorded, candidate.legacy).await
        else {
            continue;
        };
        if !candidate.legacy {
            return Some((candidate.path.clone(), apps));
        }
        // Shared files must never become the tenant's live catalog. Verify a
        // private copy and its saved file list before publishing it instead.
        let destination = &candidates[0].path;
        match legacy_cache::migrate(ps, destination, apps, record, recorded).await {
            Ok(apps) => return Some((destination.clone(), apps)),
            Err(error) => tracing::warn!(
                tenant = %record.tenant,
                app = %record.app_name,
                error = %error,
                "Failed to migrate verified pre-tenant Genesis cache"
            ),
        }
    }
    None
}

/// The apps of an install's cache directory that still match their records,
/// each with the files its install wrote, or `None` when `record`'s own app
/// is not there at its recorded version.
///
/// A directory on disk is not proof on its own: an install could have stopped
/// partway, and a file in it could have changed since. Its apps are the ones
/// its install's list names ([`written`]), read by exact path, never by
/// listing the directory: a directory whose list cannot be read is not used,
/// and the caller fetches the files again. A directory written before installs
/// kept a list is listed once and gets its list from that listing (see the
/// adoption below). Each app in it
/// is decided on its own: it is taken when it reads back at its recorded
/// digest, from `recorded` when that names the app, otherwise from the
/// tenant's stored record. A record without a digest needs the app to read
/// back. An app with no record, or at another version than its record names (a
/// dependency installed again separately), is left out, so the tenant goes on
/// resolving it from wherever it resolves it now. The apps are decided before
/// they are registered, and are registered from the same list, so an app that
/// does not match never becomes what the tenant resolves.
pub(super) async fn cached_apps_matching_records(
    ps: &dyn PlatformStore,
    cache_root: &Path,
    record: &InstalledAppRecord,
    recorded: &[InstalledAppRecord],
) -> Option<Vec<(String, AppFileSource)>> {
    matching_cached_apps(ps, cache_root, record, recorded, false).await
}

async fn matching_cached_apps(
    ps: &dyn PlatformStore,
    cache_root: &Path,
    record: &InstalledAppRecord,
    recorded: &[InstalledAppRecord],
    require_digest: bool,
) -> Option<Vec<(String, AppFileSource)>> {
    if !cache_root.is_dir() {
        return None;
    }
    // `adopted` is the one listing of a cache written before installs kept
    // their file list, when this root has no list yet.
    let (apps, adopted) = match written::read(cache_root) {
        Ok(apps) => (apps, None),
        Err(_) if !written::exists(cache_root) => {
            // Caches written before `.written.json` existed have no list.
            // Fetching each again would hold the first boot after the list
            // shipped for every install in turn, before the listener binds, so
            // such a root is listed once instead. Its apps are taken only
            // where that listing reads back at the recorded digest: a listing
            // that came back short reads back as another digest and the root
            // is fetched again, as one that does not match always is. The list
            // is then written from that listing, and every later boot reads it.
            // Delete this once every install has a list.
            let listed: Written = list_install_dir(cache_root).into_iter().collect();
            match written::app_files(cache_root, listed.clone()) {
                Ok(apps) => (apps, Some(listed)),
                Err(error) => {
                    tracing::warn!(
                        tenant = %record.tenant,
                        app = %record.app_name,
                        error = %error,
                        "An install's cache directory from before file lists holds no usable apps; it is not used"
                    );
                    return None;
                }
            }
        }
        Err(error) => {
            tracing::warn!(
                tenant = %record.tenant,
                app = %record.app_name,
                error = %error,
                "An install's cache directory has no file list it can be read from; it is not used"
            );
            return None;
        }
    };
    let digests = written_apps_bundle_digests(cache_root, apps.clone());
    let mut matching = BTreeSet::new();
    for digest in &digests {
        let expected = match recorded.iter().find(|r| r.app_name == digest.app_name) {
            Some(known) => known.bundle_digest.clone(),
            None => match ps.get_installed_app(&record.tenant, &digest.app_name).await {
                Ok(Some(stored)) => stored.bundle_digest,
                _ => continue,
            },
        };
        if (!require_digest && expected.is_empty()) || expected == digest.bundle_digest {
            matching.insert(digest.app_name.clone());
        }
    }
    if !matching.contains(&record.app_name) {
        tracing::warn!(
            tenant = %record.tenant,
            app = %record.app_name,
            cache = %cache_root.display(),
            "An install's cache directory does not hold its recorded version; it is not used"
        );
        return None;
    }
    // A shared pre-tenant root is read-only; its list is saved in the tenant copy.
    if let Some(mut listed) = adopted.filter(|_| !require_digest) {
        listed.retain(|app, _| matching.contains(app));
        if let Err(error) = written::keep(cache_root, &listed) {
            tracing::warn!(
                tenant = %record.tenant,
                app = %record.app_name,
                error = %error,
                "An install's cache directory matched its records but its file list could not be kept; the next boot lists it again"
            );
        }
    }
    Some(
        apps.into_iter()
            .filter(|(app, _)| matching.contains(app))
            .collect(),
    )
}

/// The files of installed app `app_name` in `tenant` at the version its record
/// holds, from its own cache or a recorded closure's cache. Only Genesis and
/// pushed installs keep their version's files apart from the next version's; a local
/// install replaces its files in place, so its installed version cannot be
/// read once its folder holds another. A pre-tenant shared cache is not read
/// here, since using one means copying it into the tenant first.
pub(crate) async fn installed_version_files(
    platform: &PlatformState,
    tenant: &str,
    app_name: &str,
) -> Result<AppFileSource, String> {
    let ps = platform
        .server
        .storage_stack
        .as_ref()
        .and_then(|stack| stack.platform.clone())
        .ok_or_else(|| "the server has no platform store".to_string())?;
    let record = ps
        .get_installed_app(tenant, app_name)
        .await
        .map_err(|error| format!("its install record does not read: {error}"))?
        .ok_or_else(|| "it has no install record".to_string())?;
    if record.source_kind != "genesis" && record.source_kind != pushed::SOURCE_KIND {
        return Err(format!(
            "it was installed from {} files, which are replaced in place",
            if record.source_kind.is_empty() {
                "local"
            } else {
                record.source_kind.as_str()
            }
        ));
    }
    if let Some(files) = installed_files_in_cache(platform, ps.as_ref(), &record, &record).await {
        return Ok(files);
    }
    // A dependency's own ref names its version, but its files may have been
    // written into a parent's closure. Only this tenant's recorded installs
    // contribute candidates, and each must match the dependency's digest.
    let installed = ps
        .list_all_installed_apps()
        .await
        .map_err(|error| format!("its tenant's install records do not read: {error}"))?;
    for (cache_tenant, name) in installed {
        if cache_tenant != tenant || name == app_name {
            continue;
        }
        let Some(closure) = ps.get_installed_app(tenant, &name).await.map_err(|error| {
            format!("its tenant's install record for '{name}' does not read: {error}")
        })?
        else {
            continue;
        };
        if closure.source_kind != "genesis" && closure.source_kind != pushed::SOURCE_KIND {
            continue;
        }
        if let Some(files) =
            installed_files_in_cache(platform, ps.as_ref(), &record, &closure).await
        {
            return Ok(files);
        }
    }
    Err(format!(
        "no cache directory holds its installed version {}",
        record.app_ref
    ))
}

/// Read `record`'s app from a recorded install's private cache, checking the
/// app's own digest even when a different app's install wrote the cache.
async fn installed_files_in_cache(
    platform: &PlatformState,
    ps: &dyn PlatformStore,
    record: &InstalledAppRecord,
    cache_record: &InstalledAppRecord,
) -> Option<AppFileSource> {
    let registry_tenant = match cache_record.registry_tenant.trim() {
        "" => "default",
        named => named,
    };
    for candidate in install_cache_candidates(
        platform,
        &record.tenant,
        registry_tenant,
        &cache_record.app_ref,
    )
    .into_iter()
    .filter(|candidate| !candidate.legacy)
    {
        let Some(apps) = matching_cached_apps(
            ps,
            &candidate.path,
            record,
            std::slice::from_ref(record),
            true,
        )
        .await
        else {
            continue;
        };
        if let Some((_, files)) = apps.into_iter().find(|(app, _)| app == &record.app_name) {
            return Some(files);
        }
    }
    None
}
