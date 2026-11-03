//! Bounded import, export, and publication of Genesis registry bundles.

use std::collections::BTreeSet;
use std::io::Read as _;
use std::path::{Component, Path, PathBuf};

use base64::Engine as _;

use super::bundle_transport::decode_bundle_response;
use super::cache_paths::{
    app_cache_dir, replace_directory, validate_git_object_id, validate_identity_component,
};
use super::closure_install::MaterializedApp;
use super::registry::{RegistryUrl, registry_client};
use super::written::{self, Written};
use super::{
    GenesisRegistryBundleApp, GenesisRegistryBundleFile, GenesisRegistryBundleResponse,
    RegistryAppRef, parse_registry_app_ref,
};
use crate::os_apps::AppFileSource;

pub(super) const MAX_GENESIS_BUNDLE_FILES: usize = 4096;
pub(super) const MAX_GENESIS_BUNDLE_APPS: usize = 256;
pub(super) const MAX_GENESIS_BUNDLE_FILE_BYTES: u64 = 16 * 1024 * 1024;
// Raised from 64 MiB. A real app closure did not fit: dsf-factory alone is
// ~52 MB of legitimate compiled WASM (59 modules, one per resource operation),
// leaving ~12 MB for every app it depends on combined. The budget exists to
// bound how much a single install may materialize, not to cap an app at a size
// the platform's own apps already exceed, so give it room while keeping it
// finite. Per-file (16 MiB) and file-count (4096) limits are unchanged and are
// what actually catch a runaway publish.
pub(super) const MAX_GENESIS_BUNDLE_TOTAL_BYTES: u64 = 256 * 1024 * 1024;
const MAX_GENESIS_TREE_OBJECTS: usize = 8192;
const MAX_GENESIS_TREE_ENTRIES: usize = 16_384;
const MAX_GENESIS_TREE_DEPTH: usize = 128;
const MAX_GENESIS_TREE_TOTAL_BYTES: u64 = 64 * 1024 * 1024;

/// Fetch the pinned closure's bundle and write it whole to `cache_root`:
/// each app at `<cache_root>/<name>`, with the files it was written with.
pub(super) async fn materialize_registry_app_closure_via_bundle(
    registry: &RegistryUrl,
    registry_tenant: &str,
    root_ref: RegistryAppRef,
    cache_root: &Path,
) -> Result<Vec<MaterializedApp>, String> {
    let Some(version_hash) = root_ref.version_hash.as_deref() else {
        return Err("bundle fetch requires a pinned root app ref".to_string());
    };
    let version_hash = validate_git_object_id(version_hash)?;
    let bundle_url = format!(
        "{}/api/genesis/apps/{}/{}/versions/{}/bundle",
        registry.as_str(),
        root_ref.owner,
        root_ref.name,
        version_hash
    );
    let response = registry_client()?
        .get(&bundle_url)
        .header("X-Tenant-Id", registry_tenant)
        .send()
        .await
        .map_err(|error| format!("request Genesis bundle {bundle_url}: {error}"))?;
    let bundle = decode_bundle_response(response, &bundle_url).await?;
    validate_registry_bundle(&bundle, &root_ref, registry_tenant)?;

    let cache_parent = cache_root.parent().ok_or_else(|| {
        format!(
            "Genesis registry bundle cache '{}' has no parent",
            cache_root.display()
        )
    })?;
    std::fs::create_dir_all(cache_parent).map_err(|error| {
        format!(
            "create Genesis registry bundle cache parent '{}': {error}",
            cache_parent.display()
        )
    })?;
    let staged_cache = tempfile::Builder::new()
        .prefix(".genesis-bundle-")
        .tempdir_in(cache_parent)
        .map_err(|error| format!("create staged Genesis bundle cache: {error}"))?;

    let mut apps = Vec::new();
    let mut written = Written::new();
    for app in bundle.apps {
        validate_identity_component("owner", &app.owner)?;
        let paths = write_bundle_app(&app_cache_dir(staged_cache.path(), &app.name)?, &app)?;
        written.insert(app.name.clone(), paths.iter().cloned().collect());
        // Where the app is once the staged closure is renamed into place.
        let files = AppFileSource::written(app_cache_dir(cache_root, &app.name)?, paths);
        let version = RegistryAppRef {
            owner: app.owner,
            name: app.name,
            version_hash: Some(app.version_hash),
        };
        apps.push(MaterializedApp { version, files });
    }
    written::keep(staged_cache.path(), &written)?;
    replace_directory(staged_cache.keep(), cache_root)?;
    Ok(apps)
}

