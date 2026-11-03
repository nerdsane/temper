//! Copy verified pre-tenant caches into a tenant-owned root, keeping exact file lists.

use std::path::Component;

use super::*;

/// Stage only digest-matching apps, verify the copy, and publish it with its list.
pub(super) async fn migrate(
    ps: &dyn PlatformStore,
    destination: &Path,
    apps: Vec<(String, AppFileSource)>,
    record: &InstalledAppRecord,
    recorded: &[InstalledAppRecord],
) -> Result<Vec<(String, AppFileSource)>, String> {
    let parent = destination.parent().ok_or("Genesis cache has no parent")?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("create tenant cache parent: {error}"))?;
    let staged = tempfile::Builder::new()
        .prefix(".genesis-migrate-")
        .tempdir_in(parent)
        .map_err(|error| format!("stage pre-tenant Genesis cache: {error}"))?;
    let mut listed = Written::new();
    for (app, files) in apps {
        let AppFileSource::Written { dir, paths } = files else {
            return Err("pre-tenant migration requires an exact file list".to_string());
        };
        let target = app_cache_dir(staged.path(), &app)?;
        for path in paths.iter() {
            copy_file(&dir, &target, path)?;
        }
        listed.insert(app, (*paths).clone());
    }
    written::keep(staged.path(), &listed)?;
    let verified = matching_cached_apps(ps, staged.path(), record, recorded, true)
        .await
        .ok_or("pre-tenant Genesis apps changed while copying")?;
    if verified.len() != listed.len() {
        return Err("pre-tenant Genesis dependency changed while copying".to_string());
    }
    replace_directory(staged.keep(), destination)?;
    written::read(destination)
}

/// Copy an exact relative file, rejecting symlinks in every source component.
fn copy_file(source: &Path, destination: &Path, path: &str) -> Result<(), String> {
    let relative = Path::new(path);
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(format!("invalid pre-tenant Genesis file path '{path}'"));
    }
    let regular = |path: &Path, directory: bool| -> Result<(), String> {
        let metadata = std::fs::symlink_metadata(path).map_err(|error| {
            format!("stat pre-tenant Genesis path '{}': {error}", path.display())
        })?;
        if (directory && metadata.is_dir()) || (!directory && metadata.is_file()) {
            Ok(())
        } else {
            Err(format!(
                "pre-tenant Genesis path '{}' is not a regular {}",
                path.display(),
                if directory { "directory" } else { "file" }
            ))
        }
    };
    regular(source, true)?;
    let mut file = source.to_path_buf();
    let mut parts = relative.components().peekable();
    while let Some(part) = parts.next() {
        file.push(part);
        regular(&file, parts.peek().is_some())?;
    }
    let target = destination.join(relative);
    let parent = target
        .parent()
        .ok_or("pre-tenant Genesis file has no parent")?;
    std::fs::create_dir_all(parent)
        .and_then(|()| std::fs::copy(&file, &target).map(|_| ()))
        .map_err(|error| format!("copy pre-tenant Genesis file '{}': {error}", file.display()))
}
