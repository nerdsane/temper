use std::collections::{BTreeMap, BTreeSet};
use std::io::Read as _;
use std::path::{Component, Path, PathBuf};

use base64::Engine as _;
use sha2::{Digest as _, Sha256};

use super::types::{
    BundleBlob, CanonicalAppManifest, CanonicalBundleManifestV1, CanonicalFileManifest,
    InstallBundleRequest, LocalBundleProvenance, LocalDependencyLock, WorkspaceBundle,
};
use super::{
    MAX_BUNDLE_APPS, MAX_BUNDLE_DEPTH, MAX_BUNDLE_FILE_BYTES, MAX_BUNDLE_FILES,
    MAX_BUNDLE_TOTAL_BYTES, MAX_BUNDLE_TREE_ENTRIES,
};
use crate::os_apps::AppManifest;

const LOCK_FILE: &str = "temper.lock.toml";

/// Build a canonical immutable bundle from an explicit local app workspace.
pub fn build_workspace_bundle(
    workspace: &Path,
    tenant: &str,
    locked: bool,
) -> Result<WorkspaceBundle, String> {
    let workspace_root = workspace
        .canonicalize()
        .map_err(|error| format!("canonicalize workspace '{}': {error}", workspace.display()))?;
    if !workspace_root.is_dir() {
        return Err(format!(
            "local app workspace '{}' is not a directory",
            workspace_root.display()
        ));
    }

    let lock_path = workspace_root.join(LOCK_FILE);
    let lock_bytes = match std::fs::read(&lock_path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(format!("read '{}': {error}", lock_path.display())),
    };
    if locked && lock_bytes.is_empty() {
        return Err(format!(
            "--locked requires an existing non-empty {}",
            lock_path.display()
        ));
    }
    let mut lock = if lock_bytes.is_empty() {
        LocalDependencyLock::default()
    } else {
        toml::from_str::<LocalDependencyLock>(
            std::str::from_utf8(&lock_bytes)
                .map_err(|error| format!("decode '{}': {error}", lock_path.display()))?,
        )
        .map_err(|error| format!("parse '{}': {error}", lock_path.display()))?
    };
    if lock.version != 1 {
        return Err(format!(
            "unsupported {} version {}; expected 1",
            lock_path.display(),
            lock.version
        ));
    }

    lock.entries
        .sort_by(|left, right| left.name.cmp(&right.name));
    let mut locked_paths = BTreeMap::new();
    for entry in &lock.entries {
        validate_app_name(&entry.name)?;
        if locked_paths
            .insert(entry.name.clone(), entry.path.clone())
            .is_some()
        {
            return Err(format!(
                "duplicate local dependency '{}' in lock",
                entry.name
            ));
        }
    }

    let mut state = BuildState::default();
    collect_app(&workspace_root, &workspace_root, &locked_paths, &mut state)?;
    if state.apps.len() > MAX_BUNDLE_APPS {
        return Err(format!(
            "local bundle contains {} apps; budget is {MAX_BUNDLE_APPS}",
            state.apps.len()
        ));
    }
    let root_manifest = read_manifest(&workspace_root)?;
    let root_app = root_manifest.name;

    let mut apps = state.apps.into_values().collect::<Vec<_>>();
    apps.sort_by(|left, right| left.name.cmp(&right.name));
    let bundle_digest = digest_manifest_records(&root_app, &apps);
    let manifest = CanonicalBundleManifestV1 {
        schema_version: 1,
        root_app,
        apps,
        bundle_digest: bundle_digest.clone(),
    };

    for entry in &mut lock.entries {
        if let Some(digest) = state.app_digests.get(&entry.name) {
            if locked {
                if entry.digest.is_empty() {
                    return Err(format!(
                        "locked dependency '{}' has no resolved digest",
                        entry.name
                    ));
                }
                if entry.digest != *digest {
                    return Err(format!(
                        "locked dependency '{}' changed: expected {}, found {}",
                        entry.name, entry.digest, digest
                    ));
                }
            }
            entry.digest.clone_from(digest);
        }
    }
    let resolved_lock = lock;
    let resolved_lock_text = toml::to_string_pretty(&resolved_lock)
        .map_err(|error| format!("serialize local dependency lock: {error}"))?;
    let lock_digest = if resolved_lock.entries.is_empty() {
        String::new()
    } else {
        sha256_prefixed(resolved_lock_text.as_bytes())
    };

    let blobs = state
        .blobs
        .into_iter()
        .map(|(digest, bytes)| BundleBlob {
            digest,
            content_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
        })
        .collect();
    let request = InstallBundleRequest {
        tenant: tenant.to_string(),
        provenance: LocalBundleProvenance {
            source_locator: workspace_root.display().to_string(),
            lock_digest,
        },
        manifest,
        blobs,
    };
    Ok(WorkspaceBundle {
        request,
        resolved_lock,
        workspace_root,
    })
}

