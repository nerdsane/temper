use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{OnceLock, RwLock};

use super::{AppEntry, StartupInstallMode, extract_description, read_app_guide, read_app_manifest};

pub(crate) struct AppCatalog {
    /// Directory containing app bundles.
    pub(super) apps_dir: PathBuf,
    /// Additional app directories merged into the catalog.
    additional_dirs: Vec<PathBuf>,
    /// Catalog entries (lightweight metadata).
    pub(super) entries: Vec<AppEntry>,
    /// Mapping from app name to its directory path on disk.
    pub(super) paths: BTreeMap<String, PathBuf>,
}

/// Global catalog, initialized on first access.
static CATALOG: OnceLock<RwLock<AppCatalog>> = OnceLock::new();

/// Get or initialize the global app catalog.
pub fn catalog() -> &'static RwLock<AppCatalog> {
    CATALOG.get_or_init(|| RwLock::new(AppCatalog::discover()))
}

/// Override the OS apps directory. Must be called before any catalog access.
///
/// If the catalog was already initialized, it is replaced.
pub fn set_os_apps_dir(dir: PathBuf) {
    let new_catalog = AppCatalog::from_dir(dir);
    match CATALOG.get() {
        Some(lock) => {
            *lock.write().unwrap() = new_catalog; // ci-ok: infallible lock
        }
        None => {
            let _ = CATALOG.set(RwLock::new(new_catalog));
        }
    }
}

/// Add an additional directory of apps to the catalog.
///
/// Scans the directory and merges discovered apps into the existing catalog.
/// Apps in the new directory do NOT replace existing apps with the same name.
/// Use this to register reference apps or project-specific apps alongside
/// the main os-apps directory.
pub fn add_os_apps_dir(dir: PathBuf) {
    let additional = AppCatalog::from_dir(dir);
    let cat = catalog();
    let mut lock = cat.write().unwrap(); // ci-ok: infallible lock
    lock.merge_catalog(additional);
}

/// Re-scan the OS apps directory and refresh the catalog.
///
/// Call this after modifying app files on disk to pick up changes
/// without restarting the server.
pub fn reload_os_apps() {
    let cat = catalog().read().unwrap(); // ci-ok: infallible lock
    let dir = cat.apps_dir.clone();
    let additional_dirs: Vec<PathBuf> = cat
        .additional_dirs
        .iter()
        .filter(|dir| dir.is_dir())
        .cloned()
        .collect();
    drop(cat);
    let mut new = AppCatalog::from_dir(dir);
    for additional_dir in additional_dirs {
        new.merge_dir(additional_dir);
    }
    *catalog().write().unwrap() = new; // ci-ok: infallible lock
}

/// Backward-compatible alias.
pub fn set_skills_dir(dir: PathBuf) {
    set_os_apps_dir(dir);
}

/// Backward-compatible alias.
pub fn reload_skills() {
    reload_os_apps();
}

/// List OS apps that belong to the default startup surface.
pub fn list_startup_os_apps() -> Vec<String> {
    let cat = match catalog().read() {
        Ok(cat) => cat,
        Err(poisoned) => poisoned.into_inner(),
    };
    let mut apps: Vec<String> = cat
        .entries
        .iter()
        .filter(|entry| entry.startup_install == StartupInstallMode::Core)
        .map(|entry| entry.name.clone())
        .collect();
    apps.sort();
    apps
}

