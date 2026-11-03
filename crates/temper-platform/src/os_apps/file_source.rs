//! The files of one app folder, and how the loader and the catalog find
//! them.
//!
//! An install knows every file it wrote, so it names them and keeps their
//! list beside them, and nothing it installs, restores at boot or rolls back
//! to depends on listing a folder. A listing that comes back short is not an
//! error, so a folder that lists short (on some drives a folder can list empty
//! for a while after it is renamed into place) would otherwise install as an
//! app with fewer specs, or none, and pass zero checks. Only the static apps
//! folders (`TEMPER_OS_APPS_DIR`, and folders added whole), which no install
//! wrote and which have no list, are found by listing.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The files of one app folder.
#[derive(Clone, Debug)]
pub enum AppFileSource {
    /// Found by listing the folder when they are read.
    Listed(PathBuf),
    /// Named by whoever wrote them: `/`-separated paths relative to `dir`.
    Written {
        dir: PathBuf,
        paths: Arc<BTreeSet<String>>,
    },
}

impl AppFileSource {
    /// The files at `paths`, relative to `dir`, as the install that wrote
    /// them names them.
    pub fn written(dir: PathBuf, paths: impl IntoIterator<Item = String>) -> Self {
        Self::Written {
            dir,
            paths: Arc::new(paths.into_iter().collect()),
        }
    }

    /// The app's folder.
    pub fn dir(&self) -> &Path {
        match self {
            Self::Listed(dir) | Self::Written { dir, .. } => dir,
        }
    }

    /// Whether the app has a file at `path`.
    pub(super) fn has(&self, path: &str) -> bool {
        match self {
            Self::Listed(dir) => dir.join(path).is_file(),
            Self::Written { paths, .. } => paths.contains(path),
        }
    }

    /// Whether the app has a folder at `path` holding any file.
    pub(super) fn has_dir(&self, path: &str) -> bool {
        match self {
            Self::Listed(dir) => dir.join(path).is_dir(),
            Self::Written { paths, .. } => {
                let prefix = format!("{path}/");
                paths.iter().any(|file| file.starts_with(&prefix))
            }
        }
    }

    /// The names of the files directly in folder `path` (`""` is the app's
    /// own folder), sorted.
    pub(super) fn files_in(&self, path: &str) -> Vec<String> {
        self.children(path, false)
    }

    /// The names of the folders directly in folder `path`, sorted.
    pub(super) fn dirs_in(&self, path: &str) -> Vec<String> {
        self.children(path, true)
    }

    fn children(&self, path: &str, folders: bool) -> Vec<String> {
        let mut names: Vec<String> = match self {
            Self::Listed(dir) => list(&dir.join(path))
                .into_iter()
                .filter(|(_, is_dir)| *is_dir == folders)
                .map(|(name, _)| name)
                .collect(),
            Self::Written { paths, .. } => {
                let prefix = folder_prefix(path);
                paths
                    .iter()
                    .filter_map(|file| file.strip_prefix(&prefix))
                    .filter_map(|rest| match rest.split_once('/') {
                        Some((folder, _)) if folders => Some(folder.to_string()),
                        None if !folders => Some(rest.to_string()),
                        _ => None,
                    })
                    .collect()
            }
        };
        names.sort();
        names.dedup();
        names
    }

    /// Every file below folder `path`, relative to it, sorted.
    pub(super) fn files_under(&self, path: &str) -> Vec<String> {
        let mut found = Vec::new();
        match self {
            Self::Written { paths, .. } => {
                let prefix = folder_prefix(path);
                found.extend(
                    paths
                        .iter()
                        .filter_map(|file| file.strip_prefix(&prefix))
                        .map(str::to_string),
                );
            }
            Self::Listed(_) => {
                let mut folders = vec![String::new()];
                while let Some(folder) = folders.pop() {
                    let full = join(path, &folder);
                    folders.extend(self.dirs_in(&full).iter().map(|d| join(&folder, d)));
                    found.extend(self.files_in(&full).iter().map(|f| join(&folder, f)));
                }
            }
        }
        found.sort();
        found
    }

