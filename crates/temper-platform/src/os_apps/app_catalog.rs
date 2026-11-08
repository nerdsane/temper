use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{OnceLock, RwLock, RwLockReadGuard};

use temper_spec::automaton;

use super::apps_dir::static_apps_dir;
use super::file_source::list;
use super::{
    AppBundle, AppEntry, AppFileSource, StartupInstallMode, extract_description, find_ioa_files,
    load_app_bundle, read_app_guide, read_app_manifest,
};

pub(crate) struct AppCatalog {
    /// Directory containing app bundles.
    apps_dir: PathBuf,
    /// What was merged into the catalog after its static folder, in order,
    /// so a reload merges the same again.
    added: Vec<Added>,
    /// Catalog entries (lightweight metadata).
    entries: Vec<AppEntry>,
    /// Mapping from app name to its folder and files.
    paths: BTreeMap<String, AppFileSource>,
    /// Apps installed from Genesis into one tenant other than the platform
    /// tenant, by tenant id.
    ///
    /// Such a tenant resolves its own installs first and the shared catalog
    /// above after them, so its installs change only that tenant: every other
    /// tenant, and every app in the shared catalog, resolves as before.
    tenant_installs: BTreeMap<String, TenantInstalls>,
}

/// Apps merged into the catalog after its static folder.
#[derive(Clone)]
enum Added {
    /// Every app of a folder, found by listing it ([`add_os_apps_dir`]).
    Folder(PathBuf),
    /// Static checkout apps preferred by `temper verify`, including on reload.
    PreferredFolder(PathBuf),
    /// Apps of an install's directory, each with the files the install
    /// wrote; they win over apps of the same name merged before them.
    Written(PathBuf, Vec<(String, AppFileSource)>),
}

/// `earlier` apps of a directory added again with `apps`: the earlier ones
/// this adds no files for, then these.
fn merged(
    mut earlier: Vec<(String, AppFileSource)>,
    apps: &[(String, AppFileSource)],
) -> Vec<(String, AppFileSource)> {
    earlier.retain(|(name, _)| !apps.iter().any(|(added, _)| added == name));
    earlier.extend(apps.iter().cloned());
    earlier
}

/// The apps one tenant installed; a later install of a name replaces an earlier one.
#[derive(Default)]
struct TenantInstalls {
    /// Install directories in the order they were added, with the apps of each.
    written: Vec<(PathBuf, Vec<(String, AppFileSource)>)>,
    entries: BTreeMap<String, AppEntry>,
    paths: BTreeMap<String, AppFileSource>,
}

impl TenantInstalls {
    /// Add `installed`, whose apps are `apps`, over what this tenant has. A
    /// directory added again moves last.
    fn add(&mut self, installed: AppCatalog, apps: &[(String, AppFileSource)]) {
        let dir = installed.apps_dir;
        let earlier = match self.written.iter().position(|(known, _)| known == &dir) {
            Some(index) => self.written.remove(index).1,
            None => Vec::new(),
        };
        self.written.push((dir, merged(earlier, apps)));
        self.paths.extend(installed.paths);
        for entry in installed.entries {
            self.entries.insert(entry.name.clone(), entry);
        }
    }
}

/// Global catalog, initialized on first access.
static CATALOG: OnceLock<RwLock<AppCatalog>> = OnceLock::new();

/// Get or initialize the global app catalog.
pub fn catalog() -> &'static RwLock<AppCatalog> {
    CATALOG.get_or_init(|| RwLock::new(AppCatalog::discover()))
}

fn read_catalog() -> RwLockReadGuard<'static, AppCatalog> {
    catalog().read().unwrap() // ci-ok: infallible lock
}

/// List the apps of the shared catalog.
pub fn list_os_apps() -> Vec<AppEntry> {
    read_catalog().app_entries(None)
}

/// List the apps `tenant` resolves: its own installs and the shared catalog.
pub fn list_tenant_os_apps(tenant: &str) -> Vec<AppEntry> {
    read_catalog().app_entries(Some(tenant))
}

/// Get the full spec bundle for a shared-catalog app by name.
///
/// Reads IOA, CSDL, and Cedar files from disk on each call so changes
/// are picked up without a rebuild.
pub fn get_os_app(name: &str) -> Option<AppBundle> {
    load_app_bundle(read_catalog().app_files(None, name)?).ok()
}