impl AppCatalog {
    /// Discover the apps directory and scan it.
    fn discover() -> Self {
        // Priority 1: TEMPER_OS_APPS_DIR env var.
        if let Ok(dir) = std::env::var("TEMPER_OS_APPS_DIR") {
            // determinism-ok: env var read at startup for configuration
            let path = PathBuf::from(dir);
            if path.is_dir() {
                tracing::info!(
                    "Loading OS apps from TEMPER_OS_APPS_DIR: {}",
                    path.display()
                );
                return Self::from_dir(path);
            }
        }

        // Priority 1b: legacy TEMPER_SKILLS_DIR env var.
        if let Ok(dir) = std::env::var("TEMPER_SKILLS_DIR") {
            let path = PathBuf::from(dir);
            if path.is_dir() {
                tracing::info!(
                    "Loading OS apps from legacy TEMPER_SKILLS_DIR: {}",
                    path.display()
                );
                return Self::from_dir(path);
            }
        }

        // Priority 2: Relative to this crate's source (works in dev and cargo test).
        let compile_time_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("os-apps");
        if compile_time_dir.is_dir() {
            let canonical = compile_time_dir
                .canonicalize()
                .unwrap_or(compile_time_dir.clone());
            tracing::info!("Loading OS apps from workspace: {}", canonical.display());
            return Self::from_dir(canonical);
        }

        // Priority 3: ./os-apps/ relative to CWD.
        let cwd_dir = PathBuf::from("os-apps");
        if cwd_dir.is_dir() {
            let canonical = cwd_dir.canonicalize().unwrap_or(cwd_dir.clone());
            tracing::info!("Loading OS apps from CWD: {}", canonical.display());
            return Self::from_dir(canonical);
        }

        // Priority 4: ./skills/ (legacy fallback).
        let legacy_cwd_dir = PathBuf::from("skills");
        if legacy_cwd_dir.is_dir() {
            let canonical = legacy_cwd_dir
                .canonicalize()
                .unwrap_or(legacy_cwd_dir.clone());
            tracing::info!(
                "Loading OS apps from legacy CWD skills/: {}",
                canonical.display()
            );
            return Self::from_dir(canonical);
        }

        tracing::warn!(
            "No os-apps directory found. Set TEMPER_OS_APPS_DIR (or legacy TEMPER_SKILLS_DIR)."
        );
        Self {
            apps_dir: PathBuf::new(),
            additional_dirs: Vec::new(),
            entries: Vec::new(),
            paths: BTreeMap::new(),
        }
    }

    fn from_dir(dir: PathBuf) -> Self {
        let mut entries = Vec::new();
        let mut paths = BTreeMap::new();

        let read_dir = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(e) => {
                tracing::warn!("Failed to read apps directory {}: {e}", dir.display());
                return Self {
                    apps_dir: dir,
                    additional_dirs: Vec::new(),
                    entries,
                    paths,
                };
            }
        };

        let mut app_dirs: Vec<_> = read_dir
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .collect();
        app_dirs.sort_by_key(|e| e.file_name());

        for entry in app_dirs {
            let app_dir = entry.path();
            let dir_name = entry.file_name().to_string_lossy().to_string();

            let manifest = match read_app_manifest(&app_dir) {
                Some(m) => m,
                None => {
                    tracing::warn!(
                        app = %dir_name,
                        path = %app_dir.display(),
                        "Skipping app directory — missing required app.toml"
                    );
                    continue;
                }
            };

            let app_guide = match read_app_guide(&app_dir) {
                Some(guide) => Some(guide),
                None => {
                    tracing::warn!(
                        app = %manifest.name,
                        path = %app_dir.display(),
                        "Skipping app — missing required APP.md"
                    );
                    continue;
                }
            };

            let app_name = manifest.name.clone();
            let entity_types: Vec<String> = temper_spec::loader::load_ioa_specs(&app_dir)
                .map(|specs| {
                    specs
                        .into_iter()
                        .map(|(entity_type, _source)| entity_type)
                        .collect()
                })
                .unwrap_or_default();

            let description = if !manifest.description.is_empty() {
                manifest.description.clone()
            } else {
                app_guide
                    .as_ref()
                    .and_then(|guide| extract_description(guide))
                    .unwrap_or_else(|| format!("App: {app_name}"))
            };

            let version = manifest.version.clone();
            let startup_install = manifest.startup_install;
            let dependencies = manifest.dependencies.clone();

            let app_path = app_dir.clone();
            paths.insert(app_name.clone(), app_path.clone());
            if dir_name != app_name {
                paths.entry(dir_name).or_insert(app_path);
            }
            entries.push(AppEntry {
                name: app_name,
                description,
                entity_types,
                version,
                startup_install,
                app_guide,
                dependencies,
            });
        }

        Self {
            apps_dir: dir,
            additional_dirs: Vec::new(),
            entries,
            paths,
        }
    }

    fn merge_dir(&mut self, dir: PathBuf) {
        let additional = AppCatalog::from_dir(dir);
        self.merge_catalog(additional);
    }

    fn merge_catalog(&mut self, additional: AppCatalog) {
        if additional.apps_dir.is_dir() && !self.additional_dirs.contains(&additional.apps_dir) {
            self.additional_dirs.push(additional.apps_dir.clone());
        }
        for (name, path) in additional.paths {
            self.paths.entry(name).or_insert(path);
        }
        for entry in additional.entries {
            if !self
                .entries
                .iter()
                .any(|existing| existing.name == entry.name)
            {
                self.entries.push(entry);
            }
        }
    }
}
