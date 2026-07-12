//! Spec ingestion path security (ADR-0159 / ARN-229).
//!
//! Fail-closed validation for inline spec map keys and staging under a
//! capability-owned, invocation-unique directory.

use std::path::{Component, Path, PathBuf};

use axum::http::StatusCode;

/// Max files accepted in one inline submission.
pub const MAX_INLINE_SPEC_FILES: usize = 256;

/// Max bytes for a single inline file.
pub const MAX_INLINE_FILE_BYTES: usize = 2 * 1024 * 1024;

/// Max total bytes across all inline files (including optional extras).
pub const MAX_INLINE_TOTAL_BYTES: usize = 8 * 1024 * 1024;

/// Normalized relative path plus content reference from an inline map entry.
pub type ValidatedInlineFile<'a> = (PathBuf, &'a str);

/// Validated inline bundle: files and cumulative byte total.
pub type ValidatedInlineBundle<'a> = (Vec<ValidatedInlineFile<'a>>, usize);

/// Handler error pair used across path-security helpers.
pub type PathSecurityError = (StatusCode, String);

/// Invocation-unique staging directory that is removed on drop.
///
/// Ensures cleanup on success, error, and panic after materialization begins.
pub struct InlineStagingDir {
    path: PathBuf,
    keep: bool,
}

impl InlineStagingDir {
    /// Borrow the staging root path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Prevent automatic removal (tests only).
    #[cfg(test)]
    pub fn keep_on_drop(&mut self) {
        self.keep = true;
    }
}

impl Drop for InlineStagingDir {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_dir_all(&self.path); // determinism-ok: HTTP staging cleanup
        }
    }
}

/// Validate a single inline map key as a relative, non-escaping path.
pub fn validate_inline_spec_key(key: &str) -> Result<PathBuf, PathSecurityError> {
    let key = key.trim();
    if key.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "inline spec key must not be empty".to_string(),
        ));
    }
    if key.contains('\0') {
        return Err((
            StatusCode::BAD_REQUEST,
            "inline spec key must not contain NUL".to_string(),
        ));
    }
    if key.starts_with('/') || key.starts_with('\\') {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("inline spec key must be relative (got absolute '{key}')"),
        ));
    }
    // Windows drive / UNC style
    if key.len() >= 2 && key.as_bytes()[1] == b':' {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("inline spec key must not include a drive prefix ('{key}')"),
        ));
    }

    let path = Path::new(key);
    if path.is_absolute() {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("inline spec key must be relative (got '{key}')"),
        ));
    }

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => {
                // Component::Normal never yields "." / ".." (those are CurDir/ParentDir).
                // Still reject embedded ".." tokens like "foo..bar".
                let s = part.to_string_lossy();
                if s.contains("..") {
                    return Err((
                        StatusCode::BAD_REQUEST,
                        format!("inline spec key '{key}' must not embed '..'"),
                    ));
                }
                normalized.push(part);
            }
            Component::CurDir => {}
            Component::ParentDir => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    format!("inline spec key '{key}' must not contain '..'"),
                ));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    format!("inline spec key '{key}' must not be absolute"),
                ));
            }
        }
    }

    if normalized.as_os_str().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("inline spec key '{key}' normalizes to empty"),
        ));
    }

    Ok(normalized)
}

/// Ensure `joined` stays under `root` (component-prefix containment, no `..`).
pub fn ensure_under_root(root: &Path, joined: &Path) -> Result<(), PathSecurityError> {
    // Reject any parent-dir component in the joined path (defense in depth).
    for component in joined.components() {
        if matches!(component, Component::ParentDir) {
            return Err((
                StatusCode::BAD_REQUEST,
                format!(
                    "inline path escapes staging root: {} contains '..'",
                    joined.display()
                ),
            ));
        }
    }

    let root_canon = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    // Prefer a canonical joined path when it already exists (closes symlink escapes
    // under the staging root). Fall back to the raw path for not-yet-created targets.
    let joined_check = joined
        .canonicalize()
        .unwrap_or_else(|_| joined.to_path_buf());
    if !joined_check.starts_with(root) && !joined_check.starts_with(&root_canon) {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "inline path escapes staging root: {} not under {}",
                joined.display(),
                root.display()
            ),
        ));
    }
    Ok(())
}

