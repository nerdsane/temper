use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{OnceLock, RwLock};

use temper_spec::automaton;

use super::{
    AppEntry, StartupInstallMode, extract_description, find_ioa_files, read_app_guide,
    read_app_manifest,
};

pub(crate) struct AppCatalog {
    /// Directory containing app bundles.
    pub(super) apps_dir: PathBuf,
    /// Additional app directories merged into the catalog.
    additional_dirs: Vec<PathBuf>,
    /// Additional app directories whose bundles override base/additional entries.
    preferred_dirs: Vec<PathBuf>,
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

/// Add an app directory and prefer its bundles over existing catalog entries.
///
/// Genesis installs use this path because a pinned registry closure is the
/// source of truth for the requested install. Development/local app catalogs may
/// still be present for tests or helper tools, but they must not shadow the
/// pinned Genesis bundle when both expose the same app name.
pub fn add_os_apps_dir_preferred(dir: PathBuf) {
    let additional = AppCatalog::from_dir(dir);
    let cat = catalog();
    let mut lock = cat.write().unwrap(); // ci-ok: infallible lock
    lock.merge_catalog_preferred(additional);
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
    let preferred_dirs: Vec<PathBuf> = cat
        .preferred_dirs
        .iter()
        .filter(|dir| dir.is_dir())
        .cloned()
        .collect();
    drop(cat);
    let mut new = AppCatalog::from_dir(dir);
    for additional_dir in additional_dirs {
        new.merge_dir(additional_dir);
    }
    for preferred_dir in preferred_dirs {
        new.merge_dir_preferred(preferred_dir);
    }
    *catalog().write().unwrap() = new; // ci-ok: infallible lock
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
                let mut catalog = Self::from_dir(path);
                merge_genesis_apps_dir_if_present(&mut catalog);
                return catalog;
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
            let mut catalog = Self::from_dir(canonical);
            merge_genesis_apps_dir_if_present(&mut catalog);
            return catalog;
        }

        // Priority 3: ./os-apps/ relative to CWD.
        let cwd_dir = PathBuf::from("os-apps");
        if cwd_dir.is_dir() {
            let canonical = cwd_dir.canonicalize().unwrap_or(cwd_dir.clone());
            tracing::info!("Loading OS apps from CWD: {}", canonical.display());
            let mut catalog = Self::from_dir(canonical);
            merge_genesis_apps_dir_if_present(&mut catalog);
            return catalog;
        }

        tracing::warn!("No os-apps directory found. Set TEMPER_OS_APPS_DIR for dev/local apps.");
        let mut catalog = Self {
            apps_dir: PathBuf::new(),
            additional_dirs: Vec::new(),
            preferred_dirs: Vec::new(),
            entries: Vec::new(),
            paths: BTreeMap::new(),
        };
        merge_genesis_apps_dir_if_present(&mut catalog);
        catalog
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
                    preferred_dirs: Vec::new(),
                    entries,
                    paths,
                };
            }
        };

        let mut app_dirs = Vec::new();
        for entry in read_dir
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
        {
            let app_dir = entry.path();
            let dir_name = entry.file_name().to_string_lossy().to_string();
            if app_dir.join("app.toml").is_file() {
                app_dirs.push((dir_name, app_dir));
                continue;
            }

            let nested = match std::fs::read_dir(&app_dir) {
                Ok(nested) => nested,
                Err(_) => continue,
            };
            let mut found_nested_app = false;
            for nested_entry in nested.filter_map(|e| e.ok()).filter(|e| e.path().is_dir()) {
                let nested_dir = nested_entry.path();
                if !nested_dir.join("app.toml").is_file() {
                    continue;
                }
                found_nested_app = true;
                let nested_name = nested_entry.file_name().to_string_lossy().to_string();
                app_dirs.push((format!("{dir_name}/{nested_name}"), nested_dir));
            }
            if !found_nested_app {
                tracing::warn!(
                    app = %dir_name,
                    path = %app_dir.display(),
                    "Skipping app directory — missing required app.toml"
                );
            }
        }
        app_dirs.sort_by(|left, right| left.0.cmp(&right.0));

        for (dir_name, app_dir) in app_dirs {
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
            let ioa_files = find_ioa_files(&app_dir);
            let entity_types: Vec<String> = ioa_files
                .iter()
                .filter_map(|(_, ioa_path)| {
                    let source = std::fs::read_to_string(ioa_path).ok()?;
                    let parsed = automaton::parse_automaton(&source).ok()?;
                    Some(parsed.automaton.name)
                })
                .collect();

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
                paths.entry(dir_name.clone()).or_insert(app_path.clone());
                if let Some((_, leaf_name)) = dir_name.rsplit_once('/') {
                    paths.entry(leaf_name.to_string()).or_insert(app_path);
                }
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
            preferred_dirs: Vec::new(),
            entries,
            paths,
        }
    }

    fn merge_dir(&mut self, dir: PathBuf) {
        let additional = AppCatalog::from_dir(dir);
        self.merge_catalog(additional);
    }

    fn merge_dir_preferred(&mut self, dir: PathBuf) {
        let additional = AppCatalog::from_dir(dir);
        self.merge_catalog_preferred(additional);
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

    fn merge_catalog_preferred(&mut self, additional: AppCatalog) {
        if additional.apps_dir.is_dir() {
            self.additional_dirs
                .retain(|dir| dir != &additional.apps_dir);
            self.preferred_dirs
                .retain(|dir| dir != &additional.apps_dir);
            self.preferred_dirs.push(additional.apps_dir.clone());
        }
        for (name, path) in additional.paths {
            self.paths.insert(name, path);
        }
        for entry in additional.entries {
            if let Some(existing) = self
                .entries
                .iter_mut()
                .find(|existing| existing.name == entry.name)
            {
                *existing = entry;
            } else {
                self.entries.push(entry);
            }
        }
    }
}