/// Atomically persist the resolved local dependency lock.
pub fn write_workspace_lock(bundle: &WorkspaceBundle) -> Result<(), String> {
    let path = bundle.workspace_root.join(LOCK_FILE);
    let text = toml::to_string_pretty(&bundle.resolved_lock)
        .map_err(|error| format!("serialize '{}': {error}", path.display()))?;
    let temporary = path.with_extension("toml.tmp");
    std::fs::write(&temporary, text)
        .map_err(|error| format!("write '{}': {error}", temporary.display()))?;
    std::fs::rename(&temporary, &path)
        .map_err(|error| format!("publish '{}': {error}", path.display()))
}

#[derive(Default)]
pub(super) struct BuildState {
    pub(super) apps: BTreeMap<String, CanonicalAppManifest>,
    pub(super) app_paths: BTreeMap<String, PathBuf>,
    pub(super) app_digests: BTreeMap<String, String>,
    pub(super) blobs: BTreeMap<String, Vec<u8>>,
    pub(super) visiting: BTreeSet<String>,
    pub(super) files: usize,
    pub(super) tree_entries: usize,
    pub(super) bytes: u64,
}

fn collect_app(
    app_dir: &Path,
    lock_root: &Path,
    locked_paths: &BTreeMap<String, String>,
    state: &mut BuildState,
) -> Result<(), String> {
    let manifest = read_manifest(app_dir)?;
    validate_app_name(&manifest.name)?;
    if state.visiting.contains(&manifest.name) {
        return Err(format!(
            "cyclic local dependency detected at '{}'",
            manifest.name
        ));
    }
    if let Some(existing_path) = state.app_paths.get(&manifest.name) {
        if existing_path == app_dir {
            return Ok(());
        }
        return Err(format!(
            "application name '{}' resolves to conflicting paths '{}' and '{}'",
            manifest.name,
            existing_path.display(),
            app_dir.display()
        ));
    }
    state
        .app_paths
        .insert(manifest.name.clone(), app_dir.to_path_buf());
    state.visiting.insert(manifest.name.clone());

    let mut dependencies = Vec::new();
    for raw_dependency in &manifest.dependencies {
        if raw_dependency.contains('@') || raw_dependency.contains('/') {
            return Err(format!(
                "local bundle dependency '{raw_dependency}' is a Genesis ref; install pinned Genesis closures through App.Install"
            ));
        }
        let dependency_name = dependency_name(raw_dependency)?;
        let Some(relative_path) = locked_paths.get(&dependency_name) else {
            return Err(format!(
                "dependency '{}' requires an explicit [[local]] entry in {}",
                raw_dependency,
                lock_root.join(LOCK_FILE).display()
            ));
        };
        let dependency_dir = lock_root
            .join(relative_path)
            .canonicalize()
            .map_err(|error| {
                format!(
                    "canonicalize locked dependency '{}' at '{}': {error}",
                    dependency_name,
                    lock_root.join(relative_path).display()
                )
            })?;
        let dependency_manifest = read_manifest(&dependency_dir)?;
        if dependency_manifest.name != dependency_name {
            return Err(format!(
                "locked dependency '{}' points to app '{}'",
                dependency_name, dependency_manifest.name
            ));
        }
        collect_app(&dependency_dir, lock_root, locked_paths, state)?;
        dependencies.push(dependency_name);
    }
    dependencies.sort();
    dependencies.dedup();

    let files = collect_files(app_dir, state)?;
    let app_digest = digest_app_records(&manifest.name, &manifest.version, &dependencies, &files);
    state.app_digests.insert(manifest.name.clone(), app_digest);
    state.apps.insert(
        manifest.name.clone(),
        CanonicalAppManifest {
            name: manifest.name.clone(),
            version: manifest.version,
            dependencies,
            files,
        },
    );
    state.visiting.remove(&manifest.name);
    Ok(())
}

