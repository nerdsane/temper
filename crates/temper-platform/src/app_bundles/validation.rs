use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use super::types::CanonicalBundleManifestV1;
use super::workspace::{digest_manifest_records, sha256_prefixed};
use super::{MAX_BUNDLE_APPS, MAX_BUNDLE_FILE_BYTES, MAX_BUNDLE_FILES, MAX_BUNDLE_TOTAL_BYTES};

pub(super) fn validate_manifest(manifest: &CanonicalBundleManifestV1) -> Result<(), String> {
    if manifest.schema_version != 1 {
        return Err(format!(
            "unsupported bundle schema version {}",
            manifest.schema_version
        ));
    }
    if manifest.apps.is_empty() || manifest.apps.len() > MAX_BUNDLE_APPS {
        return Err(format!(
            "bundle app count must be between 1 and {MAX_BUNDLE_APPS}"
        ));
    }
    let mut app_names = BTreeSet::new();
    let mut file_count = 0usize;
    let mut byte_count = 0u64;
    let mut previous_app = None;
    let mut root_matches = 0usize;
    for app in &manifest.apps {
        validate_component("app name", &app.name)?;
        if previous_app.is_some_and(|previous| previous >= app.name.as_str()) {
            return Err("bundle apps must be strictly sorted by name".to_string());
        }
        previous_app = Some(app.name.as_str());
        if !app_names.insert(app.name.as_str()) {
            return Err(format!("duplicate bundle app '{}'", app.name));
        }
        root_matches += usize::from(app.name == manifest.root_app);
        let mut previous_dependency = None;
        for dependency in &app.dependencies {
            validate_component("dependency name", dependency)?;
            if previous_dependency.is_some_and(|previous| previous >= dependency.as_str()) {
                return Err(format!(
                    "dependencies for '{}' must be strictly sorted",
                    app.name
                ));
            }
            previous_dependency = Some(dependency.as_str());
        }
        let mut previous_path = None;
        for file in &app.files {
            validate_relative_path(&file.path)?;
            digest_hex(&file.blob_digest)?;
            if file.size > MAX_BUNDLE_FILE_BYTES {
                return Err(format!("bundle file '{}' exceeds size budget", file.path));
            }
            if previous_path.is_some_and(|previous| previous >= file.path.as_str()) {
                return Err(format!(
                    "bundle files for '{}' must be strictly sorted",
                    app.name
                ));
            }
            previous_path = Some(file.path.as_str());
            file_count = file_count
                .checked_add(1)
                .ok_or_else(|| "bundle file count overflowed".to_string())?;
            byte_count = byte_count
                .checked_add(file.size)
                .ok_or_else(|| "bundle byte count overflowed".to_string())?;
        }
    }
    if root_matches != 1 {
        return Err("bundle must contain exactly one root app".to_string());
    }
    let dependencies = manifest
        .apps
        .iter()
        .map(|app| (app.name.as_str(), app.dependencies.as_slice()))
        .collect::<BTreeMap<_, _>>();
    for (app, app_dependencies) in &dependencies {
        for dependency in *app_dependencies {
            if !dependencies.contains_key(dependency.as_str()) {
                return Err(format!(
                    "bundle app '{app}' references missing dependency '{dependency}'"
                ));
            }
        }
    }
    validate_dependency_graph(&manifest.root_app, &dependencies)?;
    if file_count > MAX_BUNDLE_FILES || byte_count > MAX_BUNDLE_TOTAL_BYTES {
        return Err("bundle exceeds file or aggregate byte budget".to_string());
    }
    let expected = digest_manifest_records(&manifest.root_app, &manifest.apps);
    if manifest.bundle_digest != expected {
        return Err(format!(
            "bundle manifest digest mismatch: expected {expected}, found {}",
            manifest.bundle_digest
        ));
    }
    Ok(())
}