/// Enforce a single extra payload against the per-file and total budgets.
pub fn enforce_extra_payload_budget(
    label: &str,
    content: &str,
    used_total: usize,
) -> Result<usize, PathSecurityError> {
    if content.len() > MAX_INLINE_FILE_BYTES {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "inline file '{label}' exceeds size budget: {} > {}",
                content.len(),
                MAX_INLINE_FILE_BYTES
            ),
        ));
    }
    let total = used_total.saturating_add(content.len());
    if total > MAX_INLINE_TOTAL_BYTES {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "inline specs exceed total byte budget: > {}",
                MAX_INLINE_TOTAL_BYTES
            ),
        ));
    }
    Ok(total)
}

/// Validate entire inline map budgets and keys; return normalized relative paths
/// and total byte usage (for additional payloads).
pub fn validate_inline_bundle(
    specs: &std::collections::BTreeMap<String, String>,
) -> Result<ValidatedInlineBundle<'_>, PathSecurityError> {
    if specs.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "inline specs map must not be empty".to_string(),
        ));
    }
    if specs.len() > MAX_INLINE_SPEC_FILES {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "inline specs exceed file budget: {} > {}",
                specs.len(),
                MAX_INLINE_SPEC_FILES
            ),
        ));
    }

    let mut total: usize = 0;
    let mut out = Vec::with_capacity(specs.len());
    let mut seen = std::collections::BTreeSet::new();

    for (key, content) in specs {
        if content.len() > MAX_INLINE_FILE_BYTES {
            return Err((
                StatusCode::BAD_REQUEST,
                format!(
                    "inline file '{key}' exceeds size budget: {} > {}",
                    content.len(),
                    MAX_INLINE_FILE_BYTES
                ),
            ));
        }
        total = total.saturating_add(content.len());
        if total > MAX_INLINE_TOTAL_BYTES {
            return Err((
                StatusCode::BAD_REQUEST,
                format!(
                    "inline specs exceed total byte budget: > {}",
                    MAX_INLINE_TOTAL_BYTES
                ),
            ));
        }
        let rel = validate_inline_spec_key(key)?;
        let norm = rel.to_string_lossy().replace('\\', "/");
        if !seen.insert(norm.clone()) {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("duplicate normalized inline path '{norm}'"),
            ));
        }
        out.push((rel, content.as_str()));
    }

    Ok((out, total))
}

/// True when `path` is (or contains) an ephemeral inline staging directory.
///
/// Staging dirs are named `temper-inline-{tenant}-{uuid}` under the process
/// temp root and are deleted when [`InlineStagingDir`] drops. They must never
/// be written into `specs-registry.json`.
pub fn is_ephemeral_inline_staging_path(path: &str) -> bool {
    std::path::Path::new(path).components().any(|c| match c {
        std::path::Component::Normal(s) => s
            .to_str()
            .is_some_and(|name| name.starts_with("temper-inline-")),
        _ => false,
    })
}

