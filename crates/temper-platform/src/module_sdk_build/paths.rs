use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

const DEPENDENCY_ROOT_BUDGET_V1: usize = 32;
const DEPENDENCY_DIRECTORY_ENTRY_BUDGET_V1: usize = 4_096;
const DEPENDENCY_CANDIDATE_BUDGET_V1: usize = 1_024;

pub(super) fn resolve_output(
    app: &Path,
    explicit: Option<&Path>,
    default_relative: &Path,
    label: &str,
) -> Result<PathBuf, String> {
    let candidate = explicit
        .map(Path::to_path_buf)
        .unwrap_or_else(|| app.join(default_relative));
    let absolute = if candidate.is_absolute() {
        candidate
    } else {
        app.join(candidate)
    };
    if absolute
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(format!("{label} path must not contain '..'"));
    }
    if !absolute.starts_with(app) {
        return Err(format!(
            "{label} path '{}' escapes app root '{}'",
            absolute.display(),
            app.display()
        ));
    }
    let mut existing_ancestor = absolute.as_path();
    while !existing_ancestor.exists() {
        existing_ancestor = existing_ancestor
            .parent()
            .ok_or_else(|| format!("{label} path has no existing ancestor"))?;
    }
    let canonical_ancestor = existing_ancestor.canonicalize().map_err(|error| {
        format!(
            "failed to resolve {label} ancestor '{}': {error}",
            existing_ancestor.display()
        )
    })?;
    if !canonical_ancestor.starts_with(app) {
        return Err(format!(
            "{label} path '{}' escapes app root '{}' through a symlink",
            absolute.display(),
            app.display()
        ));
    }
    let suffix = absolute.strip_prefix(existing_ancestor).map_err(|error| {
        format!(
            "failed to normalize {label} path '{}': {error}",
            absolute.display()
        )
    })?;
    if suffix.as_os_str().is_empty() {
        Ok(canonical_ancestor)
    } else {
        Ok(canonical_ancestor.join(suffix))
    }
}

pub(super) fn reject_path_aliases(paths: &[(&Path, &str)]) -> Result<(), String> {
    for (index, (left_path, left_label)) in paths.iter().enumerate() {
        for (right_path, right_label) in &paths[index + 1..] {
            if left_path == right_path {
                return Err(format!(
                    "{left_label} and {right_label} must be distinct paths ('{}')",
                    left_path.display()
                ));
            }
        }
    }
    Ok(())
}

pub(super) fn resolve_manifest_path(
    app: &Path,
    explicit: Option<&Path>,
) -> Result<PathBuf, String> {
    let path = resolve_output(app, explicit, Path::new("app.toml"), "app manifest")?;
    let canonical = path.canonicalize().map_err(|error| {
        format!(
            "failed to resolve app manifest '{}': {error}",
            path.display()
        )
    })?;
    if !canonical.starts_with(app) {
        return Err("app manifest escapes app root".into());
    }
    Ok(canonical)
}

pub(super) fn canonical_directory(path: &Path, label: &str) -> Result<PathBuf, String> {
    let canonical = path
        .canonicalize()
        .map_err(|error| format!("failed to resolve {label} '{}': {error}", path.display()))?;
    if !canonical.is_dir() {
        return Err(format!(
            "{label} '{}' is not a directory",
            canonical.display()
        ));
    }
    Ok(canonical)
}

pub(super) fn scan_dependency_roots(
    roots: &[PathBuf],
) -> Result<BTreeMap<String, Vec<PathBuf>>, String> {
    if roots.len() > DEPENDENCY_ROOT_BUDGET_V1 {
        return Err(format!(
            "dependency root budget exceeded: {} > {DEPENDENCY_ROOT_BUDGET_V1}",
            roots.len()
        ));
    }
    let mut candidates = BTreeMap::new();
    let mut candidate_count = 0_usize;
    for root in roots {
        let root = canonical_directory(root, "dependency root")?;
        let mut dirs = vec![root.clone()];
        let entries = std::fs::read_dir(&root).map_err(|error| {
            format!(
                "failed to read dependency root '{}': {error}",
                root.display()
            )
        })?;
        let mut children = Vec::new();
        for entry in entries {
            if children.len() >= DEPENDENCY_DIRECTORY_ENTRY_BUDGET_V1 {
                return Err(format!(
                    "dependency directory entry budget exceeded in '{}'",
                    root.display()
                ));
            }
            children.push(entry.map_err(|error| {
                format!(
                    "failed to read dependency root '{}': {error}",
                    root.display()
                )
            })?);
        }
        children.sort_by_key(std::fs::DirEntry::file_name);
        dirs.extend(children.into_iter().filter_map(|entry| {
            entry
                .file_type()
                .ok()
                .filter(|kind| kind.is_dir() || kind.is_symlink())?;
            Some(entry.path())
        }));
        for dir in dirs {
            let manifest = dir.join("app.toml");
            if !manifest.exists() {
                continue;
            }
            candidate_count += 1;
            if candidate_count > DEPENDENCY_CANDIDATE_BUDGET_V1 {
                return Err(format!(
                    "dependency candidate budget exceeded: {candidate_count} > {DEPENDENCY_CANDIDATE_BUDGET_V1}"
                ));
            }
            let canonical_dir = dir.canonicalize().map_err(|error| {
                format!(
                    "failed to resolve dependency candidate '{}': {error}",
                    dir.display()
                )
            })?;
            if !canonical_dir.starts_with(&root) {
                return Err(format!(
                    "dependency candidate '{}' escapes explicit root '{}'",
                    dir.display(),
                    root.display()
                ));
            }
            let name = canonical_dir
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| {
                    format!(
                        "dependency candidate '{}' has no UTF-8 directory name",
                        canonical_dir.display()
                    )
                })?;
            candidates
                .entry(name.to_string())
                .or_insert_with(Vec::new)
                .push(canonical_dir);
        }
    }
    for paths in candidates.values_mut() {
        paths.sort();
        paths.dedup();
    }
    Ok(candidates)
}
