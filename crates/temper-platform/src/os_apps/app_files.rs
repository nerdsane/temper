//! The spec, CSDL, policy and compiled module files of an app, found one way
//! for its install and for `temper verify`, so what verify checks is what an
//! install runs.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::app_catalog::{add_os_apps_dir_preferred, catalog};
use super::file_source::{AppFileSource, join};
use super::types::WasmModuleManifest;
use super::{
    AppDeploymentMode, effective_app_deployment_mode, local_os_app_dependency_name,
    read_app_manifest, resolve_os_app_install_order,
};

/// The files of an app an install reads, and those it holds but does not read.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AppFiles {
    /// IOA specs: the app root first, then `specs/`.
    pub ioa: Vec<PathBuf>,
    /// The CSDL model: `model.csdl.xml` at the root, else in `specs/`, else
    /// the first `csdl/*.csdl.xml`.
    pub csdl: Option<PathBuf>,
    /// Cedar policies in `policies/`, and `policies/commons/` for an app that
    /// deploys as commons, in the order the install loads them.
    pub policies: Vec<PathBuf>,
    /// The compiled binary of each module `app.toml` declares, by module
    /// name, for each declared module whose binary is under `wasm/`.
    pub modules: BTreeMap<String, PathBuf>,
    /// Spec and CSDL files another file of the same kind shadows, so the
    /// install does not read them.
    pub shadowed: Vec<Shadowed>,
}

/// A spec or CSDL file the install does not read, and the one it reads in
/// its place.
#[derive(Debug, Clone, PartialEq)]
pub struct Shadowed {
    pub path: PathBuf,
    pub read_instead: PathBuf,
}

impl AppFiles {
    /// Find the files of the app at `app_dir`.
    pub fn find(app_dir: &Path) -> Self {
        Self::from_source(&AppFileSource::Listed(app_dir.to_path_buf()))
    }

    /// Select the files from an install's exact list, or a static app folder.
    pub(crate) fn from_source(source: &AppFileSource) -> Self {
        let app_dir = source.dir();
        let mut files = Self::default();

        // A root spec shadows a `specs/` spec of the same file name.
        let root_ioa = files_ending_in(source, "", ".ioa.toml");
        for path in files_ending_in(source, "specs", ".ioa.toml") {
            match root_ioa
                .iter()
                .find(|root| root.file_name() == path.file_name())
            {
                Some(root) => files.shadowed.push(Shadowed {
                    path,
                    read_instead: root.clone(),
                }),
                None => files.ioa.push(path),
            }
        }
        files.ioa.splice(0..0, root_ioa);

        let mut models = [
            app_dir.join("model.csdl.xml"),
            app_dir.join("specs/model.csdl.xml"),
        ]
        .into_iter()
        .filter(|path| source.has(&super::app_relative_path(app_dir, path)))
        .chain(files_ending_in(source, "csdl", ".csdl.xml"));
        files.csdl = models.next();
        if let Some(read) = &files.csdl {
            files.shadowed.extend(models.map(|path| Shadowed {
                path,
                read_instead: read.clone(),
            }));
        }

        files.policies = cedar_files_in(source, "policies");
        let manifest = read_app_manifest(app_dir);
        let commons = manifest.as_ref().is_some_and(|manifest| {
            effective_app_deployment_mode(manifest) == AppDeploymentMode::Commons
        });
        if commons {
            files
                .policies
                .extend(cedar_files_in(source, "policies/commons"));
        }

        files.modules = manifest
            .iter()
            .flat_map(|manifest| &manifest.wasm_modules)
            .filter_map(|module| {
                let binary = module_binary(source, module)?;
                Some((module.name.clone(), binary))
            })
            .collect();
        files
    }

    /// The compiled modules the specs of the app at `app_dir` can call once
    /// installed: those of every app it depends on, which its install
    /// installs first, then its own; a later app's module of a name replaces
    /// an earlier one's, as each install's registration does. The app's own
    /// directory and its neighbours resolve its dependencies ahead of the
    /// rest of the catalog.
    pub fn closure_modules(app_dir: &Path) -> Result<BTreeMap<String, PathBuf>, String> {
        let own = Self::find(app_dir).modules;
        let Some(manifest) = read_app_manifest(app_dir).filter(|m| !m.dependencies.is_empty())
        else {
            return Ok(own);
        };
        if let Some(neighbours) = app_dir.parent() {
            add_os_apps_dir_preferred(neighbours.to_path_buf());
        }
        let dependencies: Vec<String> = manifest
            .dependencies
            .iter()
            .filter_map(|dependency| local_os_app_dependency_name(dependency))
            .collect();
        let mut modules = BTreeMap::new();
        for dependency in resolve_os_app_install_order(&dependencies)?
            .iter()
            .filter(|app| **app != manifest.name)
        {
            let source = catalog()
                .read()
                .map_err(|_| "app catalog lock poisoned".to_string())?
                .app_files(None, dependency)
                .cloned()
                .ok_or_else(|| {
                    format!(
                        "app '{}' depends on '{dependency}', which is neither next to it nor in the app catalog",
                        manifest.name
                    )
                })?;
            modules.extend(Self::from_source(&source).modules);
        }
        modules.extend(own);
        Ok(modules)
    }

