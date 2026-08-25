use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use super::MAX_BUNDLE_FILE_BYTES;
use super::types::{
    BundleBlob, CanonicalBundleManifestV1, InstallBundleRequest, InstallBundleResult,
};
use super::validation::{
    digest_hex, validate_blob_file, validate_manifest, validate_materialized_view,
    validate_relative_path,
};
use super::workspace::sha256_prefixed;
use crate::os_apps::{OsAppReconcileResult, reconcile_os_app_from_dir};
use crate::state::PlatformState;
use base64::Engine as _;

/// Validate, cache, materialize, and install one local canonical bundle.
pub async fn install_local_bundle(
    platform: &PlatformState,
    mut request: InstallBundleRequest,
) -> Result<InstallBundleResult, String> {
    let blobs = std::mem::take(&mut request.blobs);
    let outcome =
        install_canonical_bundle(platform, &request.tenant, request.manifest.clone(), blobs)
            .await?;
    super::provenance::record_local_provenance(platform, &request, &outcome.view, &outcome.order)
        .await?;

    let mut added = Vec::new();
    let mut updated = Vec::new();
    let mut skipped = Vec::new();
    if let Some(result) = outcome.root_result {
        match result {
            OsAppReconcileResult::Skipped { app_name, .. } => skipped.push(app_name),
            OsAppReconcileResult::Installed { install, .. } => {
                added = install.added;
                updated = install.updated;
                skipped = install.skipped;
            }
        }
    }
    Ok(InstallBundleResult {
        app_name: request.manifest.root_app,
        bundle_digest: request.manifest.bundle_digest,
        tenant: request.tenant,
        materialized_path: outcome.view.display().to_string(),
        added,
        updated,
        skipped,
    })
}

pub(crate) struct CanonicalInstallOutcome {
    pub(crate) view: PathBuf,
    pub(crate) order: Vec<String>,
    pub(crate) root_result: Option<OsAppReconcileResult>,
    // Keep publication, reconciliation, and provenance promotion in one GC lease.
    pub(crate) _cache_guard: tokio::sync::MutexGuard<'static, ()>,
}

pub(crate) async fn install_canonical_bundle(
    platform: &PlatformState,
    tenant: &str,
    manifest: CanonicalBundleManifestV1,
    blobs: Vec<BundleBlob>,
) -> Result<CanonicalInstallOutcome, String> {
    let cache_guard = super::bundle_cache_lock().lock().await;
    let data_dir = platform.server.data_dir.clone();
    let (manifest, view) = tokio::task::spawn_blocking(move || {
        let view = publish_bundle(&data_dir, &manifest, &blobs)?;
        super::verify::verify_materialized_bundle(&view, &manifest)?;
        Ok::<_, String>((manifest, view))
    })
    .await
    .map_err(|error| format!("bundle verification task failed: {error}"))??;
    let (order, root_result) =
        reconcile_materialized_bundle(platform, tenant, &manifest, &view).await?;
    Ok(CanonicalInstallOutcome {
        view,
        order,
        root_result,
        _cache_guard: cache_guard,
    })
}

/// Restore all local bundle cache roots referenced by durable app records.
pub async fn restore_local_bundle_cache_roots(platform: &PlatformState) -> Result<usize, String> {
    let _cache_guard = super::bundle_cache_lock().lock().await;
    let Some(store) = platform
        .server
        .storage_stack
        .as_ref()
        .and_then(|stack| stack.platform.clone())
    else {
        return Ok(0);
    };
    let installed = store
        .list_all_installed_apps()
        .await
        .map_err(|error| format!("list durable local bundle installations: {error}"))?;
    let mut restored = 0usize;
    let mut digests = BTreeSet::new();
    for (tenant, app_name) in installed {
        let Some(record) = store
            .get_installed_app(&tenant, &app_name)
            .await
            .map_err(|error| format!("read durable app '{tenant}/{app_name}': {error}"))?
        else {
            continue;
        };
        if record.source_kind != "local_bundle"
            || !digests.insert((tenant.clone(), record.version_hash.clone()))
        {
            continue;
        }
        let manifest = read_cached_manifest(&platform.server.data_dir, &record.version_hash)
            .map_err(|error| {
                format!(
                    "restore durable local bundle '{tenant}/{app_name}' at '{}': {error}",
                    record.version_hash
                )
            })?;
        let view = materialize_view(&platform.server.data_dir, &manifest)?;
        super::verify::verify_materialized_bundle(&view, &manifest)?;
        let (order, _) = reconcile_materialized_bundle(platform, &tenant, &manifest, &view).await?;
        let request = InstallBundleRequest {
            tenant: tenant.clone(),
            provenance: super::types::LocalBundleProvenance {
                source_locator: record.app_ref,
                lock_digest: record.dependency_lock_digest,
            },
            manifest,
            blobs: Vec::new(),
        };
        super::provenance::record_local_provenance(platform, &request, &view, &order).await?;
        restored += 1;
    }
    Ok(restored)
}