fn merge_genesis_apps_dir_if_present(catalog: &mut AppCatalog) {
    if let Ok(dir) = std::env::var("TEMPER_GENESIS_APPS_DIR") {
        // determinism-ok: env var read at startup for configuration
        let path = PathBuf::from(dir);
        if path.is_dir() {
            tracing::info!(
                "Preferring Genesis app bundles from TEMPER_GENESIS_APPS_DIR: {}",
                path.display()
            );
            catalog.merge_catalog_preferred(AppCatalog::from_dir(path));
            return;
        }
    }

    let workspace_sibling = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("..")
        .join("apps");
    if workspace_sibling.is_dir() {
        let canonical = workspace_sibling
            .canonicalize()
            .unwrap_or(workspace_sibling.clone());
        tracing::info!(
            "Preferring Genesis workspace app bundles: {}",
            canonical.display()
        );
        catalog.merge_catalog_preferred(AppCatalog::from_dir(canonical));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_fixture_app(root: &std::path::Path, leaf: &str, version: &str) -> PathBuf {
        let app_dir = root.join(leaf);
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(
            app_dir.join("app.toml"),
            format!(
                r#"name = "duplicate-app"
description = "Duplicate app fixture"
version = "{version}"
"#
            ),
        )
        .unwrap();
        std::fs::write(
            app_dir.join("APP.md"),
            "# Duplicate App\n\nCatalog fixture.\n",
        )
        .unwrap();
        app_dir
    }

    #[test]
    fn reload_preserves_preferred_directory_precedence() {
        let root =
            std::env::temp_dir().join(format!("temper-preferred-catalog-{}", uuid::Uuid::new_v4()));
        let base = root.join("base");
        let preferred = root.join("preferred");
        let base_app = write_fixture_app(&base, "base-app", "0.1.0");
        let preferred_app = write_fixture_app(&preferred, "preferred-app", "0.2.0");

        let mut catalog = AppCatalog::from_dir(base.clone());
        catalog.merge_catalog_preferred(AppCatalog::from_dir(preferred.clone()));
        assert_eq!(
            catalog.paths.get("duplicate-app"),
            Some(&preferred_app),
            "preferred app should override before reload"
        );

        let additional_dirs = catalog.additional_dirs.clone();
        let preferred_dirs = catalog.preferred_dirs.clone();
        let mut reloaded = AppCatalog::from_dir(catalog.apps_dir.clone());
        for additional_dir in additional_dirs {
            reloaded.merge_dir(additional_dir);
        }
        for preferred_dir in preferred_dirs {
            reloaded.merge_dir_preferred(preferred_dir);
        }

        assert_eq!(
            reloaded.paths.get("duplicate-app"),
            Some(&preferred_app),
            "preferred app should still override after reload"
        );
        assert_ne!(
            reloaded.paths.get("duplicate-app"),
            Some(&base_app),
            "base duplicate must not shadow preferred app after reload"
        );

        let newer = root.join("newer");
        let newer_app = write_fixture_app(&newer, "newer-app", "0.3.0");
        catalog.merge_catalog_preferred(AppCatalog::from_dir(newer.clone()));
        assert_eq!(
            catalog.paths.get("duplicate-app"),
            Some(&newer_app),
            "newer preferred app should override before re-preferring older root"
        );

        catalog.merge_catalog_preferred(AppCatalog::from_dir(preferred.clone()));
        assert_eq!(
            catalog.paths.get("duplicate-app"),
            Some(&preferred_app),
            "re-preferred older app should override before reload"
        );

        let preferred_dirs = catalog.preferred_dirs.clone();
        let mut reloaded = AppCatalog::from_dir(catalog.apps_dir.clone());
        for preferred_dir in preferred_dirs {
            reloaded.merge_dir_preferred(preferred_dir);
        }
        assert_eq!(
            reloaded.paths.get("duplicate-app"),
            Some(&preferred_app),
            "re-preferred older app should keep recency after reload"
        );

        let _ = std::fs::remove_dir_all(root);
    }
}