fn validate_registry_bundle(
    bundle: &GenesisRegistryBundleResponse,
    root_ref: &RegistryAppRef,
    registry_tenant: &str,
) -> Result<(), String> {
    if bundle.registry_tenant != registry_tenant {
        return Err(format!(
            "Genesis bundle tenant '{}' does not match requested tenant '{registry_tenant}'",
            bundle.registry_tenant
        ));
    }
    let bundle_ref = parse_registry_app_ref(&bundle.app_ref)?;
    let expected_hash = validate_git_object_id(
        root_ref
            .version_hash
            .as_deref()
            .ok_or_else(|| "Genesis bundle root is not pinned".to_string())?,
    )?;
    let bundle_hash = validate_git_object_id(
        bundle_ref
            .version_hash
            .as_deref()
            .ok_or_else(|| "Genesis bundle response app_ref is not pinned".to_string())?,
    )?;
    if bundle_ref.owner != root_ref.owner
        || bundle_ref.name != root_ref.name
        || bundle_hash != expected_hash
    {
        return Err(
            "Genesis bundle response does not match the requested pinned app ref".to_string(),
        );
    }
    if bundle.apps.len() > MAX_GENESIS_BUNDLE_APPS {
        return Err(format!(
            "Genesis bundle contains {} apps; budget is {MAX_GENESIS_BUNDLE_APPS}",
            bundle.apps.len()
        ));
    }

    let mut app_names = BTreeSet::new();
    let mut file_count = 0usize;
    let mut decoded_budget = MAX_GENESIS_BUNDLE_TOTAL_BYTES;
    let max_encoded_file_bytes = MAX_GENESIS_BUNDLE_FILE_BYTES
        .checked_add(2)
        .map(|bytes| bytes / 3)
        .and_then(|groups| groups.checked_mul(4))
        .expect("static bundle file budget must encode without overflow");
    let mut root_matches = 0usize;
    for app in &bundle.apps {
        validate_identity_component("owner", &app.owner)?;
        validate_identity_component("app name", &app.name)?;
        let app_hash = validate_git_object_id(&app.version_hash)?;
        if !app_names.insert(app.name.as_str()) {
            return Err(format!(
                "Genesis bundle contains duplicate app directory name '{}'",
                app.name
            ));
        }
        if app.owner == root_ref.owner && app.name == root_ref.name {
            root_matches += 1;
            if app_hash != expected_hash {
                return Err(
                    "Genesis bundle root app version does not match requested hash".to_string(),
                );
            }
        }
        for file in &app.files {
            file_count = file_count
                .checked_add(1)
                .ok_or_else(|| "Genesis bundle file count overflowed usize".to_string())?;
            if file_count > MAX_GENESIS_BUNDLE_FILES {
                return Err(format!(
                    "Genesis bundle file count exceeds budget {MAX_GENESIS_BUNDLE_FILES}"
                ));
            }
            safe_bundle_relative_path(&file.path)?;
            if file.content_base64.len() as u64 > max_encoded_file_bytes {
                return Err(format!(
                    "Genesis bundle file '{}' exceeds the encoded per-file budget",
                    file.path
                ));
            }
            let decoded_upper_bound = (file.content_base64.len() as u64)
                .div_ceil(4)
                .saturating_mul(3);
            if decoded_upper_bound > decoded_budget {
                return Err(format!(
                    "Genesis bundle file '{}' exceeds the remaining aggregate byte budget",
                    file.path
                ));
            }
            decoded_budget -= decoded_upper_bound;
        }
    }
    if root_matches != 1 {
        return Err("Genesis bundle must contain exactly one requested root app".to_string());
    }
    Ok(())
}