/// Get the full spec bundle `tenant` resolves for `name`: its own install
/// when it has one, otherwise the shared catalog's.
pub fn tenant_os_app(tenant: &str, name: &str) -> Option<AppBundle> {
    load_tenant_os_app(tenant, name).ok()
}

/// [`tenant_os_app`], saying why not: the app is not in the catalog, or a
/// file of it could not be read.
pub(crate) fn load_tenant_os_app(tenant: &str, name: &str) -> Result<AppBundle, String> {
    let files = read_catalog()
        .app_files(Some(tenant), name)
        .cloned()
        .ok_or_else(|| format!("OS app '{name}' not found"))?;
    load_app_bundle(&files).map_err(|why| {
        format!(
            "OS app '{name}' at '{}' did not load: {why}",
            files.dir().display()
        )
    })
}

/// The directory `tenant` resolves `name` from, as [`tenant_os_app`] reads it.
pub(crate) fn tenant_os_app_dir(tenant: &str, name: &str) -> Option<PathBuf> {
    read_catalog()
        .app_files(Some(tenant), name)
        .map(|files| files.dir().to_path_buf())
}

/// Get the full app guide markdown `tenant` resolves for `name`.
pub fn tenant_app_guide(tenant: &str, name: &str) -> Option<String> {
    read_catalog()
        .app_entry(Some(tenant), name)
        .and_then(|entry| entry.app_guide.clone())
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
    let mut lock = catalog().write().unwrap(); // ci-ok: infallible lock
    lock.merge_catalog(AppCatalog::from_dir(dir.clone()));
    lock.added.push(Added::Folder(dir));
}

/// Prefer a local checkout and its neighbours while verifying their modules.
pub(super) fn add_os_apps_dir_preferred(dir: PathBuf) {
    let mut lock = catalog().write().unwrap(); // ci-ok: infallible lock
    lock.merge_catalog_preferred(AppCatalog::from_dir(dir.clone()));
    lock.added
        .retain(|added| !matches!(added, Added::PreferredFolder(known) if known == &dir));
    lock.added.push(Added::PreferredFolder(dir));
}

/// Add the apps an install wrote into `dir`, each its folder name and the
/// files the install named, and prefer them over existing catalog entries.
///
/// Every tenant resolves these apps. Genesis installs into the platform
/// tenant use this path because a pinned registry closure is the source of
/// truth for the requested install: development/local app catalogs must not
/// shadow the pinned Genesis bundle when both expose the same app name.
pub fn add_written_apps_preferred(dir: PathBuf, apps: Vec<(String, AppFileSource)>) {
    let mut lock = catalog().write().unwrap(); // ci-ok: infallible lock
    lock.merge_catalog_preferred(AppCatalog::from_apps(dir.clone(), apps.clone()));
    let mut earlier = Vec::new();
    lock.added.retain(|added| match added {
        Added::Written(known, apps) if known == &dir => {
            earlier = apps.clone();
            false
        }
        _ => true,
    });
    lock.added.push(Added::Written(dir, merged(earlier, &apps)));
}

/// Make the apps an install wrote into `dir` resolve for `tenant` only, ahead
/// of the shared catalog.
///
/// Genesis installs into a tenant other than the platform tenant use this: a
/// pinned closure is what that tenant asked to run, so for that tenant it wins
/// over a shared app of the same name, and no other tenant sees it.
pub fn add_tenant_written_apps(tenant: &str, dir: PathBuf, apps: Vec<(String, AppFileSource)>) {
    let installed = AppCatalog::from_apps(dir, apps.clone());
    let mut lock = catalog().write().unwrap(); // ci-ok: infallible lock
    lock.tenant_installs
        .entry(tenant.to_string())
        .or_default()
        .add(installed, &apps);
}

