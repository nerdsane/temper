//! Deterministic discovery of OS app specifications and deployment policy.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::{AppDeploymentMode, AppManifest};

/// Finds all IOA spec files in an app directory.
///
/// Root-level specs take precedence over duplicate filenames under `specs/`.
pub(super) fn find_ioa_files(app_dir: &Path) -> Vec<(String, PathBuf)> {
    let mut results = Vec::new();
    let mut seen_names = HashSet::new();

    scan_dir_for_ioa(app_dir, &mut results, &mut seen_names);

    let specs_dir = app_dir.join("specs");
    if specs_dir.is_dir() {
        scan_dir_for_ioa(&specs_dir, &mut results, &mut seen_names);
    }

    results
}

fn scan_dir_for_ioa(dir: &Path, results: &mut Vec<(String, PathBuf)>, seen: &mut HashSet<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<_> = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".ioa.toml"))
        .collect();
    files.sort_by_key(|entry| entry.file_name());

    for entry in files {
        let path = entry.path();
        let file_name = entry.file_name().to_string_lossy().to_string();
        if seen.insert(file_name) {
            results.push((String::new(), path));
        }
    }
}

/// Finds the CSDL model file for an app, in deterministic precedence order.
pub(super) fn find_csdl(app_dir: &Path) -> Option<PathBuf> {
    let root = app_dir.join("model.csdl.xml");
    if root.exists() {
        return Some(root);
    }

    let specs = app_dir.join("specs").join("model.csdl.xml");
    if specs.exists() {
        return Some(specs);
    }

    let csdl_dir = app_dir.join("csdl");
    if !csdl_dir.is_dir() {
        return None;
    }
    let Ok(entries) = std::fs::read_dir(csdl_dir) else {
        return None;
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".csdl.xml"))
        .map(|entry| entry.path())
        .collect();
    files.sort();
    files.into_iter().next()
}

/// Finds the app-owned Cedar policy files in deterministic order.
pub(super) fn find_cedar_policies(app_dir: &Path) -> Vec<PathBuf> {
    cedar_policy_files_in(&app_dir.join("policies"))
}

/// Finds the Cedar commons policy files in deterministic order.
pub(super) fn find_commons_cedar_policies(app_dir: &Path) -> Vec<PathBuf> {
    cedar_policy_files_in(&app_dir.join("policies").join("commons"))
}

fn cedar_policy_files_in(policies_dir: &Path) -> Vec<PathBuf> {
    if !policies_dir.is_dir() {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(policies_dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".cedar"))
        .map(|entry| entry.path())
        .collect();
    files.sort();
    files
}

/// Returns an app-relative path with platform-independent separators.
pub(super) fn app_relative_path(app_dir: &Path, path: &Path) -> String {
    path.strip_prefix(app_dir)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Resolves the app deployment mode, honoring the documented environment overrides.
pub(super) fn effective_app_deployment_mode(manifest: &AppManifest) -> AppDeploymentMode {
    let app_key = manifest
        .name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect::<String>();
    let specific_key = format!("TEMPER_OS_APP_{app_key}_MODE");
    let raw = std::env::var(&specific_key)
        .or_else(|_| std::env::var("TEMPER_OS_APP_MODE"))
        .ok();

    match raw.as_deref().map(str::trim) {
        Some("commons") | Some("Commons") | Some("COMMONS") => AppDeploymentMode::Commons,
        Some("operator") | Some("Operator") | Some("OPERATOR") => AppDeploymentMode::Operator,
        _ => manifest.mode,
    }
}
