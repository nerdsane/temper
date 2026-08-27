use std::path::{Path, PathBuf};

pub(super) fn set_read_only(path: &Path) -> Result<(), String> {
    let mut permissions = std::fs::metadata(path)
        .map_err(|error| format!("stat cache file '{}': {error}", path.display()))?
        .permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(path, permissions)
        .map_err(|error| format!("protect cache file '{}': {error}", path.display()))
}

pub(super) fn replace_directory(staged: PathBuf, destination: &Path) -> Result<(), String> {
    if !destination.exists() {
        std::fs::rename(&staged, destination)
            .map_err(|error| format!("publish bundle view '{}': {error}", destination.display()))?;
        return super::sync_directory(
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
    super::sync_directory(parent)
}