/// Re-scan the OS apps directory and refresh the catalog.
///
/// Call this after modifying app files on disk to pick up changes
/// without restarting the server. Folders added whole are listed again; an
/// install's apps are read again from the files it named.
pub fn reload_os_apps() {
    let cat = catalog().read().unwrap(); // ci-ok: infallible lock
    let dir = cat.apps_dir.clone();
    let added = cat.added.clone();
    let tenant_written: Vec<_> = cat
        .tenant_installs
        .iter()
        .map(|(tenant, installs)| (tenant.clone(), installs.written.clone()))
        .collect();
    drop(cat);
    let mut new = AppCatalog::from_dir(dir);
    for added in &added {
        match added {
            Added::Folder(dir) => new.merge_catalog(AppCatalog::from_dir(dir.clone())),
            Added::PreferredFolder(dir) => {
                new.merge_catalog_preferred(AppCatalog::from_dir(dir.clone()))
            }
            Added::Written(dir, apps) => {
                new.merge_catalog_preferred(AppCatalog::from_apps(dir.clone(), apps.clone()));
            }
        }
    }
    new.added = added;
    for (tenant, written) in tenant_written {
        let installs = new.tenant_installs.entry(tenant).or_default();
        for (dir, apps) in written {
            installs.add(AppCatalog::from_apps(dir, apps.clone()), &apps);
        }
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
    /// The files `name` resolves to for `tenant`: that tenant's own install
    /// when it has one, otherwise the shared catalog's. `None` looks at the
    /// shared catalog only.
    pub(super) fn app_files(&self, tenant: Option<&str>, name: &str) -> Option<&AppFileSource> {
        self.installs(tenant)
            .and_then(|installs| installs.paths.get(name))
            .or_else(|| self.paths.get(name))
    }

    /// The catalog entry `name` resolves to for `tenant`, as [`Self::app_files`].
    pub(super) fn app_entry(&self, tenant: Option<&str>, name: &str) -> Option<&AppEntry> {
        self.installs(tenant)
            .and_then(|installs| installs.entries.get(name))
            .or_else(|| self.entries.iter().find(|entry| entry.name == name))
    }

    /// Every app `tenant` resolves: its own installs, then the shared apps it
    /// has not replaced.
    pub(super) fn app_entries(&self, tenant: Option<&str>) -> Vec<AppEntry> {
        let installs = self.installs(tenant);
        let mut entries: Vec<AppEntry> = installs
            .map(|installs| installs.entries.values().cloned().collect())
            .unwrap_or_default();
        entries.extend(
            self.entries
                .iter()
                .filter(|entry| installs.is_none_or(|i| !i.entries.contains_key(&entry.name)))
                .cloned(),
        );
        entries
    }

    fn installs(&self, tenant: Option<&str>) -> Option<&TenantInstalls> {
        tenant.and_then(|tenant| self.tenant_installs.get(tenant))
    }

    /// Discover the static apps folder and scan it.
    fn discover() -> Self {
        match static_apps_dir() {
            Some(dir) => Self::from_dir(dir),
            None => Self::from_apps(PathBuf::new(), Vec::new()),
        }
    }

    /// The apps of `dir`, found by listing it: only for a static apps folder,
    /// which no install wrote and which has no list of its files.
    pub(super) fn from_dir(dir: PathBuf) -> Self {
        if !dir.is_dir() {
            tracing::warn!("Failed to read apps directory {}", dir.display());
        }
        let mut folders: Vec<String> = list(&dir)
            .into_iter()
            .filter(|(_, is_dir)| *is_dir)
            .map(|(name, _)| name)
            .collect();
        folders.sort();
        let apps = folders
            .into_iter()
            .map(|name| {
                let files = AppFileSource::Listed(dir.join(&name));
                (name, files)
            })
            .collect();
        Self::from_apps(dir, apps)
    }

    /// The apps of `dir`, each its folder name and its files.
    pub(super) fn from_apps(dir: PathBuf, apps: Vec<(String, AppFileSource)>) -> Self {
        let mut entries = Vec::new();
        let mut paths = BTreeMap::new();

        for (dir_name, files) in apps {
            let app_dir = files.dir();
            let manifest = match read_app_manifest(app_dir) {
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

            let app_guide = match read_app_guide(app_dir) {
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
            let entity_types: Vec<String> = find_ioa_files(&files)
                .iter()
                .filter_map(|path| {
                    let source = files.read_text(path).ok()?;
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

            paths.insert(app_name.clone(), files.clone());
            if dir_name != app_name {
                paths.entry(dir_name).or_insert(files);
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
            added: Vec::new(),
            entries,
            paths,
            tenant_installs: BTreeMap::new(),
        }
    }

    fn merge_catalog(&mut self, additional: AppCatalog) {
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