    /// The bytes of the file at `path`. A file the app names that cannot be
    /// read is an error naming it, never a file left out.
    pub(crate) fn read(&self, path: &str) -> Result<Vec<u8>, String> {
        let full = self.dir().join(path);
        if made_unreadable(&full) {
            return Err(format!(
                "{path} could not be read: made unreadable by a test"
            ));
        }
        std::fs::read(&full).map_err(|error| format!("{path} could not be read: {error}"))
    }

    /// The text of the file at `path`, as [`Self::read`].
    pub(crate) fn read_text(&self, path: &str) -> Result<String, String> {
        String::from_utf8(self.read(path)?).map_err(|_| format!("{path} is not UTF-8 text"))
    }
}

fn folder_prefix(path: &str) -> String {
    if path.is_empty() {
        String::new()
    } else {
        format!("{path}/")
    }
}

/// Every app folder of install directory `root`, each with every file below
/// it, found by listing `root` once. Folders whose names start with a dot
/// (an install's staging folders) are not apps.
///
/// Only for adopting an install cache written before installs kept their
/// file list (see `genesis_install::restore::cache_recovery`); delete it with
/// that adoption.
pub fn list_install_dir(root: &Path) -> Vec<(String, BTreeSet<String>)> {
    list(root)
        .into_iter()
        .filter(|(name, is_dir)| *is_dir && !name.starts_with('.'))
        .map(|(name, _)| {
            let files = AppFileSource::Listed(root.join(&name)).files_under("");
            (name, files.into_iter().collect())
        })
        .collect()
}

/// `folder/name`, where an empty `folder` is the app's own.
pub(super) fn join(folder: &str, name: &str) -> String {
    match (folder.is_empty(), name.is_empty()) {
        (true, _) => name.to_string(),
        (_, true) => folder.to_string(),
        _ => format!("{folder}/{name}"),
    }
}

/// The names in folder `dir`, each with whether it is a folder: the one place
/// the catalog and the loader list a folder. Empty when it cannot be listed.
pub(super) fn list(dir: &Path) -> Vec<(String, bool)> {
    if made_to_list_empty(dir) {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            let name = entry.file_name().into_string().ok()?;
            if path.is_dir() {
                Some((name, true))
            } else if path.is_file() {
                Some((name, false))
            } else {
                None
            }
        })
        .collect()
}

#[cfg(test)]
fn made_to_list_empty(dir: &Path) -> bool {
    seam::lists_empty(dir)
}

#[cfg(not(test))]
fn made_to_list_empty(_: &Path) -> bool {
    false
}

#[cfg(test)]
fn made_unreadable(path: &Path) -> bool {
    seam::unreadable(path)
}

#[cfg(not(test))]
fn made_unreadable(_: &Path) -> bool {
    false
}

/// Folders that list empty and files that cannot be read, set by tests.
#[cfg(test)]
pub(crate) mod seam {
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    static LIST_EMPTY: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
    static UNREADABLE: Mutex<Vec<(PathBuf, String)>> = Mutex::new(Vec::new());

    /// From now on every folder at or below `root` lists empty, as a folder
    /// just renamed into place can on a drive whose listings lag. Each test
    /// passes its own temporary folder, so tests that run at once never see
    /// each other's.
    pub(crate) fn list_empty_under(root: &Path) {
        LIST_EMPTY
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(root.to_path_buf());
    }

    /// From now on reading any file at or below `root` whose path ends in
    /// `name` fails.
    pub(crate) fn unreadable_under(root: &Path, name: &str) {
        UNREADABLE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push((root.to_path_buf(), name.to_string()));
    }

    pub(super) fn lists_empty(dir: &Path) -> bool {
        LIST_EMPTY
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .any(|root| dir.starts_with(root))
    }

    pub(super) fn unreadable(path: &Path) -> bool {
        UNREADABLE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .any(|(root, name)| path.starts_with(root) && path.ends_with(name))
    }
}