    /// One line for each spec or CSDL file under `app_dir` the install does
    /// not read: each shadowed one, named with the file read in its place,
    /// and each `.ioa.toml` outside the root and `specs/`.
    pub fn unread_warnings(&self, app_dir: &Path) -> Vec<String> {
        self.unread_warnings_from_source(&AppFileSource::Listed(app_dir.to_path_buf()))
    }

    pub(crate) fn unread_warnings_from_source(&self, source: &AppFileSource) -> Vec<String> {
        let app_dir = source.dir();
        let relative = |path: &Path| super::app_relative_path(app_dir, path);
        let mut warnings: Vec<String> = self
            .shadowed
            .iter()
            .map(|file| {
                format!(
                    "{} is not read: {} is read in its place",
                    relative(&file.path),
                    relative(&file.read_instead)
                )
            })
            .collect();
        let mut elsewhere = Vec::new();
        match source {
            AppFileSource::Listed(_) => collect_stray_ioa(app_dir, app_dir, &mut elsewhere),
            AppFileSource::Written { paths, .. } => {
                elsewhere.extend(
                    paths
                        .iter()
                        .filter(|path| {
                            let path = Path::new(path);
                            path.to_string_lossy().ends_with(".ioa.toml")
                                && path.parent().is_some_and(|parent| {
                                    parent != Path::new("") && parent != Path::new("specs")
                                })
                                && !path.components().any(|part| {
                                    matches!(
                                        part.as_os_str().to_str(),
                                        Some("target" | ".git" | "node_modules")
                                    )
                                })
                        })
                        .map(|path| app_dir.join(path)),
                );
            }
        }
        warnings.extend(elsewhere.iter().map(|path| {
            format!(
                "{} is not read: specs are read from the app root and specs/ only",
                relative(path)
            )
        }));
        warnings
    }
}

/// The app directory `dir` belongs to: its parent when `dir` is the `specs/`
/// of an app, since an install reads the whole app; otherwise `dir`.
pub fn app_dir_of(dir: &Path) -> &Path {
    match dir.parent() {
        Some(parent)
            if dir.file_name().is_some_and(|name| name == "specs")
                && parent.join("app.toml").is_file() =>
        {
            parent
        }
        _ => dir,
    }
}

/// The first binary of `module` that exists, looking where the app is
/// packaged before the build output a local checkout leaves: `wasm/<name>/`,
/// then `wasm/`, then the module's own `target/`, by its underscored and
/// dashed names.
pub(super) fn module_binary(
    source: &AppFileSource,
    module: &WasmModuleManifest,
) -> Option<PathBuf> {
    let wasm_dir = source.dir().join("wasm");
    let name = &module.name;
    let module_dir = wasm_dir.join(name);
    if !source.has_dir(&format!("wasm/{name}")) {
        return None;
    }
    let dashed = name.replace('_', "-");
    let targets: Vec<&str> = match module.target.as_deref() {
        Some(target) => vec![target],
        None => vec!["wasm32-unknown-unknown", "wasm32-wasip1"],
    };
    let mut candidates = Vec::new();
    for dir in [&module_dir, &wasm_dir] {
        for file in [name, &dashed] {
            candidates.push(dir.join(format!("{file}.wasm")));
        }
    }
    for file in [name, &dashed] {
        for target in &targets {
            candidates.push(
                module_dir
                    .join("target")
                    .join(target)
                    .join("release")
                    .join(format!("{file}.wasm")),
            );
        }
    }
    candidates
        .into_iter()
        .find(|path| source.has(&super::app_relative_path(source.dir(), path)))
}

/// Cedar policy files in `dir`, sorted: a `.cedar` extension in any case,
/// as `temper serve` loads them.
fn cedar_files_in(source: &AppFileSource, folder: &str) -> Vec<PathBuf> {
    source
        .files_in(folder)
        .into_iter()
        .filter(|name| {
            Path::new(name)
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("cedar"))
        })
        .map(|name| source.dir().join(join(folder, &name)))
        .collect()
}

/// Files in `folder` (not below it) whose name ends in `suffix`, sorted.
fn files_ending_in(source: &AppFileSource, folder: &str, suffix: &str) -> Vec<PathBuf> {
    source
        .files_in(folder)
        .into_iter()
        .filter(|name| name.ends_with(suffix))
        .map(|name| source.dir().join(join(folder, &name)))
        .collect()
}

/// `.ioa.toml` files below `app_dir` outside the root and `specs/`, skipping
/// build output and dependencies.
fn collect_stray_ioa(app_dir: &Path, dir: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.filter_map(|entry| entry.ok()).collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if !matches!(name.as_ref(), "target" | ".git" | "node_modules") {
                collect_stray_ioa(app_dir, &path, found);
            }
        } else if name.ends_with(".ioa.toml") && dir != app_dir && dir != app_dir.join("specs") {
            found.push(path);
        }
    }
}

#[cfg(test)]
#[path = "app_files_test.rs"]
mod tests;