/// Rebuild and validate a materialized view for one cached digest.
pub fn materialize_cached_bundle(data_dir: &Path, digest: &str) -> Result<PathBuf, String> {
    let manifest = read_cached_manifest(data_dir, digest)?;
    materialize_view(data_dir, &manifest)
}

pub(super) fn read_cached_manifest(
    data_dir: &Path,
    digest: &str,
) -> Result<CanonicalBundleManifestV1, String> {
    let hex = digest_hex(digest)?;
    let manifest_path = cache_root(data_dir)
        .join("manifests/sha256")
        .join(format!("{hex}.json"));
    let bytes = std::fs::read(&manifest_path).map_err(|error| {
        format!(
            "read bundle manifest '{}': {error}",
            manifest_path.display()
        )
    })?;
    let manifest: CanonicalBundleManifestV1 = match serde_json::from_slice(&bytes)
        .map_err(|error| format!("decode bundle manifest: {error}"))
        .and_then(|manifest| {
            validate_manifest(&manifest)?;
            Ok(manifest)
        }) {
        Ok(manifest) => manifest,
        Err(error) => {
            let quarantine = quarantine_file(&manifest_path)?;
            return Err(format!(
                "bundle manifest '{}' was quarantined at '{}': {error}",
                manifest_path.display(),
                quarantine.display()
            ));
        }
    };
    Ok(manifest)
}

pub(super) async fn reconcile_materialized_bundle(
    platform: &PlatformState,
    tenant: &str,
    manifest: &CanonicalBundleManifestV1,
    view: &Path,
) -> Result<(Vec<String>, Option<OsAppReconcileResult>), String> {
    let order = dependency_install_order(manifest)?;
    let mut root_result = None;
    for app_name in &order {
        let result = reconcile_os_app_from_dir(
            platform,
            tenant,
            app_name,
            &view.join(app_name),
            Some((manifest, view)),
        )
        .await?;
        if app_name == &manifest.root_app {
            root_result = Some(result);
        }
    }
    Ok((order, root_result))
}

fn dependency_install_order(manifest: &CanonicalBundleManifestV1) -> Result<Vec<String>, String> {
    let dependencies = manifest
        .apps
        .iter()
        .map(|app| (app.name.as_str(), app.dependencies.as_slice()))
        .collect::<BTreeMap<_, _>>();
    fn visit(
        app: &str,
        dependencies: &BTreeMap<&str, &[String]>,
        visited: &mut BTreeSet<String>,
        order: &mut Vec<String>,
    ) -> Result<(), String> {
        if visited.contains(app) {
            return Ok(());
        }
        let app_dependencies = dependencies
            .get(app)
            .ok_or_else(|| format!("bundle dependency '{app}' is missing"))?;
        for dependency in *app_dependencies {
            visit(dependency, dependencies, visited, order)?;
        }
        visited.insert(app.to_string());
        order.push(app.to_string());
        Ok(())
    }
    let mut visited = BTreeSet::new();
    let mut order = Vec::new();
    visit(&manifest.root_app, &dependencies, &mut visited, &mut order)?;
    Ok(order)
}

