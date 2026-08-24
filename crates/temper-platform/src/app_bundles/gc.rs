use std::collections::BTreeSet;
use std::path::Path;

use crate::state::PlatformState;

use super::cache::cache_root;
use super::types::{BundleCacheGcResult, CanonicalBundleManifestV1};
use super::validation::{digest_hex, validate_manifest};

/// Remove cache objects unreachable from durable local installation records.
pub async fn garbage_collect_local_bundle_cache(
    platform: &PlatformState,
    dry_run: bool,
) -> Result<BundleCacheGcResult, String> {
    let _cache_guard = super::bundle_cache_lock().lock().await;
    let store = platform
        .server
        .storage_stack
        .as_ref()
        .and_then(|stack| stack.platform.clone())
        .ok_or_else(|| "bundle cache GC requires durable platform storage".to_string())?;
    let mut retained_digests = BTreeSet::new();
    for (tenant, app_name) in store.list_all_installed_apps().await? {
        let Some(record) = store.get_installed_app(&tenant, &app_name).await? else {
            continue;
        };
        if record.source_kind == "local_bundle" {
            retained_digests.insert(record.version_hash);
        } else if record.source_kind == "genesis"
            && let Some(digest) = record.closure_id.strip_prefix("bundle:")
        {
            retained_digests.insert(digest.to_string());
        }
    }

    let root = cache_root(&platform.server.data_dir);
    let mut retained_blobs = BTreeSet::new();
    let mut retained_errors = Vec::new();
    for digest in &retained_digests {
        let hex = match digest_hex(digest) {
            Ok(hex) => hex,
            Err(error) => {
                retained_errors.push(error);
                continue;
            }
        };
        let path = root.join("manifests/sha256").join(format!("{hex}.json"));
        let manifest = std::fs::read(&path)
            .map_err(|error| error.to_string())
            .and_then(|bytes| {
                serde_json::from_slice::<CanonicalBundleManifestV1>(&bytes)
                    .map_err(|error| error.to_string())
            })
            .and_then(|manifest| {
                validate_manifest(&manifest)?;
                Ok(manifest)
            });
        match manifest {
            Ok(manifest) => {
                for app in manifest.apps {
                    retained_blobs.extend(app.files.into_iter().map(|file| file.blob_digest));
                }
            }
            Err(error) => retained_errors.push(format!("retain '{}': {error}", path.display())),
        }
    }

    let manifests = collect_files(
        &root.join("manifests/sha256"),
        |name| {
            name.contains(".corrupt-")
                || name
                    .strip_suffix(".json")
                    .is_some_and(|hex| !retained_digests.contains(&format!("sha256:{hex}")))
        },
        dry_run,
    )?;
    // A corrupt referenced manifest makes blob reachability unknowable. Keep
    // every blob until the operator repairs or removes that durable root.
    let blobs = if retained_errors.is_empty() {
        collect_files(
            &root.join("blobs/sha256"),
            |name| !retained_blobs.contains(&format!("sha256:{name}")),
            dry_run,
        )?
    } else {
        0
    };
    let views = collect_directories(
        &root.join("views/sha256"),
        |name| !retained_digests.contains(&format!("sha256:{name}")),
        dry_run,
    )?;
    Ok(BundleCacheGcResult {
        dry_run,
        manifests,
        blobs,
        views,
        retained_errors,
    })
}

fn collect_files(
    directory: &Path,
    collectible: impl Fn(&str) -> bool,
    dry_run: bool,
) -> Result<usize, String> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => {
            return Err(format!(
                "read cache directory '{}': {error}",
                directory.display()
            ));
        }
    };
    let mut count = 0;
    for entry in entries {
        let entry = entry.map_err(|error| format!("read cache entry: {error}"))?;
        let file_type = entry
            .file_type()
            .map_err(|error| format!("inspect cache entry: {error}"))?;
        let name = entry.file_name();
        let name = name
            .to_str()
            .ok_or_else(|| "cache entry name is not UTF-8".to_string())?;
        if file_type.is_file() && collectible(name) {
            count += 1;
            if !dry_run {
                std::fs::remove_file(entry.path()).map_err(|error| {
                    format!("remove cache object '{}': {error}", entry.path().display())
                })?;
            }
        }
    }
    Ok(count)
}

fn collect_directories(
    directory: &Path,
    collectible: impl Fn(&str) -> bool,
    dry_run: bool,
) -> Result<usize, String> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => {
            return Err(format!(
                "read cache directory '{}': {error}",
                directory.display()
            ));
        }
    };
    let mut count = 0;
    for entry in entries {
        let entry = entry.map_err(|error| format!("read cache entry: {error}"))?;
        let file_type = entry
            .file_type()
            .map_err(|error| format!("inspect cache entry: {error}"))?;
        let name = entry.file_name();
        let name = name
            .to_str()
            .ok_or_else(|| "cache entry name is not UTF-8".to_string())?;
        if file_type.is_dir() && collectible(name) {
            count += 1;
            if !dry_run {
                std::fs::remove_dir_all(entry.path()).map_err(|error| {
                    format!("remove cache view '{}': {error}", entry.path().display())
                })?;
            }
        }
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collectors_report_then_remove_only_unreachable_objects() {
        let root = tempfile::tempdir().unwrap();
        let files = root.path().join("files");
        let views = root.path().join("views");
        std::fs::create_dir_all(&files).unwrap();
        std::fs::create_dir_all(views.join("keep")).unwrap();
        std::fs::create_dir_all(views.join("drop")).unwrap();
        std::fs::write(files.join("keep"), b"keep").unwrap();
        std::fs::write(files.join("drop"), b"drop").unwrap();

        assert_eq!(
            collect_files(&files, |name| name == "drop", true).unwrap(),
            1
        );
        assert!(files.join("drop").is_file());
        assert_eq!(
            collect_directories(&views, |name| name == "drop", true).unwrap(),
            1
        );
        assert!(views.join("drop").is_dir());

        assert_eq!(
            collect_files(&files, |name| name == "drop", false).unwrap(),
            1
        );
        assert_eq!(
            collect_directories(&views, |name| name == "drop", false).unwrap(),
            1
        );
        assert!(files.join("keep").is_file());
        assert!(views.join("keep").is_dir());
        assert!(!files.join("drop").exists());
        assert!(!views.join("drop").exists());
    }
}
