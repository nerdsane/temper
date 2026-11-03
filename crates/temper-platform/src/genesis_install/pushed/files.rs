//! A pushed version's files: checked, hashed, and written whole into its
//! cache directory.

use std::collections::BTreeMap;
use std::path::Path;

use base64::Engine as _;
use sha2::{Digest as _, Sha256};

use super::{PushRefused, PushedFile};
use crate::genesis_install::written::{self, Written};

/// Refuse a path that is not a plain relative path inside the app: absolute,
/// with an empty, `.` or `..` part, or with a NUL byte. A `.` part would let
/// two paths name one file, and `..` or an absolute path a file outside it.
fn check_path(path: &str) -> Result<(), String> {
    let plain = !path.starts_with('/')
        && !path.contains('\0')
        && path.split('/').all(|part| !matches!(part, "" | "." | ".."));
    if plain {
        Ok(())
    } else {
        Err(format!(
            "'{path}' is not a relative path inside the app: no leading '/', and no empty, '.' or '..' parts"
        ))
    }
}

/// Each file's bytes. A kept file is read from `kept_from`, the installed
/// version's app directory, and must hash to what the caller named.
pub(super) fn resolve(
    files: BTreeMap<String, PushedFile>,
    kept_from: Option<&Path>,
) -> Result<BTreeMap<String, Vec<u8>>, PushRefused> {
    let mut resolved = BTreeMap::new();
    for (path, file) in files {
        check_path(&path).map_err(PushRefused::Request)?;
        let bytes = match file {
            PushedFile::Text(text) => text.into_bytes(),
            PushedFile::Bytes(file) => base64::engine::general_purpose::STANDARD
                .decode(file.base64.as_bytes())
                .map_err(|error| {
                    PushRefused::Request(format!("'{path}' is not valid base64: {error}"))
                })?,
            PushedFile::Kept(file) => {
                let Some(dir) = kept_from else {
                    return Err(PushRefused::Request(format!(
                        "'{path}' is kept, but this app has no pushed version installed to keep it from"
                    )));
                };
                let bytes = std::fs::read(dir.join(&path)).map_err(|_| {
                    PushRefused::Request(format!(
                        "'{path}' is kept, but the installed version has no such file"
                    ))
                })?;
                if !sha256_hex(&bytes).eq_ignore_ascii_case(&file.keep) {
                    return Err(PushRefused::Request(format!(
                        "'{path}' is kept as {}, but the installed version's file hashes to {}",
                        file.keep,
                        sha256_hex(&bytes)
                    )));
                }
                bytes
            }
        };
        resolved.insert(path, bytes);
    }
    // A path that is also another's directory cannot be written.
    for path in resolved.keys() {
        let mut parent = path.as_str();
        while let Some((dir, _)) = parent.rsplit_once('/') {
            if resolved.contains_key(dir) {
                return Err(PushRefused::Request(format!(
                    "'{dir}' is a file, so '{path}' cannot be inside it"
                )));
            }
            parent = dir;
        }
    }
    Ok(resolved)
}

/// The digest of every file's path and bytes, in path order. Each part is
/// length-prefixed so no two lists run together into the same bytes.
pub(super) fn digest(files: &BTreeMap<String, Vec<u8>>) -> String {
    let mut hasher = Sha256::new();
    for (path, bytes) in files {
        hasher.update((path.len() as u64).to_le_bytes());
        hasher.update(path.as_bytes());
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    }
    format!("{:x}", hasher.finalize())
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Whether a cache still names and holds exactly the requested files.
/// Read by explicit path, including the saved list; no directory listing is
/// needed for either validation or repair.
pub(super) fn matches(root: &Path, app: &str, files: &BTreeMap<String, Vec<u8>>) -> bool {
    let Ok(apps) = written::read(root) else {
        return false;
    };
    let [(name, source)] = apps.as_slice() else {
        return false;
    };
    let crate::os_apps::AppFileSource::Written { paths, .. } = source else {
        return false;
    };
    name == app
        && paths.iter().eq(files.keys())
        && files
            .iter()
            .all(|(path, bytes)| source.read(path).is_ok_and(|stored| stored == *bytes))
}

/// Write `files` as `<root>/<app>/`, with their list ([`written`]). They are
/// written to a directory beside `root` and renamed into place, so a directory
/// with a version's name always holds all of that version's files.
pub(super) fn materialize(
    root: &Path,
    app: &str,
    files: &BTreeMap<String, Vec<u8>>,
) -> std::io::Result<()> {
    let parent = root
        .parent()
        .ok_or_else(|| std::io::Error::other("a pushed version's directory has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let staging = tempfile::Builder::new()
        .prefix(".pushed-staging-")
        .tempdir_in(parent)?;
    let app_dir = staging.path().join(app);
    for (path, bytes) in files {
        let file = app_dir.join(path);
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&file, bytes)?;
    }
    let paths = files.keys().cloned().collect();
    written::keep(staging.path(), &Written::from([(app.to_string(), paths)]))
        .map_err(std::io::Error::other)?;
    super::replace_directory(staging.path().to_path_buf(), root).map_err(std::io::Error::other)?;
    // Renamed into place: nothing is left for the staging guard to remove.
    let _ = staging.keep();
    Ok(())
}

/// Every regular file under `dir`, by its `/`-separated path relative to it.
pub(super) fn read_tree(dir: &Path) -> std::io::Result<BTreeMap<String, Vec<u8>>> {
    let mut tree = BTreeMap::new();
    let mut dirs = vec![dir.to_path_buf()];
    while let Some(next) = dirs.pop() {
        for entry in std::fs::read_dir(&next)? {
            let path = entry?.path();
            let metadata = std::fs::metadata(&path)?;
            if metadata.is_dir() {
                dirs.push(path);
                continue;
            }
            if !metadata.is_file() {
                continue;
            }
            let relative = path.strip_prefix(dir).map_err(std::io::Error::other)?;
            let parts: Vec<String> = relative
                .components()
                .map(|part| part.as_os_str().to_string_lossy().into_owned())
                .collect();
            tree.insert(parts.join("/"), std::fs::read(&path)?);
        }
    }
    Ok(tree)
}