fn validate_dependency_graph(
    app: &str,
    dependencies: &BTreeMap<&str, &[String]>,
) -> Result<(), String> {
    fn visit<'a>(
        app: &'a str,
        dependencies: &BTreeMap<&'a str, &'a [String]>,
        visiting: &mut BTreeSet<&'a str>,
        visited: &mut BTreeSet<&'a str>,
    ) -> Result<(), String> {
        if visited.contains(app) {
            return Ok(());
        }
        if !visiting.insert(app) {
            return Err(format!("cyclic bundle dependency detected at '{app}'"));
        }
        for dependency in dependencies.get(app).copied().unwrap_or_default() {
            visit(dependency, dependencies, visiting, visited)?;
        }
        visiting.remove(app);
        visited.insert(app);
        Ok(())
    }
    let mut visiting = BTreeSet::new();
    let mut visited = BTreeSet::new();
    visit(app, dependencies, &mut visiting, &mut visited)?;
    if visited.len() != dependencies.len() {
        return Err("bundle contains apps unreachable from its root".to_string());
    }
    Ok(())
}

pub(super) fn digest_hex(digest: &str) -> Result<&str, String> {
    let Some(hex) = digest.strip_prefix("sha256:") else {
        return Err(format!("digest '{digest}' must use sha256"));
    };
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(format!(
            "digest '{digest}' must contain 64 lowercase hex characters"
        ));
    }
    Ok(hex)
}

fn validate_component(label: &str, value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 128
        || matches!(value, "." | "..")
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(format!("invalid bundle {label} '{value}'"));
    }
    Ok(())
}

pub(super) fn validate_relative_path(path: &str) -> Result<PathBuf, String> {
    let relative = PathBuf::from(path);
    if relative.as_os_str().is_empty() {
        return Err("bundle path must not be empty".to_string());
    }
    let mut safe = PathBuf::new();
    for component in relative.components() {
        match component {
            Component::Normal(part) if part != ".git" && part != "target" => safe.push(part),
            _ => return Err(format!("unsafe bundle path '{path}'")),
        }
    }
    Ok(safe)
}

pub(super) fn validate_blob_file(path: &Path, digest: &str, size: u64) -> Result<(), String> {
    let bytes = std::fs::read(path)
        .map_err(|error| format!("read cached blob '{}': {error}", path.display()))?;
    if bytes.len() as u64 != size || sha256_prefixed(&bytes) != digest {
        return Err(format!(
            "cached blob '{}' failed integrity validation",
            path.display()
        ));
    }
    Ok(())
}

pub(super) fn validate_materialized_view(
    root: &Path,
    destination: &Path,
    manifest: &CanonicalBundleManifestV1,
) -> Result<(), String> {
    for app in &manifest.apps {
        for file in &app.files {
            let relative = validate_relative_path(&file.path)?;
            let path = destination.join(&app.name).join(relative);
            validate_blob_file(&path, &file.blob_digest, file.size)?;
            let source = root
                .join("blobs/sha256")
                .join(digest_hex(&file.blob_digest)?);
            validate_blob_file(&source, &file.blob_digest, file.size)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_bundles::types::CanonicalAppManifest;

    fn manifest(apps: Vec<CanonicalAppManifest>) -> CanonicalBundleManifestV1 {
        let root_app = "root".to_string();
        let bundle_digest = digest_manifest_records(&root_app, &apps);
        CanonicalBundleManifestV1 {
            schema_version: 1,
            root_app,
            apps,
            bundle_digest,
        }
    }

    #[test]
    fn manifest_rejects_missing_and_cyclic_dependencies() {
        let missing = manifest(vec![CanonicalAppManifest {
            name: "root".to_string(),
            version: "1.0.0".to_string(),
            dependencies: vec!["missing".to_string()],
            files: Vec::new(),
        }]);
        assert!(validate_manifest(&missing).unwrap_err().contains("missing"));

        let cyclic = manifest(vec![
            CanonicalAppManifest {
                name: "dependency".to_string(),
                version: "1.0.0".to_string(),
                dependencies: vec!["root".to_string()],
                files: Vec::new(),
            },
            CanonicalAppManifest {
                name: "root".to_string(),
                version: "1.0.0".to_string(),
                dependencies: vec!["dependency".to_string()],
                files: Vec::new(),
            },
        ]);
        assert!(validate_manifest(&cyclic).unwrap_err().contains("cyclic"));
    }
}