/// Write `app`'s files into `app_dir`, and return the path of each inside
/// it, `/`-separated.
pub(super) fn write_bundle_app(
    app_dir: &Path,
    app: &GenesisRegistryBundleApp,
) -> Result<Vec<String>, String> {
    if app_dir.exists() {
        std::fs::remove_dir_all(app_dir).map_err(|error| {
            format!("clear Genesis bundle app '{}': {error}", app_dir.display())
        })?;
    }
    std::fs::create_dir_all(app_dir)
        .map_err(|error| format!("create Genesis bundle app '{}': {error}", app_dir.display()))?;

    let mut written = Vec::new();
    for file in &app.files {
        let rel = safe_bundle_relative_path(&file.path)?;
        let path = app_dir.join(&rel);
        written.push(rel.to_string_lossy().replace('\\', "/"));
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                format!("create bundle file parent '{}': {error}", parent.display())
            })?;
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&file.content_base64)
            .map_err(|error| format!("decode bundle file '{}': {error}", file.path))?;
        if bytes.len() as u64 > MAX_GENESIS_BUNDLE_FILE_BYTES {
            return Err(format!(
                "decoded Genesis bundle file '{}' exceeds {MAX_GENESIS_BUNDLE_FILE_BYTES} bytes",
                file.path
            ));
        }
        std::fs::write(&path, bytes)
            .map_err(|error| format!("write bundle file '{}': {error}", path.display()))?;
    }
    Ok(written)
}