pub(super) fn read_manifest(app_dir: &Path) -> Result<AppManifest, String> {
    let path = app_dir.join("app.toml");
    let source = std::fs::read_to_string(&path)
        .map_err(|error| format!("read app manifest '{}': {error}", path.display()))?;
    let manifest: AppManifest = toml::from_str(&source)
        .map_err(|error| format!("parse app manifest '{}': {error}", path.display()))?;
    Ok(manifest)
}

pub(super) fn collect_files(
    app_dir: &Path,
    state: &mut BuildState,
) -> Result<Vec<CanonicalFileManifest>, String> {
    let mut paths = Vec::new();
    walk_files(app_dir, app_dir, 0, &mut state.tree_entries, &mut paths)?;
    paths.sort();
    let mut files = Vec::new();
    for path in paths {
        let relative = path
            .strip_prefix(app_dir)
            .map_err(|error| format!("strip app path '{}': {error}", path.display()))?;
        let relative = normalized_relative_path(relative)?;
        let before = std::fs::symlink_metadata(&path)
            .map_err(|error| format!("stat bundle file '{}': {error}", path.display()))?;
        if before.file_type().is_symlink() || !before.is_file() {
            return Err(format!(
                "bundle entry '{}' is not a regular file",
                path.display()
            ));
        }
        if before.len() > MAX_BUNDLE_FILE_BYTES {
            return Err(format!(
                "bundle file '{}' exceeds {MAX_BUNDLE_FILE_BYTES} bytes",
                path.display()
            ));
        }
        state.files = state
            .files
            .checked_add(1)
            .ok_or_else(|| "bundle file count overflowed".to_string())?;
        if state.files > MAX_BUNDLE_FILES {
            return Err(format!("bundle file count exceeds {MAX_BUNDLE_FILES}"));
        }
        state.bytes = state
            .bytes
            .checked_add(before.len())
            .ok_or_else(|| "bundle byte count overflowed".to_string())?;
        if state.bytes > MAX_BUNDLE_TOTAL_BYTES {
            return Err(format!("bundle bytes exceed {MAX_BUNDLE_TOTAL_BYTES}"));
        }
        let mut bytes = Vec::with_capacity(before.len() as usize);
        std::fs::File::open(&path)
            .map_err(|error| format!("open bundle file '{}': {error}", path.display()))?
            .take(before.len().saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|error| format!("read bundle file '{}': {error}", path.display()))?;
        let after = std::fs::symlink_metadata(&path)
            .map_err(|error| format!("restat bundle file '{}': {error}", path.display()))?;
        if bytes.len() as u64 != before.len()
            || after.len() != before.len()
            || after.modified().ok() != before.modified().ok()
        {
            return Err(format!(
                "bundle file '{}' changed while reading",
                path.display()
            ));
        }
        let digest = sha256_prefixed(&bytes);
        state.blobs.entry(digest.clone()).or_insert(bytes);
        files.push(CanonicalFileManifest {
            path: relative,
            size: before.len(),
            blob_digest: digest,
        });
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(files)
}

fn walk_files(
    root: &Path,
    dir: &Path,
    depth: usize,
    tree_entries: &mut usize,
    paths: &mut Vec<PathBuf>,
) -> Result<(), String> {
    if depth > MAX_BUNDLE_DEPTH {
        return Err(format!("bundle tree exceeds depth {MAX_BUNDLE_DEPTH}"));
    }
    let mut entries = std::fs::read_dir(dir)
        .map_err(|error| format!("read bundle directory '{}': {error}", dir.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("read bundle directory '{}': {error}", dir.display()))?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        *tree_entries = tree_entries
            .checked_add(1)
            .ok_or_else(|| "bundle tree entry count overflowed".to_string())?;
        if *tree_entries > MAX_BUNDLE_TREE_ENTRIES {
            return Err(format!(
                "bundle tree entries exceed {MAX_BUNDLE_TREE_ENTRIES}"
            ));
        }
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|error| format!("stat bundle entry '{}': {error}", path.display()))?;
        if file_type.is_symlink() {
            return Err(format!(
                "bundle entry '{}' must not be a symlink",
                path.display()
            ));
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|error| format!("strip bundle entry '{}': {error}", path.display()))?;
        let forbidden = relative.components().any(|component| {
            matches!(component, Component::Normal(part) if part == ".git" || part == "target")
        });
        if forbidden || relative == Path::new(LOCK_FILE) {
            continue;
        }
        if file_type.is_dir() {
            walk_files(root, &path, depth + 1, tree_entries, paths)?;
        } else if file_type.is_file() {
            paths.push(path);
        } else {
            return Err(format!(
                "bundle entry '{}' must be a regular file or directory",
                path.display()
            ));
        }
    }
    Ok(())
}