fn publish_bundle(
    data_dir: &Path,
    manifest: &CanonicalBundleManifestV1,
    transported_blobs: &[BundleBlob],
) -> Result<PathBuf, String> {
    validate_manifest(manifest)?;
    let root = cache_root(data_dir);
    std::fs::create_dir_all(root.join("manifests/sha256"))
        .map_err(|error| format!("create bundle manifest cache: {error}"))?;
    std::fs::create_dir_all(root.join("blobs/sha256"))
        .map_err(|error| format!("create bundle blob cache: {error}"))?;

    let mut supplied = BTreeMap::new();
    for blob in transported_blobs {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&blob.content_base64)
            .map_err(|error| format!("decode bundle blob '{}': {error}", blob.digest))?;
        if bytes.len() as u64 > MAX_BUNDLE_FILE_BYTES || sha256_prefixed(&bytes) != blob.digest {
            return Err(format!(
                "bundle blob '{}' failed digest or size validation",
                blob.digest
            ));
        }
        supplied.insert(blob.digest.clone(), bytes);
    }

    for app in &manifest.apps {
        for file in &app.files {
            let blob_path = blob_path(&root, &file.blob_digest)?;
            if blob_path.is_file() {
                if let Err(error) = validate_blob_file(&blob_path, &file.blob_digest, file.size) {
                    let quarantine = quarantine_file(&blob_path)?;
                    return Err(format!(
                        "{error}; quarantined at '{}'",
                        quarantine.display()
                    ));
                }
                continue;
            }
            let bytes = supplied.get(&file.blob_digest).ok_or_else(|| {
                format!(
                    "bundle request omitted uncached blob '{}'",
                    file.blob_digest
                )
            })?;
            if bytes.len() as u64 != file.size {
                return Err(format!(
                    "bundle blob '{}' has the wrong length",
                    file.blob_digest
                ));
            }
            publish_file(&blob_path, bytes)?;
        }
    }

    let manifest_hex = digest_hex(&manifest.bundle_digest)?;
    let manifest_path = root
        .join("manifests/sha256")
        .join(format!("{manifest_hex}.json"));
    let manifest_bytes = serde_json::to_vec_pretty(manifest)
        .map_err(|error| format!("encode canonical bundle manifest: {error}"))?;
    publish_file(&manifest_path, &manifest_bytes)?;
    materialize_view(data_dir, manifest)
}

pub(super) fn materialize_view(
    data_dir: &Path,
    manifest: &CanonicalBundleManifestV1,
) -> Result<PathBuf, String> {
    let root = cache_root(data_dir);
    let hex = digest_hex(&manifest.bundle_digest)?;
    let destination = root.join("views/sha256").join(hex);
    let parent = destination
        .parent()
        .ok_or_else(|| "bundle view has no parent".to_string())?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("create bundle view parent '{}': {error}", parent.display()))?;
    if destination.is_dir() && validate_materialized_view(&root, &destination, manifest).is_ok() {
        return Ok(destination);
    }
    let stage = tempfile::Builder::new()
        .prefix(".bundle-view-")
        .tempdir_in(parent)
        .map_err(|error| format!("create staged bundle view: {error}"))?;
    for app in &manifest.apps {
        let app_dir = stage.path().join(&app.name);
        std::fs::create_dir_all(&app_dir)
            .map_err(|error| format!("create bundle app view '{}': {error}", app_dir.display()))?;
        for file in &app.files {
            let relative = validate_relative_path(&file.path)?;
            let target = app_dir.join(relative);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(|error| {
                    format!(
                        "create bundle view directory '{}': {error}",
                        parent.display()
                    )
                })?;
            }
            let source = blob_path(&root, &file.blob_digest)?;
            if let Err(error) = validate_blob_file(&source, &file.blob_digest, file.size) {
                let quarantine = quarantine_file(&source)?;
                return Err(format!(
                    "{error}; quarantined at '{}'",
                    quarantine.display()
                ));
            }
            std::fs::copy(&source, &target).map_err(|error| {
                format!("materialize bundle file '{}': {error}", target.display())
            })?;
            set_read_only(&target)?;
        }
    }
    replace_directory(stage.keep(), &destination)?;
    Ok(destination)
}