/// Create an invocation-unique staging directory for inline specs.
///
/// Uses exclusive `create_dir` so an existing path cannot be clobbered.
pub fn create_inline_staging_dir(tenant: &str) -> Result<InlineStagingDir, PathSecurityError> {
    let safe_tenant: String = tenant
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    let id = temper_runtime::scheduler::sim_uuid();
    let dir = std::env::temp_dir().join(format!("temper-inline-{safe_tenant}-{id}")); // determinism-ok: HTTP staging
    // Exclusive create — fail if the path already exists (no clobber).
    std::fs::create_dir(&dir).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to create staging dir: {e}"),
        )
    })?;
    Ok(InlineStagingDir {
        path: dir,
        keep: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn detects_ephemeral_inline_staging_paths() {
        assert!(is_ephemeral_inline_staging_path(
            "/tmp/temper-inline-acme-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee/app"
        ));
        assert!(is_ephemeral_inline_staging_path(
            "/var/folders/xx/temper-inline-t1-uuid"
        ));
        assert!(!is_ephemeral_inline_staging_path(
            "/var/lib/temper/specs/acme"
        ));
        assert!(!is_ephemeral_inline_staging_path("/tmp/other-temper-data"));
    }

    #[test]
    fn rejects_parent_and_absolute_keys() {
        assert!(validate_inline_spec_key("../etc/passwd").is_err());
        assert!(validate_inline_spec_key("/etc/passwd").is_err());
        assert!(validate_inline_spec_key("foo/../../etc").is_err());
        assert!(validate_inline_spec_key("C:\\Windows\\system32").is_err());
        assert!(validate_inline_spec_key("foo\0bar").is_err());
        assert!(validate_inline_spec_key("").is_err());
    }

    #[test]
    fn accepts_nested_relative_keys() {
        let p = validate_inline_spec_key("app/model.csdl.xml").expect("ok");
        assert_eq!(p, PathBuf::from("app/model.csdl.xml"));
    }

    #[test]
    fn rejects_oversize_bundle() {
        let mut specs = BTreeMap::new();
        for i in 0..(MAX_INLINE_SPEC_FILES + 1) {
            specs.insert(format!("f{i}.ioa.toml"), "x".into());
        }
        assert!(validate_inline_bundle(&specs).is_err());
    }

    #[test]
    fn rejects_oversize_single_file() {
        let mut specs = BTreeMap::new();
        specs.insert("big.ioa.toml".into(), "x".repeat(MAX_INLINE_FILE_BYTES + 1));
        assert!(validate_inline_bundle(&specs).is_err());
    }

    #[test]
    fn rejects_oversize_total_bytes() {
        let mut specs = BTreeMap::new();
        // Several files under per-file cap that sum past total.
        let chunk = MAX_INLINE_FILE_BYTES;
        let need = (MAX_INLINE_TOTAL_BYTES / chunk) + 1;
        for i in 0..need {
            specs.insert(format!("f{i}.ioa.toml"), "y".repeat(chunk));
        }
        assert!(validate_inline_bundle(&specs).is_err());
    }

    #[test]
    fn rejects_duplicate_normalized_keys() {
        let mut specs = BTreeMap::new();
        specs.insert("a/b.ioa.toml".into(), "1".into());
        // Same path with redundant ./  — validate component-wise; both are distinct
        // string keys but if one has parent dir it fails earlier.
        specs.insert("./a/b.ioa.toml".into(), "2".into());
        // "./a/b" normalizes by skipping CurDir → same as a/b
        let err = validate_inline_bundle(&specs);
        assert!(err.is_err(), "duplicate after normalize should fail");
    }

    #[test]
    fn extra_payload_budget_enforced() {
        let used = MAX_INLINE_TOTAL_BYTES - 10;
        assert!(enforce_extra_payload_budget("cross", "x".repeat(20).as_str(), used).is_err());
        assert!(enforce_extra_payload_budget("cross", "ok", used).is_ok());
        assert!(
            enforce_extra_payload_budget("cross", &"z".repeat(MAX_INLINE_FILE_BYTES + 1), 0)
                .is_err()
        );
    }

    #[test]
    fn ensure_under_root_rejects_parent_components() {
        let root = PathBuf::from("/tmp/staging-root");
        let bad = root.join("a").join("..").join("escape");
        assert!(ensure_under_root(&root, &bad).is_err());
        let good = root.join("a").join("b.ioa.toml");
        assert!(ensure_under_root(&root, &good).is_ok());
    }

    #[test]
    fn staging_dirs_are_invocation_unique_for_same_tenant() {
        let a = create_inline_staging_dir("tenant-a").expect("a");
        let b = create_inline_staging_dir("tenant-a").expect("b");
        assert_ne!(a.path(), b.path());
        assert!(a.path().exists());
        assert!(b.path().exists());
        // Drop cleans both.
        let path_a = a.path().to_path_buf();
        let path_b = b.path().to_path_buf();
        drop(a);
        drop(b);
        assert!(!path_a.exists(), "staging A must be removed on drop");
        assert!(!path_b.exists(), "staging B must be removed on drop");
    }

    #[test]
    fn staging_create_is_exclusive() {
        let mut first = create_inline_staging_dir("excl").expect("first");
        first.keep_on_drop();
        let path = first.path().to_path_buf();
        // Manually attempt create_dir on the same path — should fail.
        let err = std::fs::create_dir(&path);
        assert!(err.is_err(), "exclusive create must fail when path exists");
        let _ = std::fs::remove_dir_all(&path);
    }
}
