//! Filesystem-safety helpers for inline spec ingestion (ARN-229, ADR-0163).
//!
//! `POST /api/specs/load-inline` materializes caller-supplied spec files under a
//! private per-request temp dir. Every caller-controlled path segment — the spec
//! filenames and the tenant-derived temp-dir leaf — is validated here before it
//! reaches the filesystem, and the temp dir is cleaned up on every exit path.

use std::path::{Component, Path, PathBuf};

use axum::http::StatusCode;

/// Resolve a caller-supplied inline-spec filename to a path guaranteed to stay
/// under `tmp_dir`. The `specs` keys come from the request body, so a traversal
/// (`../`) or absolute name must be rejected before any filesystem write escapes
/// the private temp dir.
pub(super) fn safe_inline_spec_path(
    tmp_dir: &Path,
    filename: &str,
) -> Result<PathBuf, (StatusCode, String)> {
    let rel = Path::new(filename);
    if rel.as_os_str().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "inline spec path must not be empty".to_string(),
        ));
    }
    // Every component must be a plain name: `..` is `ParentDir`, an absolute path
    // starts with `RootDir`/`Prefix`, `.` is `CurDir` — all rejected, so the join
    // can only ever land under `tmp_dir`. Nested relative paths (app subdirs) are
    // allowed, since inline submissions legitimately carry them.
    let mut safe = PathBuf::new();
    for component in rel.components() {
        match component {
            Component::Normal(part) => safe.push(part),
            _ => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    format!(
                        "inline spec path '{filename}' must be a relative path with no '..', \
                         '.', or absolute segments"
                    ),
                ));
            }
        }
    }
    Ok(tmp_dir.join(safe))
}

/// Resolve the per-request temp-dir leaf name to a direct child of `temp_root`.
/// Unlike `safe_inline_spec_path` (which permits nested spec paths), the leaf must
/// be exactly ONE `Component::Normal`: it embeds the caller-supplied tenant, and a
/// tenant containing `/` would make only the final component uuid-suffixed, leaving
/// a *predictable* intermediate dir a local attacker could pre-plant a symlink at.
/// A single component keeps the whole dir unpredictable.
pub(super) fn safe_temp_dir_leaf(
    temp_root: &Path,
    leaf: &str,
) -> Result<PathBuf, (StatusCode, String)> {
    let mut components = Path::new(leaf).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(part)), None) => Ok(temp_root.join(part)),
        _ => Err((
            StatusCode::BAD_REQUEST,
            "invalid inline spec request: tenant must be a single safe path component \
             (no '/', '..', or absolute segments)"
                .to_string(),
        )),
    }
}

/// RAII cleanup for the private per-request temp dir. Removing on `Drop` covers
/// every exit path — success, a validation `400`, or a write I/O error — so a
/// stream of malformed requests can't accumulate abandoned dirs.
pub(super) struct TempDirGuard(pub(super) PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0); // determinism-ok: HTTP handler removes its private temp dir
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_inline_spec_path_rejects_traversal_and_absolute() {
        // ARN-229: `specs` keys come from the request body. A traversal or absolute
        // key must be rejected before it escapes the private temp dir.
        let root = Path::new("/var/tmp/temper-inline-abc");
        safe_inline_spec_path(root, "../../etc/cron.d/evil")
            .expect_err("traversal key must be rejected");
        safe_inline_spec_path(root, "/etc/passwd").expect_err("absolute key must be rejected");
        safe_inline_spec_path(root, "..").expect_err("parent key must be rejected");
        safe_inline_spec_path(root, "").expect_err("empty key must be rejected");
        safe_inline_spec_path(root, "a/../../b").expect_err("mid-path traversal must be rejected");
        // Legitimate relative spec paths (including nested app dirs) are allowed.
        assert_eq!(
            safe_inline_spec_path(root, "model.csdl.xml").expect("simple key allowed"),
            root.join("model.csdl.xml")
        );
        assert_eq!(
            safe_inline_spec_path(root, "app/order.ioa.toml").expect("nested key allowed"),
            root.join("app/order.ioa.toml")
        );
    }

    #[test]
    fn safe_temp_dir_leaf_requires_single_component() {
        // ARN-229: the tenant is interpolated into the temp-dir leaf name, which must
        // stay a single unpredictable directory.
        let temp_root = Path::new("/var/folders/tmp");
        // Traversal tenant escapes the temp root.
        safe_temp_dir_leaf(temp_root, "temper-inline-a/../../etc-01890000")
            .expect_err("tenant traversal in the leaf name must be rejected");
        // A tenant containing `/` (permitted by TenantId::new) would leave a
        // predictable intermediate dir — reject the nested leaf.
        safe_temp_dir_leaf(temp_root, "temper-inline-evil/x-01890000")
            .expect_err("nested leaf (tenant with '/') must be rejected");
        // A normal tenant yields a single child dir under the temp root.
        assert_eq!(
            safe_temp_dir_leaf(temp_root, "temper-inline-default-01890000")
                .expect("normal tenant leaf allowed"),
            temp_root.join("temper-inline-default-01890000")
        );
    }
}
