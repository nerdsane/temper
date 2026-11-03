//! The list of files an install wrote, kept in its cache directory beside
//! them.
//!
//! Install, boot and rollback all register and load an app from this list,
//! reading each file by its exact path, and never from a listing of the
//! folder: a folder can list short (on some drives empty, for a while after it
//! is renamed into place), and a short listing is not an error, so it would
//! load an app with fewer specs, or none. The list is part of what the bundle
//! digest covers: the digest is of the files it names, so a list that leaves a
//! file out does not match its install record. The static apps folder
//! (`TEMPER_OS_APPS_DIR`) has no such list, and is the one catalog still found
//! by listing.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use super::cache_paths::app_cache_dir;
use crate::os_apps::AppFileSource;

/// Where in a cache directory its list is kept. App folders are named by app
/// names, which never start with a dot.
const LIST: &str = ".written.json";

/// Each app's files: `/`-separated paths relative to `<cache root>/<app>`.
pub(super) type Written = BTreeMap<String, BTreeSet<String>>;

/// Keep `written` as the list of `root`, replacing any list there. It is
/// written beside and renamed over the old one, so a reader finds one list
/// whole.
pub(super) fn keep(root: &Path, written: &Written) -> Result<(), String> {
    let text = serde_json::to_vec(written)
        .map_err(|error| format!("encode the written-file list: {error}"))?;
    let staged = root.join(format!("{LIST}.new"));
    std::fs::write(&staged, text)
        .and_then(|()| std::fs::rename(&staged, root.join(LIST)))
        .map_err(|error| {
            format!(
                "keep the written-file list in '{}': {error}",
                root.display()
            )
        })
}

/// The apps of `root` as its install's list names them, each with exactly
/// the files it was written with. An error when there is no list, or it cannot
/// be read: such a directory is never served, and a caller fetches the files
/// again instead.
pub(super) fn read(root: &Path) -> Result<Vec<(String, AppFileSource)>, String> {
    app_files(root, load(root)?)
}

/// Whether `root` has a list at all, readable or not.
pub(super) fn exists(root: &Path) -> bool {
    root.join(LIST).exists()
}

fn load(root: &Path) -> Result<Written, String> {
    let path = root.join(LIST);
    let text = std::fs::read(&path)
        .map_err(|error| format!("'{}' could not be read: {error}", path.display()))?;
    serde_json::from_slice(&text)
        .map_err(|error| format!("'{}' is not a file list: {error}", path.display()))
}

/// Name `paths` as `app`'s files in `root`'s list, keeping the other apps it
/// names, as a test's hand-written cache directory needs.
#[cfg(test)]
pub(super) fn keep_app(root: &Path, app: &str, paths: &[&str]) {
    let mut written = load(root).unwrap_or_default();
    let paths = paths.iter().map(ToString::to_string).collect();
    written.insert(app.to_string(), paths);
    keep(root, &written).expect("keep the written-file list");
}

/// `written`'s apps, each at its folder in `root`.
pub(super) fn app_files(
    root: &Path,
    written: Written,
) -> Result<Vec<(String, AppFileSource)>, String> {
    written
        .into_iter()
        .map(|(app, paths)| {
            let dir = app_cache_dir(root, &app)?;
            Ok((app, AppFileSource::written(dir, paths)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_kept_list_reads_back_as_the_files_it_names() {
        let root = tempfile::tempdir().expect("root");
        assert!(read(root.path()).is_err(), "no list, nothing served");
        let written = Written::from([(
            "notes".to_string(),
            BTreeSet::from(["app.toml".to_string(), "specs/note.ioa.toml".to_string()]),
        )]);
        keep(root.path(), &written).expect("kept");
        let apps = read(root.path()).expect("read back");
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].0, "notes");
        assert_eq!(apps[0].1.dir(), root.path().join("notes"));

        std::fs::write(root.path().join(LIST), "not json").expect("broken");
        assert!(read(root.path()).is_err());
    }
}