pub(super) fn cache_root(data_dir: &Path) -> PathBuf {
    data_dir.join("bundles/v1")
}

fn blob_path(root: &Path, digest: &str) -> Result<PathBuf, String> {
    Ok(root.join("blobs/sha256").join(digest_hex(digest)?))
}

fn publish_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if path.is_file() {
        let existing = std::fs::read(path)
            .map_err(|error| format!("read cached object '{}': {error}", path.display()))?;
        if existing == bytes {
            return Ok(());
        }
        let quarantine = quarantine_file(path)?;
        return Err(format!(
            "cached object '{}' did not match its digest and was quarantined at '{}'",
            path.display(),
            quarantine.display()
        ));
    }
    let parent = path
        .parent()
        .ok_or_else(|| format!("cache object '{}' has no parent", path.display()))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("create cache directory '{}': {error}", parent.display()))?;
    let temporary = tempfile::Builder::new()
        .prefix(".bundle-object-")
        .tempfile_in(parent)
        .map_err(|error| format!("stage cache object: {error}"))?;
    std::fs::write(temporary.path(), bytes)
        .map_err(|error| format!("write staged cache object: {error}"))?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|error| format!("flush staged cache object: {error}"))?;
    temporary
        .persist_noclobber(path)
        .map_err(|error| format!("publish cache object '{}': {}", path.display(), error.error))?;
    set_read_only(path)?;
    sync_directory(parent)
}

fn sync_directory(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        std::fs::File::open(path)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| format!("flush cache directory '{}': {error}", path.display()))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

fn quarantine_file(path: &Path) -> Result<PathBuf, String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("cache object '{}' has no parent", path.display()))?;
    let quarantine = parent.join(format!(
        ".corrupt-{}-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("object"),
        uuid::Uuid::new_v4()
    ));
    std::fs::rename(path, &quarantine).map_err(|error| {
        format!(
            "quarantine corrupt cache object '{}' to '{}': {error}",
            path.display(),
            quarantine.display()
        )
    })?;
    sync_directory(parent)?;
    Ok(quarantine)
}

fn set_read_only(path: &Path) -> Result<(), String> {
    let mut permissions = std::fs::metadata(path)
        .map_err(|error| format!("stat cache file '{}': {error}", path.display()))?
        .permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(path, permissions)
        .map_err(|error| format!("protect cache file '{}': {error}", path.display()))
}

fn replace_directory(staged: PathBuf, destination: &Path) -> Result<(), String> {
    if !destination.exists() {
        std::fs::rename(&staged, destination)
            .map_err(|error| format!("publish bundle view '{}': {error}", destination.display()))?;
        return sync_directory(
            destination
                .parent()
                .ok_or_else(|| "bundle view destination has no parent".to_string())?,
        );
    }
    let parent = destination
        .parent()
        .ok_or_else(|| "bundle view destination has no parent".to_string())?;
    let backup = tempfile::Builder::new()
        .prefix(".bundle-view-backup-")
        .tempdir_in(parent)
        .map_err(|error| format!("stage bundle view backup: {error}"))?
        .keep();
    std::fs::remove_dir(&backup).map_err(|error| format!("prepare bundle view backup: {error}"))?;
    std::fs::rename(destination, &backup)
        .map_err(|error| format!("backup bundle view '{}': {error}", destination.display()))?;
    if let Err(error) = std::fs::rename(&staged, destination) {
        let rollback = std::fs::rename(&backup, destination);
        return match rollback {
            Ok(()) => Err(format!(
                "publish bundle view '{}': {error}",
                destination.display()
            )),
            Err(rollback_error) => Err(format!(
                "publish bundle view '{}': {error}; rollback failed: {rollback_error}",
                destination.display()
            )),
        };
    }
    std::fs::remove_dir_all(&backup)
        .map_err(|error| format!("remove bundle view backup '{}': {error}", backup.display()))?;
    sync_directory(parent)
}

#[cfg(test)]
#[path = "cache_test.rs"]
mod tests;
