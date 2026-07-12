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

/// Max total bytes across all inline files.
pub const MAX_INLINE_TOTAL_BYTES: usize = 8 * 1024 * 1024;

/// Validate a single inline map key as a relative, non-escaping path.
pub fn validate_inline_spec_key(key: &str) -> Result<PathBuf, (StatusCode, String)> {
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
                let s = part.to_string_lossy();
                if s == ".." || s == "." {
                    return Err((
                        StatusCode::BAD_REQUEST,
                        format!("inline spec key '{key}' has forbidden component"),
                    ));
                }
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

/// Ensure `joined` stays under `root` (string-prefix containment after normalize).
pub fn ensure_under_root(root: &Path, joined: &Path) -> Result<(), (StatusCode, String)> {
    let root_canon = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    // Parent of joined may not exist yet — walk components against root.
    if !joined.starts_with(root) && !joined.starts_with(&root_canon) {
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

/// Validate entire inline map budgets and keys; return normalized relative paths.
pub fn validate_inline_bundle(
    specs: &std::collections::BTreeMap<String, String>,
) -> Result<Vec<(PathBuf, &str)>, (StatusCode, String)> {
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

    Ok(out)
}

/// Create an invocation-unique staging directory for inline specs.
pub fn create_inline_staging_dir(tenant: &str) -> Result<PathBuf, (StatusCode, String)> {
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
    std::fs::create_dir_all(&dir).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to create staging dir: {e}"),
        )
    })?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn rejects_parent_and_absolute_keys() {
        assert!(validate_inline_spec_key("../etc/passwd").is_err());
        assert!(validate_inline_spec_key("/etc/passwd").is_err());
        assert!(validate_inline_spec_key("foo/../../etc").is_err());
        assert!(validate_inline_spec_key("C:\\Windows\\system32").is_err());
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
}