fn normalized_relative_path(path: &Path) -> Result<String, String> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => {
                let part = part
                    .to_str()
                    .ok_or_else(|| format!("bundle path '{}' is not UTF-8", path.display()))?;
                if part.is_empty() || part == "." || part == ".." {
                    return Err(format!("unsafe bundle path '{}'", path.display()));
                }
                parts.push(part);
            }
            _ => return Err(format!("unsafe bundle path '{}'", path.display())),
        }
    }
    if parts.is_empty() {
        return Err("bundle path must not be empty".to_string());
    }
    Ok(parts.join("/"))
}

pub(super) fn dependency_name(raw: &str) -> Result<String, String> {
    let unpinned = raw
        .trim()
        .split_once('@')
        .map_or(raw.trim(), |(left, _)| left);
    let name = unpinned.rsplit_once('/').map_or(unpinned, |(_, name)| name);
    validate_app_name(name)?;
    Ok(name.to_string())
}

fn validate_app_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.len() > 128
        || matches!(name, "." | "..")
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(format!("invalid application name '{name}'"));
    }
    Ok(())
}

pub(super) fn digest_manifest_records(root_app: &str, apps: &[CanonicalAppManifest]) -> String {
    let mut hasher = Sha256::new();
    digest_part(&mut hasher, b"temper-canonical-bundle-v1");
    digest_part(&mut hasher, root_app.as_bytes());
    for app in apps {
        digest_part(&mut hasher, app.name.as_bytes());
        digest_part(&mut hasher, app.version.as_bytes());
        for dependency in &app.dependencies {
            digest_part(&mut hasher, dependency.as_bytes());
        }
        for file in &app.files {
            digest_part(&mut hasher, file.path.as_bytes());
            digest_part(&mut hasher, &file.size.to_be_bytes());
            digest_part(&mut hasher, file.blob_digest.as_bytes());
        }
    }
    format!("sha256:{:x}", hasher.finalize())
}

pub(super) fn digest_app_records(
    name: &str,
    version: &str,
    dependencies: &[String],
    files: &[CanonicalFileManifest],
) -> String {
    let app = CanonicalAppManifest {
        name: name.to_string(),
        version: version.to_string(),
        dependencies: dependencies.to_vec(),
        files: files.to_vec(),
    };
    digest_manifest_records(name, &[app])
}

fn digest_part(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

pub(super) fn sha256_prefixed(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
#[path = "workspace_test.rs"]
mod tests;