pub(super) fn safe_bundle_relative_path(path: &str) -> Result<PathBuf, String> {
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

pub(super) struct GenesisBundleBudget {
    files_remaining: usize,
    bytes_remaining: u64,
    tree_objects_remaining: usize,
    tree_entries_remaining: usize,
    tree_bytes_remaining: u64,
}

impl GenesisBundleBudget {
    pub(super) fn new() -> Self {
        Self {
            files_remaining: MAX_GENESIS_BUNDLE_FILES,
            bytes_remaining: MAX_GENESIS_BUNDLE_TOTAL_BYTES,
            tree_objects_remaining: MAX_GENESIS_TREE_OBJECTS,
            tree_entries_remaining: MAX_GENESIS_TREE_ENTRIES,
            tree_bytes_remaining: MAX_GENESIS_TREE_TOTAL_BYTES,
        }
    }

    pub(super) fn consume_file(&mut self, path: &Path, bytes: u64) -> Result<(), String> {
        if self.files_remaining == 0 {
            return Err(format!(
                "Genesis bundle file count exceeded budget {MAX_GENESIS_BUNDLE_FILES}"
            ));
        }
        if bytes > MAX_GENESIS_BUNDLE_FILE_BYTES {
            return Err(format!(
                "Genesis bundle file '{}' is {bytes} bytes; per-file budget is {MAX_GENESIS_BUNDLE_FILE_BYTES}",
                path.display()
            ));
        }
        if bytes > self.bytes_remaining {
            // Name the budget and what is left of it. The usual cause is an app
            // published with build output committed (a Rust `target/`, a
            // `node_modules/`), where one intermediate is larger than the whole
            // remaining allowance — and the app that fails is the one being
            // installed, not the one at fault, so say which file it was.
            let consumed = MAX_GENESIS_BUNDLE_TOTAL_BYTES.saturating_sub(self.bytes_remaining);
            return Err(format!(
                "Genesis bundle file '{}' is {bytes} bytes but only {} of the \
                 {MAX_GENESIS_BUNDLE_TOTAL_BYTES}-byte bundle budget remains ({consumed} already \
                 consumed). An app published with build output committed is the usual cause; \
                 republish it without those files.",
                path.display(),
                self.bytes_remaining
            ));
        }
        self.files_remaining -= 1;
        self.bytes_remaining -= bytes;
        Ok(())
    }

    pub(super) fn consume_tree(&mut self, path: &Path, canonical_bytes: u64) -> Result<(), String> {
        if self.tree_objects_remaining == 0 {
            return Err(format!(
                "Genesis tree count exceeded budget {MAX_GENESIS_TREE_OBJECTS} at '{}'",
                path.display()
            ));
        }
        if canonical_bytes > self.tree_bytes_remaining {
            return Err(format!(
                "Genesis tree '{}' exceeds the remaining aggregate tree byte budget {}",
                path.display(),
                self.tree_bytes_remaining
            ));
        }
        self.tree_objects_remaining -= 1;
        self.tree_bytes_remaining -= canonical_bytes;
        Ok(())
    }

    pub(super) fn consume_tree_entry(&mut self, path: &Path, depth: usize) -> Result<(), String> {
        if depth > MAX_GENESIS_TREE_DEPTH {
            return Err(format!(
                "Genesis tree path '{}' exceeds depth budget {MAX_GENESIS_TREE_DEPTH}",
                path.display()
            ));
        }
        if self.tree_entries_remaining == 0 {
            return Err(format!(
                "Genesis tree entries exceeded budget {MAX_GENESIS_TREE_ENTRIES} at '{}'",
                path.display()
            ));
        }
        self.tree_entries_remaining -= 1;
        Ok(())
    }
}

/// Read `paths` of `app_dir`, the files its Genesis tree named, as bundle
/// files. Files under a generated folder (`target`, `.git`) are left out.
pub(super) fn collect_bundle_files(
    app_dir: &Path,
    paths: &BTreeSet<String>,
    budget: &mut GenesisBundleBudget,
) -> Result<Vec<GenesisRegistryBundleFile>, String> {
    let file_budget = budget.files_remaining;
    let mut files = Vec::new();
    let generated = |rel: &&String| {
        rel.split('/')
            .any(|part| part == "target" || part == ".git")
    };
    for rel in paths.iter().filter(|rel| !generated(rel)) {
        if files.len() >= file_budget {
            return Err(format!(
                "Genesis bundle file count exceeded budget {MAX_GENESIS_BUNDLE_FILES}"
            ));
        }
        let path = app_dir.join(rel);
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|error| format!("stat bundle file '{}': {error}", path.display()))?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "Genesis bundle entry '{}' must not be a symbolic link",
                path.display()
            ));
        }
        if !metadata.is_file() {
            return Err(format!(
                "Genesis bundle path '{}' must be a regular file",
                path.display()
            ));
        }
        budget.consume_file(&path, metadata.len())?;
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        std::fs::File::open(&path)
            .map_err(|error| format!("open bundle file '{}': {error}", path.display()))?
            .take(metadata.len().saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|error| format!("read bundle file '{}': {error}", path.display()))?;
        if bytes.len() as u64 != metadata.len() {
            return Err(format!(
                "Genesis bundle file '{}' changed size while reading",
                path.display()
            ));
        }
        files.push(GenesisRegistryBundleFile {
            path: rel.clone(),
            content_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
        });
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn materialization_budget_rejects_files_trees_entries_and_depth_before_io() {
        let path = Path::new("app/file");

        let mut file_budget = GenesisBundleBudget::new();
        assert!(
            file_budget
                .consume_file(path, MAX_GENESIS_BUNDLE_FILE_BYTES + 1)
                .expect_err("oversized file")
                .contains("per-file budget")
        );

        let mut tree_budget = GenesisBundleBudget::new();
        assert!(
            tree_budget
                .consume_tree(path, MAX_GENESIS_TREE_TOTAL_BYTES + 1)
                .expect_err("oversized aggregate tree input")
                .contains("tree byte budget")
        );
        assert!(
            tree_budget
                .consume_tree_entry(path, MAX_GENESIS_TREE_DEPTH + 1)
                .expect_err("excessive depth")
                .contains("depth budget")
        );

        let mut entry_budget = GenesisBundleBudget::new();
        for _ in 0..MAX_GENESIS_TREE_ENTRIES {
            entry_budget
                .consume_tree_entry(path, 1)
                .expect("entry within budget");
        }
        assert!(
            entry_budget
                .consume_tree_entry(path, 1)
                .expect_err("entry count exhausted")
                .contains("tree entries")
        );
    }
}
