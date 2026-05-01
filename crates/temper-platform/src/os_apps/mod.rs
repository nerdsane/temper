//! OS App Catalog — agent-installable pre-built application specs.
//!
//! OS apps are spec bundles (IOA TOML + CSDL + Cedar policies) loaded from
//! the `os-apps/` directory at runtime. Agents discover them via
//! `list_os_apps()` / `install_os_app()`.
//!
//! Backward-compatible skill aliases are preserved (`list_skills()`,
//! `install_skill()`) to avoid breaking older callers.
//!
//! Install reuses [`crate::bootstrap::bootstrap_tenant_specs`] so every app goes through the same verification cascade as system specs.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

use serde::Serialize;
use sha2::{Digest, Sha256};
use temper_runtime::tenant::TenantId;
use temper_spec::automaton;
use temper_spec::csdl::{emit_csdl_xml, merge_csdl, parse_csdl};

use crate::bootstrap;
use crate::state::PlatformState;

mod agent_bootstrap;
pub mod git_sources;
mod reconcile;
mod runtime_heal;
mod system_files;
mod types;
pub use reconcile::{os_app_bundle_digest, reconcile_os_app, resolve_os_app_install_order};
pub(crate) use runtime_heal::{
    restore_app_specs_from_matching_digest, tenant_has_ready_app_specs_for_bundle,
};
pub use types::*;

fn read_app_manifest(app_dir: &Path) -> Option<AppManifest> {
    let path = app_dir.join("app.toml");
    let content = std::fs::read_to_string(&path).ok()?;
    toml::from_str(&content).ok()
}

// ── Agent / Skill / Seed Data types ─────────────────────────────────

/// An agent definition discovered in the app's `agents/{name}/` directory.
///
/// All `.md` files in the directory are concatenated alphabetically.
/// The platform is filename-agnostic — conventions like SOUL.md, STYLE.md,
/// AGENT.md are for humans, not the platform.
#[derive(Debug, Clone, Serialize)]
pub struct AgentDefinition {
    /// Agent name (from directory name).
    pub name: String,
    /// Concatenated content of all `.md` files, sorted alphabetically.
    pub content: String,
    /// Whether a `SOUL.md` file was present (indicates personality overlay).
    pub has_soul: bool,
    /// Description extracted from the first non-header paragraph.
    pub description: String,
}

/// A skill definition discovered in the app's directory tree.
///
/// Skills are scoped by their location:
/// - `system/skills/{name}/` → system-level (all agents)
/// - `agents/{agent}/skills/{name}/` → agent-scoped
///
/// Each skill directory must contain a `SKILL.md` file. Other files in the
/// directory are companion files (examples, references, scripts) that get
/// uploaded to TemperFS alongside the main skill document.
#[derive(Debug, Clone, Serialize)]
pub struct AppSkillDefinition {
    /// Skill name (from directory name).
    pub name: String,
    /// Main skill document content (from `SKILL.md`).
    pub content: String,
    /// Description extracted from the skill document.
    pub description: String,
    /// Which agent this skill belongs to (None = system skill).
    pub agent_name: Option<String>,
    /// Companion files in the skill directory (everything except SKILL.md).
    #[serde(skip)]
    pub companion_files: Vec<CompanionFile>,
}

pub use system_files::SystemFileEntry;

/// An architecture decision record discovered from `adrs/*.md`.
#[derive(Debug, Clone, Serialize)]
pub struct AdrEntry {
    /// Filename stem, e.g. `001-initial-design`.
    pub name: String,
    /// File name including extension, e.g. `001-initial-design.md`.
    pub file_name: String,
    /// Full markdown content.
    pub content: String,
}

/// A companion file bundled with a skill.
#[derive(Debug, Clone)]
pub struct CompanionFile {
    /// Relative path within the skill directory.
    pub name: String,
    /// File content bytes.
    pub content: Vec<u8>,
    /// MIME type (inferred from extension).
    pub mime_type: String,
}

/// A seed data instance to create on first install.
///
/// Parsed from `seed-data/*.toml` files using `[[instance]]` blocks.
#[derive(Debug, Clone, serde::Deserialize, Serialize)]
pub struct SeedInstance {
    /// Entity type name (must be a registered type).
    #[serde(rename = "type")]
    pub entity_type: String,
    /// Optional explicit entity ID.
    #[serde(default)]
    pub id: Option<String>,
    /// Fields to set on the entity.
    #[serde(default)]
    pub fields: serde_json::Value,
    /// Actions to dispatch after creation, in order.
    #[serde(default)]
    pub actions: Vec<SeedAction>,
}

/// An action to dispatch on a seed entity after creation.
#[derive(Debug, Clone, serde::Deserialize, Serialize)]
pub struct SeedAction {
    /// Action name (e.g. "Activate", "Register").
    pub name: String,
    /// Action parameters.
    #[serde(default)]
    pub params: serde_json::Value,
}

/// Container for parsing `seed-data/*.toml` files.
#[derive(Debug, serde::Deserialize)]
struct SeedFile {
    #[serde(rename = "instance", default)]
    instances: Vec<SeedInstance>,
}

// Backward-compatible alias: SkillBundle → AppBundle.
pub type SkillBundle = AppBundle;

// Backward-compatible type aliases.
pub type OsAppEntry = AppEntry;
pub type OsAppBundle = AppBundle;

// ── App Catalog (disk-loaded, cached) ───────────────────────────────

/// In-memory cache of discovered apps.
struct AppCatalog {
    /// Directory containing app bundles.
    apps_dir: PathBuf,
    /// Catalog entries (lightweight metadata).
    entries: Vec<AppEntry>,
    /// Mapping from app name to its directory path on disk.
    paths: BTreeMap<String, PathBuf>,
}

/// Global catalog, initialized on first access.
static CATALOG: OnceLock<RwLock<AppCatalog>> = OnceLock::new();

/// Get or initialize the global app catalog.
fn catalog() -> &'static RwLock<AppCatalog> {
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
    for (name, path) in additional.paths {
        lock.paths.entry(name.clone()).or_insert(path);
    }
    for entry in additional.entries {
        if !lock.entries.iter().any(|e| e.name == entry.name) {
            lock.entries.push(entry);
        }
    }
}

/// Re-scan the OS apps directory and refresh the catalog.
///
/// Call this after modifying app files on disk to pick up changes
/// without restarting the server.
pub fn reload_os_apps() {
    let cat = catalog().read().unwrap(); // ci-ok: infallible lock
    let dir = cat.apps_dir.clone();
    drop(cat);
    let new = AppCatalog::from_dir(dir);
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
            entries: Vec::new(),
            paths: BTreeMap::new(),
        }
    }

    /// Build catalog from a specific directory.
    fn from_dir(dir: PathBuf) -> Self {
        let mut entries = Vec::new();
        let mut paths = BTreeMap::new();

        let read_dir = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(e) => {
                tracing::warn!("Failed to read apps directory {}: {e}", dir.display());
                return Self {
                    apps_dir: dir,
                    entries,
                    paths,
                };
            }
        };

        let mut app_dirs: Vec<_> = read_dir
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .collect();
        // Deterministic ordering.
        app_dirs.sort_by_key(|e| e.file_name());

        for entry in app_dirs {
            let app_dir = entry.path();
            let dir_name = entry.file_name().to_string_lossy().to_string();

            // app.toml is required — skip directories without it.
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

            // APP.md is required — skip directories without it.
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

            // Scan for IOA specs to determine entity types.
            let ioa_files = find_ioa_files(&app_dir);
            let entity_types: Vec<String> = ioa_files
                .iter()
                .filter_map(|(_, ioa_path)| {
                    let source = std::fs::read_to_string(ioa_path).ok()?;
                    let parsed = automaton::parse_automaton(&source).ok()?;
                    Some(parsed.automaton.name)
                })
                .collect();

            // Description from manifest (required), fallback to APP.md extract.
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
            entries,
            paths,
        }
    }
}

/// Find all IOA spec files in an app directory.
///
/// Handles both layouts:
/// - Root-level: `app-name/*.ioa.toml` + `app-name/model.csdl.xml`
/// - Specs subdir: `app-name/specs/*.ioa.toml` + `app-name/specs/model.csdl.xml`
///
/// Returns `(entity_type_hint, path)` pairs. The entity type is extracted
/// from the IOA file's `[automaton] name` field, not the filename.
fn find_ioa_files(app_dir: &Path) -> Vec<(String, PathBuf)> {
    let mut results = Vec::new();
    let mut seen_names = std::collections::HashSet::new();

    // Scan root first (takes priority for dedup).
    scan_dir_for_ioa(app_dir, &mut results, &mut seen_names);

    // Then scan specs/ subdirectory.
    let specs_dir = app_dir.join("specs");
    if specs_dir.is_dir() {
        scan_dir_for_ioa(&specs_dir, &mut results, &mut seen_names);
    }

    results
}

/// Scan a single directory for `*.ioa.toml` files.
fn scan_dir_for_ioa(
    dir: &Path,
    results: &mut Vec<(String, PathBuf)>,
    seen: &mut std::collections::HashSet<String>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<_> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".ioa.toml"))
        .collect();
    files.sort_by_key(|e| e.file_name());

    for entry in files {
        let path = entry.path();
        let fname = entry.file_name().to_string_lossy().to_string();
        // Use filename as dedup key.
        if !seen.insert(fname) {
            continue;
        }
        results.push((String::new(), path));
    }
}

/// Find the CSDL model file in an app directory.
fn find_csdl(app_dir: &Path) -> Option<PathBuf> {
    // Root-level first.
    let root = app_dir.join("model.csdl.xml");
    if root.exists() {
        return Some(root);
    }
    // Then specs/.
    let specs = app_dir.join("specs").join("model.csdl.xml");
    if specs.exists() {
        return Some(specs);
    }
    // Then a dedicated csdl/ directory.
    let csdl_dir = app_dir.join("csdl");
    if csdl_dir.is_dir() {
        let Ok(entries) = std::fs::read_dir(&csdl_dir) else {
            return None;
        };
        let mut files: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".csdl.xml"))
            .map(|e| e.path())
            .collect();
        files.sort();
        if let Some(first) = files.into_iter().next() {
            return Some(first);
        }
    }
    None
}

/// Find all Cedar policy files in an app directory.
fn find_cedar_policies(app_dir: &Path) -> Vec<PathBuf> {
    let policies_dir = app_dir.join("policies");
    if !policies_dir.is_dir() {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(&policies_dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".cedar"))
        .map(|e| e.path())
        .collect();
    files.sort();
    files
}

/// Find compiled WASM module binaries in an app directory.
///
/// Scans both `wasm32-unknown-unknown` and `wasm32-wasip1` release outputs
/// because some OS apps mix pure WASM modules with WASI modules such as
/// sandboxed tool runners.
fn find_wasm_modules(
    app_dir: &Path,
    module_configs: &BTreeMap<String, WasmModuleManifest>,
) -> BTreeMap<String, Vec<u8>> {
    let mut modules = BTreeMap::new();
    let wasm_dir = app_dir.join("wasm");
    if !wasm_dir.is_dir() {
        return modules;
    }
    let Ok(entries) = std::fs::read_dir(&wasm_dir) else {
        return modules;
    };
    let mut dirs: Vec<_> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .collect();
    dirs.sort_by_key(|e| e.file_name());

    for entry in dirs {
        let module_name = entry.file_name().to_string_lossy().to_string();
        // Skip target directories that cargo creates.
        if module_name == "target" {
            continue;
        }

        // When the manifest declares a specific compilation target, search
        // only that target's release directory — avoids picking up a stale
        // build from the wrong target (e.g. wasm32-unknown-unknown when the
        // module requires wasm32-wasip1). Fall back to a sibling bundled
        // artifact ({module_name}.wasm) which build.sh copies after compilation.
        let candidates: Vec<PathBuf> = if let Some(config) = module_configs.get(&module_name)
            && let Some(ref target) = config.target
        {
            vec![
                entry
                    .path()
                    .join("target")
                    .join(target)
                    .join("release")
                    .join(format!("{module_name}.wasm")),
                entry.path().join(format!("{module_name}.wasm")),
            ]
        } else {
            vec![
                entry
                    .path()
                    .join("target")
                    .join("wasm32-unknown-unknown")
                    .join("release")
                    .join(format!("{module_name}.wasm")),
                entry
                    .path()
                    .join("target")
                    .join("wasm32-wasip1")
                    .join("release")
                    .join(format!("{module_name}.wasm")),
                entry.path().join(format!("{module_name}.wasm")),
            ]
        };

        for wasm_path in candidates {
            if !wasm_path.exists() {
                continue;
            }
            match std::fs::read(&wasm_path) {
                Ok(bytes) => {
                    tracing::debug!(
                        module = %module_name,
                        path = %wasm_path.display(),
                        size = bytes.len(),
                        "Found WASM module in OS app"
                    );
                    modules.insert(module_name.clone(), bytes);
                    break;
                }
                Err(e) => {
                    tracing::warn!(
                        module = %module_name,
                        path = %wasm_path.display(),
                        error = %e,
                        "Failed to read WASM module binary"
                    );
                }
            }
        }
    }
    modules
}

// ── Agent / Skill / Seed Data discovery ─────────────────────────────

/// Discover agent definitions from `agents/{name}/` subdirectories.
///
/// Each subdirectory is one agent. All `.md` files within it are collected,
/// sorted alphabetically, and concatenated. The platform is filename-agnostic —
/// conventions like SOUL.md, STYLE.md, AGENT.md are for humans.
fn find_agents(app_dir: &Path) -> Vec<AgentDefinition> {
    let agents_dir = app_dir.join("agents");
    if !agents_dir.is_dir() {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(&agents_dir) else {
        return Vec::new();
    };

    let mut dirs: Vec<_> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .collect();
    dirs.sort_by_key(|e| e.file_name());

    let mut results = Vec::new();
    for dir_entry in dirs {
        let agent_name = dir_entry.file_name().to_string_lossy().to_string();
        let agent_dir = dir_entry.path();

        // Collect all .md files, sorted alphabetically.
        let mut md_files: Vec<PathBuf> = std::fs::read_dir(&agent_dir)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .to_lowercase()
                    .ends_with(".md")
            })
            .map(|e| e.path())
            .collect();
        md_files.sort();

        if md_files.is_empty() {
            continue;
        }

        let has_soul = md_files
            .iter()
            .any(|p| p.file_name().map(|f| f == "SOUL.md").unwrap_or(false));

        let mut content = String::new();
        for path in &md_files {
            if !content.is_empty() {
                content.push_str("\n\n");
            }
            if let Ok(text) = std::fs::read_to_string(path) {
                content.push_str(&text);
            }
        }

        let description =
            extract_description(&content).unwrap_or_else(|| format!("Agent: {agent_name}"));

        results.push(AgentDefinition {
            name: agent_name,
            content,
            has_soul,
            description,
        });
    }
    results
}

/// Discover skill definitions from the app's directory tree.
///
/// Scans two locations:
/// - `system/skills/{name}/` → system-level skills (agent_name = None)
/// - `agents/{agent}/skills/{name}/` → agent-scoped skills (agent_name = Some)
///
/// Each subdirectory must contain a `SKILL.md` file as the main document.
/// All other files are collected as companion files.
fn find_app_skills(app_dir: &Path) -> Vec<AppSkillDefinition> {
    let mut results = Vec::new();

    // 1. System skills: system/skills/{name}/
    let system_skills_dir = app_dir.join("system").join("skills");
    if system_skills_dir.is_dir() {
        scan_skill_dirs(&system_skills_dir, None, &mut results);
    }

    // 2. Agent-scoped skills: agents/{agent}/skills/{name}/
    let agents_dir = app_dir.join("agents");
    if agents_dir.is_dir()
        && let Ok(entries) = std::fs::read_dir(&agents_dir)
    {
        let mut dirs: Vec<_> = entries
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .collect();
        dirs.sort_by_key(|e| e.file_name());

        for agent_entry in dirs {
            let agent_name = agent_entry.file_name().to_string_lossy().to_string();
            let agent_skills_dir = agent_entry.path().join("skills");
            if agent_skills_dir.is_dir() {
                scan_skill_dirs(&agent_skills_dir, Some(&agent_name), &mut results);
            }
        }
    }

    results
}

/// Scan a directory of skill subdirectories and collect definitions.
fn scan_skill_dirs(
    skills_dir: &Path,
    agent_name: Option<&str>,
    results: &mut Vec<AppSkillDefinition>,
) {
    let Ok(entries) = std::fs::read_dir(skills_dir) else {
        return;
    };

    let mut dirs: Vec<_> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .collect();
    dirs.sort_by_key(|e| e.file_name());

    for dir_entry in dirs {
        let skill_name = dir_entry.file_name().to_string_lossy().to_string();
        let skill_dir = dir_entry.path();

        // Main skill document.
        let skill_path = skill_dir.join("SKILL.md");
        let content = match std::fs::read_to_string(&skill_path) {
            Ok(c) => c,
            Err(_) => continue, // Skip directories without SKILL.md
        };

        // Extract frontmatter (YAML or TOML).
        let frontmatter = extract_frontmatter(&content);

        let description = frontmatter
            .description
            .or_else(|| extract_description(&content))
            .unwrap_or_else(|| format!("Skill: {skill_name}"));

        // Collect companion files (everything except SKILL.md).
        let companion_files = collect_companion_files(&skill_dir);

        results.push(AppSkillDefinition {
            name: skill_name,
            content,
            description,
            agent_name: agent_name.map(String::from),
            companion_files,
        });
    }
}

/// Discover app-local ADR markdown files from `adrs/*.md`.
fn find_adrs(app_dir: &Path) -> Vec<AdrEntry> {
    let adrs_dir = app_dir.join("adrs");
    if !adrs_dir.is_dir() {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(&adrs_dir) else {
        return Vec::new();
    };

    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|path| {
            path.is_file()
                && path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
        })
        .collect();
    files.sort();

    let mut results = Vec::new();
    for path in files {
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let name = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or(file_name)
            .to_string();
        results.push(AdrEntry {
            name,
            file_name: file_name.to_string(),
            content,
        });
    }
    results
}

/// Parsed frontmatter from a SKILL.md file.
struct SkillFrontmatter {
    #[allow(dead_code)]
    name: Option<String>,
    description: Option<String>,
}

/// Extract frontmatter from SKILL.md content. Supports YAML (`---`) and TOML (`+++`).
fn extract_frontmatter(content: &str) -> SkillFrontmatter {
    // Try YAML frontmatter (---)
    if let Some(rest) = content.strip_prefix("---")
        && let Some(end) = rest.find("\n---")
    {
        let fm = &rest[..end];
        let mut name = None;
        let mut description = None;
        for line in fm.lines() {
            let trimmed = line.trim();
            if let Some(val) = trimmed.strip_prefix("name:") {
                let val = val.trim().trim_matches('"').trim_matches('\'');
                if !val.is_empty() {
                    name = Some(val.to_string());
                }
            } else if let Some(val) = trimmed.strip_prefix("description:") {
                let val = val.trim().trim_matches('"').trim_matches('\'');
                if !val.is_empty() {
                    description = Some(val.to_string());
                }
            }
        }
        return SkillFrontmatter { name, description };
    }

    // Fall back to TOML frontmatter (+++)
    if let Some(rest) = content.strip_prefix("+++")
        && let Some(end) = rest.find("+++")
    {
        let fm = &rest[..end];
        let mut name = None;
        let mut description = None;
        for line in fm.lines() {
            let trimmed = line.trim();
            if let Some((key, val)) = trimmed.split_once('=') {
                let key = key.trim();
                let val = val.trim().trim_matches('"');
                if val.is_empty() {
                    continue;
                }
                match key {
                    "name" => name = Some(val.to_string()),
                    "description" => description = Some(val.to_string()),
                    _ => {}
                }
            }
        }
        return SkillFrontmatter { name, description };
    }

    SkillFrontmatter {
        name: None,
        description: None,
    }
}

/// Recursively collect companion files from a skill directory (excluding SKILL.md).
fn collect_companion_files(skill_dir: &Path) -> Vec<CompanionFile> {
    let mut files = Vec::new();
    collect_companions_recursive(skill_dir, skill_dir, &mut files);
    files
}

fn collect_companions_recursive(
    base_dir: &Path,
    current_dir: &Path,
    results: &mut Vec<CompanionFile>,
) {
    let Ok(entries) = std::fs::read_dir(current_dir) else {
        return;
    };
    let mut sorted: Vec<_> = entries.filter_map(|e| e.ok()).collect();
    sorted.sort_by_key(|e| e.file_name());

    for entry in sorted {
        let path = entry.path();
        if path.is_dir() {
            collect_companions_recursive(base_dir, &path, results);
        } else if path.file_name().map(|f| f != "SKILL.md").unwrap_or(true)
            && let Ok(content) = std::fs::read(&path)
        {
            let rel_path = path
                .strip_prefix(base_dir)
                .unwrap_or(&path)
                .to_string_lossy()
                .to_string();
            let mime_type = mime_from_extension(&path);
            results.push(CompanionFile {
                name: rel_path,
                content,
                mime_type,
            });
        }
    }
}

/// Infer MIME type from file extension.
fn mime_from_extension(path: &Path) -> String {
    match path.extension().and_then(|e| e.to_str()) {
        Some("md") => "text/markdown".to_string(),
        Some("txt") => "text/plain".to_string(),
        Some("json") => "application/json".to_string(),
        Some("toml") => "application/toml".to_string(),
        Some("yaml" | "yml") => "application/yaml".to_string(),
        Some("sh") => "application/x-sh".to_string(),
        Some("py") => "text/x-python".to_string(),
        Some("ts" | "js") => "text/javascript".to_string(),
        _ => "application/octet-stream".to_string(),
    }
}

/// Discover seed data instances from `seed-data/*.toml` files.
///
/// Each TOML file contains `[[instance]]` blocks that declare entities
/// to create on first install.
fn find_seed_data(app_dir: &Path) -> Vec<SeedInstance> {
    let seed_dir = app_dir.join("seed-data");
    if !seed_dir.is_dir() {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(&seed_dir) else {
        return Vec::new();
    };

    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".toml"))
        .map(|e| e.path())
        .collect();
    files.sort();

    let mut all_instances = Vec::new();
    for path in &files {
        match std::fs::read_to_string(path) {
            Ok(content) => match toml::from_str::<SeedFile>(&content) {
                Ok(seed_file) => all_instances.extend(seed_file.instances),
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "Failed to parse seed data file"
                    );
                }
            },
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "Failed to read seed data file"
                );
            }
        }
    }
    all_instances
}

/// Read the app guide markdown (APP.md/app.md first, then skill.md/SKILL.md fallback).
fn read_app_guide(app_dir: &Path) -> Option<String> {
    for name in &["APP.md", "app.md", "skill.md", "SKILL.md"] {
        let path = app_dir.join(name);
        if let Ok(content) = std::fs::read_to_string(&path) {
            return Some(content);
        }
    }
    None
}

/// Extract a description from app guide markdown.
///
/// Looks for the first non-header, non-empty line, or a TOML frontmatter
/// `description` field.
fn extract_description(guide: &str) -> Option<String> {
    // Check for TOML frontmatter (+++...+++ delimited).
    if let Some(rest) = guide.strip_prefix("+++")
        && let Some(end) = rest.find("+++")
    {
        let frontmatter = &rest[..end];
        for line in frontmatter.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("description")
                && let Some(val) = trimmed.split('=').nth(1)
            {
                let val = val.trim().trim_matches('"');
                if !val.is_empty() {
                    return Some(val.to_string());
                }
            }
        }
    }
    // Fall back to first paragraph after any heading.
    for line in guide.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with("+++") {
            continue;
        }
        return Some(trimmed.to_string());
    }
    None
}

// ── Public API ──────────────────────────────────────────────────────

/// List all available OS apps.
pub fn list_os_apps() -> Vec<AppEntry> {
    let cat = catalog().read().unwrap(); // ci-ok: infallible lock
    cat.entries.clone()
}

/// Backward-compatible alias.
pub fn list_skills() -> Vec<AppEntry> {
    list_os_apps()
}

/// Get the full spec bundle for an OS app by name.
///
/// Reads IOA, CSDL, and Cedar files from disk on each call so changes
/// are picked up without a rebuild.
pub fn get_os_app(name: &str) -> Option<AppBundle> {
    let cat = catalog().read().unwrap(); // ci-ok: infallible lock
    let app_dir = cat.paths.get(name)?;
    load_app_bundle(app_dir)
}

/// Backward-compatible alias.
pub fn get_skill(name: &str) -> Option<AppBundle> {
    get_os_app(name)
}

/// Get the full app guide markdown for an app by name.
pub fn get_app_guide(name: &str) -> Option<String> {
    let cat = catalog().read().unwrap(); // ci-ok: infallible lock
    cat.entries
        .iter()
        .find(|e| e.name == name)
        .and_then(|e| e.app_guide.clone())
}

/// Backward-compatible alias.
pub fn get_skill_guide(name: &str) -> Option<String> {
    get_app_guide(name)
}

/// Load a complete app bundle from a directory on disk.
fn load_app_bundle(app_dir: &Path) -> Option<AppBundle> {
    let manifest = read_app_manifest(app_dir)?;
    let legacy_reaction_paths = [
        app_dir.join("reactions").join("reactions.toml"),
        app_dir.join("specs").join("reactions.toml"),
    ];
    if let Some(path) = legacy_reaction_paths.iter().find(|path| path.exists()) {
        tracing::error!(
            path = %path.display(),
            "legacy reactions.toml is no longer supported; migrate this app to inline [[action.triggers]]"
        );
        return None;
    }
    let ioa_files = find_ioa_files(app_dir);

    // Read IOA specs, extracting entity type from the parsed automaton name.
    let mut specs = Vec::new();
    for (_hint, path) in &ioa_files {
        let source = std::fs::read_to_string(path).ok()?;
        let parsed = automaton::parse_automaton(&source).ok()?;
        specs.push((parsed.automaton.name, source));
    }

    // Read CSDL (optional — apps without specs won't have CSDL).
    let csdl = find_csdl(app_dir).and_then(|p| std::fs::read_to_string(&p).ok());
    let cross_invariants_toml =
        std::fs::read_to_string(app_dir.join("specs").join("cross-invariants.toml")).ok();

    // Read Cedar policies.
    let cedar_policies: Vec<String> = find_cedar_policies(app_dir)
        .into_iter()
        .filter_map(|p| std::fs::read_to_string(&p).ok())
        .collect();

    // Build module configs first so find_wasm_modules can use declared targets.
    let wasm_module_configs: BTreeMap<String, WasmModuleManifest> = manifest
        .wasm_modules
        .into_iter()
        .map(|module| (module.name.clone(), module))
        .collect();

    // Read WASM module binaries, respecting declared targets from app.toml.
    let wasm_modules = find_wasm_modules(app_dir, &wasm_module_configs);

    // Discover agents, skills, and seed data.
    let agents = find_agents(app_dir);
    let skills = find_app_skills(app_dir);
    let adrs = find_adrs(app_dir);
    let system_files = system_files::find_system_files(app_dir);
    let seed_instances = find_seed_data(app_dir);

    // Read app guide to check if there's anything at all.
    let app_guide = read_app_guide(app_dir);

    // Return None only if the app has nothing at all.
    if specs.is_empty()
        && cedar_policies.is_empty()
        && wasm_modules.is_empty()
        && wasm_module_configs.is_empty()
        && agents.is_empty()
        && skills.is_empty()
        && adrs.is_empty()
        && system_files.is_empty()
        && seed_instances.is_empty()
        && app_guide.is_none()
        && csdl.is_none()
        && cross_invariants_toml.is_none()
    {
        return None;
    }

    Some(AppBundle {
        specs,
        csdl,
        cross_invariants_toml,
        cedar_policies,
        wasm_modules,
        wasm_module_configs,
        agents,
        skills,
        adrs,
        system_files,
        seed_instances,
    })
}

fn os_app_dependencies(name: &str) -> Vec<String> {
    // Check manifest first.
    let cat = catalog().read().unwrap(); // ci-ok: infallible lock
    if let Some(entry) = cat.entries.iter().find(|e| e.name == name)
        && !entry.dependencies.is_empty()
    {
        return entry.dependencies.clone();
    }
    // Hardcoded fallback.
    match name {
        // TemperAgent persists conversation/files in TemperFS entities.
        "temper-agent" => vec!["temper-fs".to_string()],
        _ => vec![],
    }
}

/// Install an OS app into a tenant (workspace).
///
/// Reads app files from disk, runs the verification cascade, registers
/// specs in the SpecRegistry, loads Cedar policies, and **persists
/// everything to the platform DB** so specs survive redeployments.
///
/// **Write ordering:** Turso first, then memory. If Turso persistence fails
/// the operation returns an error *before* touching in-memory state, so the
/// registry and Cedar engine stay consistent with the durable store.
pub async fn install_os_app(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
) -> Result<InstallResult, String> {
    let order = resolve_os_app_install_order(&[app_name.to_string()])?;

    let mut final_result = None;
    for app in order {
        let result = install_os_app_without_dependencies(state, tenant, &app).await?;
        if app == app_name {
            final_result = Some(result);
        }
    }
    final_result.ok_or_else(|| format!("OS app '{app_name}' produced no install result"))
}

async fn install_os_app_without_dependencies(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
) -> Result<InstallResult, String> {
    install_os_app_with_plan(state, tenant, app_name, OsAppInstallPlan::all()).await
}

pub(super) async fn install_os_app_with_plan(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
    plan: OsAppInstallPlan,
) -> Result<InstallResult, String> {
    let app_dir = {
        let cat = catalog().read().unwrap(); // ci-ok: infallible lock
        cat.paths.get(app_name).cloned().ok_or_else(|| {
            let known: Vec<String> = cat.paths.keys().cloned().collect();
            format!("OS app '{app_name}' not found in catalog (known path keys: {known:?})")
        })?
    };
    let bundle = load_app_bundle(&app_dir).ok_or_else(|| {
        format!(
            "OS app '{app_name}' is registered at '{}' but its bundle failed to load",
            app_dir.display()
        )
    })?;
    let tenant_id = TenantId::new(tenant);

    if bundle.adrs.is_empty() {
        tracing::warn!(
            tenant,
            app = %app_name,
            "App installed with no ADRs; add adrs/*.md to record design decisions"
        );
    }

    // Classify each bundle spec as added / updated / skipped, and compute the
    // merged CSDL only when the reconcile plan needs spec work.
    let (mut added, mut updated, mut skipped, merged_csdl) = if plan.specs {
        let registry = state.registry.read().unwrap(); // ci-ok: infallible lock
        let mut added = Vec::new();
        let mut updated = Vec::new();
        let mut skipped = Vec::new();
        for (entity_type, ioa_source) in &bundle.specs {
            let incoming_hash = temper_store_turso::spec_content_hash(ioa_source);
            match registry.get_spec(&tenant_id, entity_type) {
                Some(existing) => {
                    let existing_hash = temper_store_turso::spec_content_hash(&existing.ioa_source);
                    let verified = registry
                        .get_verification_status(&tenant_id, entity_type)
                        .map(|s| s.is_passed())
                        .unwrap_or(false);
                    if incoming_hash == existing_hash && verified {
                        skipped.push(entity_type.to_string());
                    } else {
                        updated.push(entity_type.to_string());
                    }
                }
                None => {
                    added.push(entity_type.to_string());
                }
            }
        }
        // App installs must preserve existing tenant types.
        let merged_csdl = if let Some(ref csdl) = bundle.csdl {
            if let Some(existing) = registry.get_tenant(&tenant_id) {
                let incoming = parse_csdl(csdl)
                    .map_err(|e| format!("Failed to parse CSDL for os-app '{app_name}': {e}"))?;
                Some(emit_csdl_xml(&merge_csdl(&existing.csdl, &incoming)))
            } else {
                Some(csdl.clone())
            }
        } else {
            // No CSDL in bundle; keep existing if any.
            registry
                .get_tenant(&tenant_id)
                .map(|t| emit_csdl_xml(&t.csdl))
        };
        (added, updated, skipped, merged_csdl)
    } else {
        (Vec::new(), Vec::new(), Vec::new(), None)
    };
    // Sort for deterministic output.
    added.sort();
    updated.sort();
    skipped.sort();

    // Build the full Cedar policy text for this tenant (existing + new).
    let combined_policy = if plan.policies && !bundle.cedar_policies.is_empty() {
        let combined: String = bundle.cedar_policies.join("\n");
        let policies = state.server.tenant_policies.read().unwrap(); // ci-ok: infallible lock
        let existing = policies.get(tenant).cloned().unwrap_or_default();
        let full_text = if existing.is_empty() {
            combined
        } else {
            format!("{existing}\n{combined}")
        };
        Some(full_text)
    } else {
        None
    };

    // ── Step 1: Persist to Turso FIRST (if available). ──────────────
    // If any write fails, bail before touching in-memory state.
    if let Some(turso) = state
        .server
        .storage_stack
        .as_ref()
        .and_then(|stack| stack.turso.as_ref())
        .and_then(|provider| provider.platform_store())
    {
        if plan.specs
            && let Some(ref merged) = merged_csdl
        {
            // Collect only this app's specs with their content hashes for a
            // single transactional write. Existing tenant specs are preserved by
            // the merged CSDL, but no longer re-written on every app install.
            let owned: Vec<(String, String, String, String)> = bundle
                .specs
                .iter()
                .map(|(et, ioa)| {
                    let hash = temper_store_turso::spec_content_hash(ioa);
                    (et.clone(), ioa.clone(), merged.clone(), hash)
                })
                .collect();
            let refs: Vec<(&str, &str, &str, &str)> = owned
                .iter()
                .map(|(et, ioa, csdl, h)| (et.as_str(), ioa.as_str(), csdl.as_str(), h.as_str()))
                .collect();
            turso
                .upsert_specs_and_commit(tenant, &refs, combined_policy.as_deref(), app_name)
                .await
                .map_err(|e| format!("Failed to persist and commit specs: {e}"))?;
        } else if let Some(ref policy_text) = combined_policy {
            turso
                .upsert_tenant_policy(tenant, policy_text)
                .await
                .map_err(|e| format!("Failed to persist Cedar policy: {e}"))?;
            turso
                .record_installed_app(tenant, app_name)
                .await
                .map_err(|e| format!("Failed to record os-app installation: {e}"))?;
        }
        if plan.specs
            && let Some(ref cross_invariants_toml) = bundle.cross_invariants_toml
        {
            turso
                .upsert_tenant_constraints(tenant, cross_invariants_toml)
                .await
                .map_err(|e| format!("Failed to persist cross-invariants: {e}"))?;
        }
    } else if let Some(ps) = state
        .server
        .storage_stack
        .as_ref()
        .and_then(|stack| stack.platform.clone())
    {
        if plan.specs
            && let Some(ref merged) = merged_csdl
        {
            for (entity_type, ioa_source) in &bundle.specs {
                let hash = temper_store_turso::spec_content_hash(ioa_source);
                ps.upsert_spec(tenant, entity_type, ioa_source, merged, &hash)
                    .await
                    .map_err(|e| format!("Failed to persist spec {entity_type}: {e}"))?;
            }
        }
        if let Some(ref policy_text) = combined_policy {
            ps.upsert_tenant_policy(tenant, policy_text)
                .await
                .map_err(|e| format!("Failed to persist Cedar policy: {e}"))?;
        }
        if plan.specs
            && let Some(ref cross_invariants_toml) = bundle.cross_invariants_toml
        {
            ps.upsert_tenant_constraints(tenant, cross_invariants_toml)
                .await
                .map_err(|e| format!("Failed to persist cross-invariants: {e}"))?;
        }
        ps.record_installed_app(tenant, app_name)
            .await
            .map_err(|e| format!("Failed to record os-app installation: {e}"))?;
        if plan.specs {
            // Commit only when this path used individual spec writes.
            ps.commit_specs(tenant)
                .await
                .map_err(|e| format!("Failed to commit specs: {e}"))?;
        }
    }

    // ── Step 2: Bootstrap into memory (verification + registry). ────
    // Only process specs whose content has changed (added or updated);
    // skipped specs are already loaded with identical content.
    if plan.specs && !bundle.specs.is_empty() {
        let specs_to_bootstrap: Vec<(&str, &str)> = bundle
            .specs
            .iter()
            .map(|(et, src)| (et.as_str(), src.as_str()))
            .collect();
        let verified_cache = if let Some(turso) = state
            .server
            .storage_stack
            .as_ref()
            .and_then(|stack| stack.turso.as_ref())
            .and_then(|provider| provider.platform_store())
        {
            turso
                .load_verification_cache(tenant)
                .await
                .unwrap_or_default()
        } else if let Some(ps) = state
            .server
            .storage_stack
            .as_ref()
            .and_then(|stack| stack.platform.clone())
        {
            ps.load_verification_cache(tenant).await.unwrap_or_default()
        } else {
            std::collections::BTreeMap::new()
        };

        if let Some(ref merged) = merged_csdl {
            // Even when every spec is byte-for-byte unchanged, we still need to
            // merge the app's CSDL back into the in-memory registry so entity-set
            // mappings survive partial restores and process restarts. Re-running
            // bootstrap also lets recovery heal specs that were durably left
            // in `pending` by older installs.
            let spec_hashes = bootstrap::bootstrap_tenant_specs(
                state,
                tenant,
                merged,
                &specs_to_bootstrap,
                bootstrap::BootstrapTenantSpecsOptions {
                    merge: true,
                    label: &format!("OsApp({app_name})"),
                    verified_cache: &verified_cache,
                    cross_invariants_source: bundle.cross_invariants_toml.as_deref(),
                    verification_mode: bootstrap::BootstrapSpecVerificationMode::TrustBundle,
                },
            );

            if let Some(ps) = state
                .server
                .storage_stack
                .as_ref()
                .and_then(|stack| stack.platform.clone())
            {
                bootstrap::persist_bootstrap_verification(
                    ps.as_ref(),
                    tenant,
                    &specs_to_bootstrap,
                    merged,
                    &spec_hashes,
                    &verified_cache,
                )
                .await;
            }
        }
        // App installs can introduce or update inline entity triggers.
        // Refresh the live dispatcher after the registry mutation so the
        // newly bootstrapped trigger graph is active for subsequent traffic.
        state.server.rebuild_reaction_dispatcher();
    }

    // App installs can add or change cross-entity reactions. Refresh the live
    // dispatcher immediately so the newly registered tenant config takes effect
    // without requiring a process restart or a separate specs reload.
    if plan.specs {
        state.server.rebuild_reaction_dispatcher();
    }

    // ── Step 3: Load Cedar policies into memory. ────────────────────
    if let Some(ref policy_text) = combined_policy {
        if let Err(e) = state
            .server
            .authz
            .reload_tenant_policies(tenant, policy_text)
        {
            tracing::warn!(
                tenant,
                error = %e,
                "Failed to reload tenant Cedar policies after os-app install"
            );
        } else {
            let mut policies = state.server.tenant_policies.write().unwrap(); // ci-ok: infallible lock
            policies.insert(tenant.to_string(), policy_text.clone());
        }
    }

    // ── Step 4: Persist/register WASM modules, warming only eager modules. ──
    //
    // Source-aware preservation: if a (tenant, module) row in the durable store
    // has source='upload' (i.e., a hot upload from `POST /api/wasm/modules/{name}`),
    // skip every step for that module — upsert (preserved by the store anyway),
    // registry overwrite (would clobber the upload hash loaded by boot recovery),
    // and engine warm-up (would cache bundled bytes the registry shouldn't pick).
    // Without this guard, hot uploads silently revert on every restart.
    let mut wasm_registered = Vec::new();
    let mut wasm_skipped = Vec::new();
    let mut wasm_failures = Vec::new();
    if plan.wasm {
        let existing_sources = match state.server.load_wasm_module_sources(tenant).await {
            Ok(map) => map,
            Err(e) => {
                tracing::warn!(
                    tenant,
                    error = %e,
                    "Failed to load existing WASM modules; install pipeline will not honor hot-upload preservation this cycle"
                );
                std::collections::BTreeMap::new()
            }
        };

        for (module_name, wasm_bytes) in &bundle.wasm_modules {
            let module_config = bundle.wasm_module_configs.get(module_name);
            let hash = temper_wasm::WasmEngine::hash_module(wasm_bytes);

            if let Some((existing_hash, existing_source)) = existing_sources.get(module_name)
                && existing_source == "upload"
                && existing_hash != &hash
            {
                tracing::info!(
                    tenant,
                    module = %module_name,
                    bundled_hash = %hash,
                    upload_hash = %existing_hash,
                    "Skipping bundled install: hot-uploaded module preserved"
                );
                wasm_skipped.push(module_name.clone());
                continue;
            }

            if let Err(e) = state
                .server
                .upsert_wasm_module(tenant, module_name, wasm_bytes, &hash, "bundled")
                .await
            {
                tracing::warn!(
                    tenant,
                    module = %module_name,
                    error = %e,
                    "Failed to persist WASM module to durable store (continuing in-memory only)"
                );
            }
            {
                let mut wasm_reg = state.server.wasm_module_registry.write().unwrap(); // ci-ok: infallible lock
                wasm_reg.register(&tenant_id, module_name, &hash);
            }

            if matches!(
                module_config.map(|config| config.startup_loading),
                Some(WasmStartupLoading::Eager)
            ) && let Err(e) = state.server.wasm_engine.compile_and_cache(wasm_bytes)
            {
                wasm_failures.push(module_name.clone());
                tracing::warn!(
                    tenant,
                    module = %module_name,
                    error = %e,
                    "Failed to eagerly compile WASM module from OS app"
                );
            }

            tracing::info!(
                tenant,
                module = %module_name,
                hash = %hash,
                size = wasm_bytes.len(),
                startup_loading = ?module_config
                    .map(|config| config.startup_loading)
                    .unwrap_or_default(),
                "WASM module registered from OS app"
            );
            wasm_registered.push(module_name.clone());
        }

        for (module_name, module_config) in &bundle.wasm_module_configs {
            if bundle.wasm_modules.contains_key(module_name) {
                continue;
            }
            match module_config.criticality {
                WasmModuleCriticality::Optional => {
                    wasm_skipped.push(module_name.clone());
                    tracing::warn!(
                        tenant,
                        module = %module_name,
                        "Configured optional WASM module artifact is missing from the app bundle"
                    );
                }
                WasmModuleCriticality::PlatformRequired | WasmModuleCriticality::AppRequired => {
                    wasm_failures.push(module_name.clone());
                    tracing::error!(
                        tenant,
                        module = %module_name,
                        criticality = ?module_config.criticality,
                        "Configured required WASM module artifact is missing from the app bundle"
                    );
                }
            }
        }
    }

    tracing::info!(
        "Installed os-app '{app_name}' for tenant '{tenant}': \
         added={:?} updated={:?} skipped={:?} wasm={:?}",
        added,
        updated,
        skipped,
        wasm_registered,
    );

    // ── Step 5: Bootstrap App entity + APP.md. ──────────────────────────
    let (agents_bootstrapped, skills_bootstrapped, adrs_bootstrapped) = if plan.content {
        let app_id = bootstrap_app_entity(state, &tenant_id, tenant, app_name).await;

        // ── Step 6: Bootstrap agents (returns name→uuid map). ──────────
        let (agents_bootstrapped, agent_uuid_map) = agent_bootstrap::bootstrap_agents(
            state,
            &tenant_id,
            tenant,
            &bundle.agents,
            app_id.as_deref(),
        )
        .await;

        // ── Step 7: Bootstrap skills (agent-scoped + system). ──────────
        let skills_bootstrapped =
            bootstrap_skills(state, &tenant_id, tenant, &bundle.skills, &agent_uuid_map).await;

        // ── Step 7b: Bootstrap system files (e.g. mode-instructions). ─
        system_files::bootstrap_system_files(state, &tenant_id, tenant, &bundle.system_files).await;

        // ── Step 8: Bootstrap ADRs into TemperFS. ─────────────────────
        let adrs_bootstrapped =
            bootstrap_adrs(state, &tenant_id, tenant, app_name, &bundle.adrs).await;

        (agents_bootstrapped, skills_bootstrapped, adrs_bootstrapped)
    } else {
        (Vec::new(), Vec::new(), Vec::new())
    };

    // ── Step 9: Create seed instances. ───────────────────────────────
    let seed_created = if plan.seed {
        bootstrap_seed_data(state, &tenant_id, tenant, &bundle.seed_instances).await
    } else {
        Vec::new()
    };

    reconcile::record_app_install_metadata_for_bundle(state, tenant, app_name, &bundle).await;

    Ok(InstallResult {
        added,
        updated,
        skipped,
        wasm_modules: wasm_registered,
        wasm_skipped,
        wasm_failures,
        agents: agents_bootstrapped,
        skills: skills_bootstrapped,
        adrs_bootstrapped,
        seed_instances: seed_created,
    })
}

/// Backward-compatible alias.
pub async fn install_skill(
    state: &PlatformState,
    tenant: &str,
    skill_name: &str,
) -> Result<InstallResult, String> {
    install_os_app(state, tenant, skill_name).await
}

// ── Bootstrap helpers (entity creation during install) ───────────────

/// Bootstrap an App entity for the installed OS app.
///
/// Creates (or updates) an App entity and writes APP.md to TemperFS.
/// Returns the App entity ID if successful.
async fn bootstrap_app_entity(
    state: &PlatformState,
    tenant_id: &TenantId,
    tenant: &str,
    app_name: &str,
) -> Option<String> {
    // Check if App entity type is registered.
    let has_apps = {
        let registry = state.registry.read().unwrap(); // ci-ok: infallible lock
        registry.get_spec(tenant_id, "App").is_some()
    };
    if !has_apps {
        tracing::debug!(
            tenant,
            app = %app_name,
            "Skipping App entity bootstrap — App entity type not registered"
        );
        return None;
    }

    let agent_ctx = temper_server::request_context::AgentContext::for_service("platform-bootstrap");

    // Look for an existing App entity with this name.
    let existing_ids = state.server.list_entity_ids_lazy(tenant_id, "App").await;
    let mut existing_app_id = None;
    for id in &existing_ids {
        if let Ok(resp) = state
            .server
            .get_tenant_entity_state(tenant_id, "App", id)
            .await
            && let Some(name) = state_field_str(&resp.state.fields, &["Name", "name"])
            && name.eq_ignore_ascii_case(app_name)
        {
            existing_app_id = Some(id.clone());
            break;
        }
    }

    // Read app manifest for metadata.
    let manifest = {
        let cat = catalog().read().unwrap(); // ci-ok: infallible lock
        cat.entries.iter().find(|e| e.name == app_name).cloned()
    };
    let description = manifest
        .as_ref()
        .map(|m| m.description.clone())
        .unwrap_or_default();
    let version = manifest
        .as_ref()
        .map(|m| m.version.clone())
        .unwrap_or_else(|| "0.1.0".to_string());

    // Bootstrap APP.md into TemperFS at /apps/{app-name}/APP.md.
    let mut app_guide_file_id = String::new();
    if let Some(guide) = manifest.as_ref().and_then(|m| m.app_guide.as_ref()) {
        // Ensure the app docs workspace and /apps/{app-name}/ directory exist.
        let has_fs = {
            let registry = state.registry.read().unwrap(); // ci-ok: infallible lock
            registry.get_spec(tenant_id, "File").is_some()
                && registry.get_spec(tenant_id, "Directory").is_some()
                && registry.get_spec(tenant_id, "Workspace").is_some()
        };
        if has_fs {
            if let Err(e) = ensure_app_docs_workspace(state, tenant_id, &agent_ctx).await {
                tracing::warn!(tenant, app = %app_name, error = %e, "Failed to ensure app docs workspace");
            } else {
                let app_slug = slug_fragment(app_name);
                let app_dir_id = format!("os-app-docs-dir-{app_slug}");
                let app_dir_path = format!("{APP_DOCS_ROOT_PATH}/{app_name}");
                if let Err(e) = ensure_directory(
                    state,
                    tenant_id,
                    &agent_ctx,
                    DirectoryBootstrapTarget {
                        directory_id: &app_dir_id,
                        name: app_name,
                        path: &app_dir_path,
                        parent_id: Some(APP_DOCS_ROOT_DIR_ID),
                        workspace_id: APP_DOCS_WORKSPACE_ID,
                    },
                )
                .await
                {
                    tracing::warn!(tenant, app = %app_name, error = %e, "Failed to create app directory for APP.md");
                } else {
                    let file_id_str = format!("os-app-guide-{app_slug}");
                    let file_path = format!("{app_dir_path}/APP.md");
                    if let Err(e) = ensure_markdown_file(
                        state,
                        tenant_id,
                        &agent_ctx,
                        MarkdownFileBootstrapTarget {
                            file_id: &file_id_str,
                            name: "APP.md",
                            path: &file_path,
                            directory_id: &app_dir_id,
                            workspace_id: APP_DOCS_WORKSPACE_ID,
                        },
                        guide.as_bytes(),
                    )
                    .await
                    {
                        tracing::warn!(tenant, app = %app_name, error = %e, "Failed to bootstrap APP.md");
                    } else {
                        app_guide_file_id = file_id_str;
                        tracing::info!(tenant, app = %app_name, path = %file_path, "APP.md bootstrapped into TemperFS");
                    }
                }
            }
        }
    }

    if let Some(ref id) = existing_app_id {
        // Update existing App entity.
        let _ = state
            .server
            .dispatch(temper_server::state::DispatchCommand {
                tenant: tenant_id,
                entity_type: "App",
                entity_id: id,
                action: "Install",
                params: serde_json::json!({
                    "name": app_name,
                    "description": description,
                    "version": version,
                    "app_guide_file_id": app_guide_file_id,
                }),
                agent_ctx: &agent_ctx,
                await_integration: false,
            })
            .await;
        tracing::info!(tenant, app = %app_name, id = %id, "App entity updated");
        return existing_app_id;
    }

    // Create new App entity with a prefixed UUID.
    let new_app_id = format!("ap-{}", temper_runtime::scheduler::sim_uuid());
    match state
        .server
        .get_or_create_tenant_entity(tenant_id, "App", &new_app_id, serde_json::json!({}))
        .await
    {
        Ok(_) => {
            let app_id = new_app_id;
            // Initialize with Install action.
            if let Err(e) = state
                .server
                .dispatch(temper_server::state::DispatchCommand {
                    tenant: tenant_id,
                    entity_type: "App",
                    entity_id: &app_id,
                    action: "Install",
                    params: serde_json::json!({
                        "name": app_name,
                        "description": description,
                        "version": version,
                        "app_guide_file_id": app_guide_file_id,
                    }),
                    agent_ctx: &agent_ctx,
                    await_integration: false,
                })
                .await
            {
                tracing::warn!(
                    tenant,
                    app = %app_name,
                    error = %e,
                    "Failed to initialize App entity"
                );
                return None;
            }
            tracing::info!(tenant, app = %app_name, id = %app_id, "App entity created");
            Some(app_id)
        }
        Err(e) => {
            tracing::warn!(
                tenant,
                app = %app_name,
                error = %e,
                "Failed to create App entity"
            );
            None
        }
    }
}

/// Bootstrap skills as TemperFS files at path-based scope locations (ADR-002).
///
/// Skills are written to:
/// - `/system/skills/{slug}/SKILL.md` — system-level skills (agent_name = None)
/// - `/agents/{agent-uuid}/skills/{slug}/SKILL.md` — agent-scoped skills
///
/// No Skill entities are created — the file IS the skill.
/// Returns the names of successfully bootstrapped skills.
async fn bootstrap_skills(
    state: &PlatformState,
    tenant_id: &TenantId,
    tenant: &str,
    skills: &[AppSkillDefinition],
    agent_uuid_map: &BTreeMap<String, String>,
) -> Vec<String> {
    if skills.is_empty() {
        return Vec::new();
    }

    // Require TemperFS types (File, Directory).
    let has_fs = {
        let registry = state.registry.read().unwrap(); // ci-ok: infallible lock
        registry.get_spec(tenant_id, "File").is_some()
            && registry.get_spec(tenant_id, "Directory").is_some()
    };
    if !has_fs {
        tracing::info!(
            tenant,
            count = skills.len(),
            "Skipping skill bootstrap — TemperFS not registered (install temper-fs first)"
        );
        return Vec::new();
    }

    let agent_ctx = temper_server::request_context::AgentContext::for_service("platform-bootstrap");

    // Ensure workspace exists.
    if let Err(e) = ensure_app_docs_workspace(state, tenant_id, &agent_ctx).await {
        tracing::warn!(tenant, error = %e, "Failed to ensure app docs workspace for skill bootstrap");
        return Vec::new();
    }

    // Ensure /system/ root directory exists.
    if let Err(e) = ensure_directory(
        state,
        tenant_id,
        &agent_ctx,
        DirectoryBootstrapTarget {
            directory_id: "os-system-root",
            name: "system",
            path: "/system",
            parent_id: Some(APP_DOCS_ROOT_DIR_ID),
            workspace_id: APP_DOCS_WORKSPACE_ID,
        },
    )
    .await
    {
        tracing::warn!(tenant, error = %e, "Failed to create /system/ directory");
        return Vec::new();
    }

    // Ensure /system/skills/ directory exists.
    if let Err(e) = ensure_directory(
        state,
        tenant_id,
        &agent_ctx,
        DirectoryBootstrapTarget {
            directory_id: "os-system-skills-root",
            name: "skills",
            path: "/system/skills",
            parent_id: Some("os-system-root"),
            workspace_id: APP_DOCS_WORKSPACE_ID,
        },
    )
    .await
    {
        tracing::warn!(tenant, error = %e, "Failed to create /system/skills/ directory");
        return Vec::new();
    }

    // Ensure /agents/ root directory exists.
    if let Err(e) = ensure_directory(
        state,
        tenant_id,
        &agent_ctx,
        DirectoryBootstrapTarget {
            directory_id: "os-agents-root",
            name: "agents",
            path: "/agents",
            parent_id: Some(APP_DOCS_ROOT_DIR_ID),
            workspace_id: APP_DOCS_WORKSPACE_ID,
        },
    )
    .await
    {
        tracing::warn!(tenant, error = %e, "Failed to create /agents/ directory");
        return Vec::new();
    }

    let mut bootstrapped = Vec::new();

    for skill in skills {
        let slug = skill.name.to_lowercase().replace(' ', "-");

        // Determine TemperFS path based on scope.
        let (dir_id, file_id, dir_path, file_path, parent_dir_id) = match &skill.agent_name {
            None => {
                // System skill: /system/skills/{slug}/SKILL.md
                let dir_id = format!("os-sys-skill-dir-{slug}");
                let file_id = format!("os-sys-skill-file-{slug}");
                let dir_path = format!("/system/skills/{slug}");
                let file_path = format!("/system/skills/{slug}/SKILL.md");
                (
                    dir_id,
                    file_id,
                    dir_path,
                    file_path,
                    "os-system-skills-root".to_string(),
                )
            }
            Some(agent_name) => {
                // Agent-scoped skill: /agents/{agent-uuid}/skills/{slug}/SKILL.md
                let agent_uuid = match agent_uuid_map.get(agent_name) {
                    Some(uuid) => uuid.clone(),
                    None => {
                        tracing::warn!(
                            tenant,
                            skill = %skill.name,
                            agent = %agent_name,
                            "Agent UUID not found for agent-scoped skill — skipping"
                        );
                        continue;
                    }
                };

                // Ensure /agents/{agent-uuid}/ directory exists.
                let agent_dir_id = format!("os-agent-dir-{}", slug_fragment(&agent_uuid));
                let agent_dir_path = format!("/agents/{agent_uuid}");
                if let Err(e) = ensure_directory(
                    state,
                    tenant_id,
                    &agent_ctx,
                    DirectoryBootstrapTarget {
                        directory_id: &agent_dir_id,
                        name: agent_name,
                        path: &agent_dir_path,
                        parent_id: Some("os-agents-root"),
                        workspace_id: APP_DOCS_WORKSPACE_ID,
                    },
                )
                .await
                {
                    tracing::warn!(tenant, agent = %agent_name, error = %e, "Failed to create agent directory");
                    continue;
                }

                // Ensure /agents/{agent-uuid}/skills/ directory exists.
                let agent_skills_dir_id =
                    format!("os-agent-skills-dir-{}", slug_fragment(&agent_uuid));
                let agent_skills_dir_path = format!("/agents/{agent_uuid}/skills");
                if let Err(e) = ensure_directory(
                    state,
                    tenant_id,
                    &agent_ctx,
                    DirectoryBootstrapTarget {
                        directory_id: &agent_skills_dir_id,
                        name: "skills",
                        path: &agent_skills_dir_path,
                        parent_id: Some(&agent_dir_id),
                        workspace_id: APP_DOCS_WORKSPACE_ID,
                    },
                )
                .await
                {
                    tracing::warn!(tenant, agent = %agent_name, error = %e, "Failed to create agent skills directory");
                    continue;
                }

                let dir_id = format!("os-agent-skill-dir-{}-{slug}", slug_fragment(&agent_uuid));
                let file_id = format!("os-agent-skill-file-{}-{slug}", slug_fragment(&agent_uuid));
                let dir_path = format!("/agents/{agent_uuid}/skills/{slug}");
                let file_path = format!("/agents/{agent_uuid}/skills/{slug}/SKILL.md");
                (dir_id, file_id, dir_path, file_path, agent_skills_dir_id)
            }
        };

        // Create skill directory.
        if let Err(e) = ensure_directory(
            state,
            tenant_id,
            &agent_ctx,
            DirectoryBootstrapTarget {
                directory_id: &dir_id,
                name: &slug,
                path: &dir_path,
                parent_id: Some(&parent_dir_id),
                workspace_id: APP_DOCS_WORKSPACE_ID,
            },
        )
        .await
        {
            tracing::warn!(tenant, skill = %skill.name, error = %e, "Failed to create skill directory");
            continue;
        }

        // Create SKILL.md and upload content.
        if let Err(e) = ensure_markdown_file(
            state,
            tenant_id,
            &agent_ctx,
            MarkdownFileBootstrapTarget {
                file_id: &file_id,
                name: "SKILL.md",
                path: &file_path,
                directory_id: &dir_id,
                workspace_id: APP_DOCS_WORKSPACE_ID,
            },
            skill.content.as_bytes(),
        )
        .await
        {
            tracing::warn!(tenant, skill = %skill.name, error = %e, "Failed to create SKILL.md file");
            continue;
        }

        // Bootstrap companion files in the same directory.
        for companion in &skill.companion_files {
            let comp_slug = companion
                .name
                .replace(std::path::MAIN_SEPARATOR, "-")
                .to_lowercase();
            let comp_file_id = format!("{file_id}-{comp_slug}");
            let comp_file_path = format!("{dir_path}/{}", companion.name);
            let comp_file_name = companion.name.rsplit('/').next().unwrap_or(&companion.name);
            if let Err(e) = ensure_markdown_file(
                state,
                tenant_id,
                &agent_ctx,
                MarkdownFileBootstrapTarget {
                    file_id: &comp_file_id,
                    name: comp_file_name,
                    path: &comp_file_path,
                    directory_id: &dir_id,
                    workspace_id: APP_DOCS_WORKSPACE_ID,
                },
                &companion.content,
            )
            .await
            {
                tracing::warn!(
                    tenant,
                    skill = %skill.name,
                    companion = %companion.name,
                    error = %e,
                    "Failed to create companion file"
                );
            }
        }

        let scope_label = match &skill.agent_name {
            None => "system",
            Some(a) => a.as_str(),
        };
        tracing::info!(tenant, skill = %skill.name, scope = %scope_label, path = %file_path, "Skill bootstrapped as TemperFS file");
        bootstrapped.push(skill.name.clone());
    }
    bootstrapped
}

pub(super) const APP_DOCS_WORKSPACE_ID: &str = "os-app-docs";
const APP_DOCS_WORKSPACE_NAME: &str = "apps";
pub(super) const APP_DOCS_ROOT_DIR_ID: &str = "os-app-docs-root";
const APP_DOCS_ROOT_PATH: &str = "/apps";
const APP_DOCS_QUOTA_BYTES: i64 = 1_099_511_627_776;

fn state_field_str<'a>(fields: &'a serde_json::Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| fields.get(*key).and_then(|value| value.as_str()))
}

/// Bootstrap app-local ADRs into TemperFS under `/apps/{app-name}/adrs/`.
async fn bootstrap_adrs(
    state: &PlatformState,
    tenant_id: &TenantId,
    tenant: &str,
    app_name: &str,
    adrs: &[AdrEntry],
) -> Vec<String> {
    if adrs.is_empty() {
        return Vec::new();
    }

    let has_workspace = {
        let registry = state.registry.read().unwrap(); // ci-ok: infallible lock
        registry.get_spec(tenant_id, "Workspace").is_some()
    };
    let has_directory = {
        let registry = state.registry.read().unwrap(); // ci-ok: infallible lock
        registry.get_spec(tenant_id, "Directory").is_some()
    };
    let has_file = {
        let registry = state.registry.read().unwrap(); // ci-ok: infallible lock
        registry.get_spec(tenant_id, "File").is_some()
    };
    if !(has_workspace && has_directory && has_file) {
        tracing::info!(
            tenant,
            app = %app_name,
            count = adrs.len(),
            "Skipping ADR bootstrap — TemperFS types not registered (install temper-fs first)"
        );
        return Vec::new();
    }

    let agent_ctx = temper_server::request_context::AgentContext::for_service("platform-bootstrap");
    if let Err(error) = ensure_app_docs_workspace(state, tenant_id, &agent_ctx).await {
        tracing::warn!(
            tenant,
            app = %app_name,
            error = %error,
            "Failed to ensure app docs workspace for ADR bootstrap"
        );
        return Vec::new();
    }

    let app_slug = slug_fragment(app_name);
    let app_dir_id = format!("os-app-docs-dir-{app_slug}");
    let app_dir_path = format!("{APP_DOCS_ROOT_PATH}/{app_name}");
    if let Err(error) = ensure_directory(
        state,
        tenant_id,
        &agent_ctx,
        DirectoryBootstrapTarget {
            directory_id: &app_dir_id,
            name: app_name,
            path: &app_dir_path,
            parent_id: Some(APP_DOCS_ROOT_DIR_ID),
            workspace_id: APP_DOCS_WORKSPACE_ID,
        },
    )
    .await
    {
        tracing::warn!(
            tenant,
            app = %app_name,
            error = %error,
            "Failed to ensure app ADR directory"
        );
        return Vec::new();
    }

    let adrs_dir_id = format!("os-app-docs-dir-{app_slug}-adrs");
    let adrs_dir_path = format!("{app_dir_path}/adrs");
    if let Err(error) = ensure_directory(
        state,
        tenant_id,
        &agent_ctx,
        DirectoryBootstrapTarget {
            directory_id: &adrs_dir_id,
            name: "adrs",
            path: &adrs_dir_path,
            parent_id: Some(app_dir_id.as_str()),
            workspace_id: APP_DOCS_WORKSPACE_ID,
        },
    )
    .await
    {
        tracing::warn!(
            tenant,
            app = %app_name,
            error = %error,
            "Failed to ensure app ADR subdirectory"
        );
        return Vec::new();
    }

    let mut bootstrapped = Vec::new();
    for adr in adrs {
        let file_slug = slug_fragment(&adr.name);
        let file_id = format!("os-app-adr-{app_slug}-{file_slug}");
        let file_path = format!("{adrs_dir_path}/{}", adr.file_name);
        match ensure_markdown_file(
            state,
            tenant_id,
            &agent_ctx,
            MarkdownFileBootstrapTarget {
                file_id: &file_id,
                name: &adr.file_name,
                path: &file_path,
                directory_id: &adrs_dir_id,
                workspace_id: APP_DOCS_WORKSPACE_ID,
            },
            adr.content.as_bytes(),
        )
        .await
        {
            Ok(()) => bootstrapped.push(file_path),
            Err(error) => {
                tracing::warn!(
                    tenant,
                    app = %app_name,
                    adr = %adr.file_name,
                    error = %error,
                    "Failed to bootstrap ADR into TemperFS"
                );
            }
        }
    }

    bootstrapped
}

pub(super) async fn ensure_app_docs_workspace(
    state: &PlatformState,
    tenant_id: &TenantId,
    agent_ctx: &temper_server::request_context::AgentContext,
) -> Result<(), String> {
    if !state
        .server
        .ensure_entity_loaded(tenant_id, "Workspace", APP_DOCS_WORKSPACE_ID)
        .await
    {
        state
            .server
            .get_or_create_tenant_entity(
                tenant_id,
                "Workspace",
                APP_DOCS_WORKSPACE_ID,
                serde_json::json!({}),
            )
            .await
            .map_err(|e| format!("failed to create app docs workspace entity: {e}"))?;
        state
            .server
            .dispatch(temper_server::state::DispatchCommand {
                tenant: tenant_id,
                entity_type: "Workspace",
                entity_id: APP_DOCS_WORKSPACE_ID,
                action: "Create",
                params: serde_json::json!({
                    "name": APP_DOCS_WORKSPACE_NAME,
                    "quota_limit": APP_DOCS_QUOTA_BYTES,
                }),
                agent_ctx,
                await_integration: false,
            })
            .await
            .map_err(|e| format!("failed to initialize app docs workspace: {e}"))?;
    }

    ensure_directory(
        state,
        tenant_id,
        agent_ctx,
        DirectoryBootstrapTarget {
            directory_id: APP_DOCS_ROOT_DIR_ID,
            name: APP_DOCS_WORKSPACE_NAME,
            path: APP_DOCS_ROOT_PATH,
            parent_id: None,
            workspace_id: APP_DOCS_WORKSPACE_ID,
        },
    )
    .await
}

pub(super) struct DirectoryBootstrapTarget<'a> {
    pub(super) directory_id: &'a str,
    pub(super) name: &'a str,
    pub(super) path: &'a str,
    pub(super) parent_id: Option<&'a str>,
    pub(super) workspace_id: &'a str,
}

pub(super) async fn ensure_directory(
    state: &PlatformState,
    tenant_id: &TenantId,
    agent_ctx: &temper_server::request_context::AgentContext,
    target: DirectoryBootstrapTarget<'_>,
) -> Result<(), String> {
    if state
        .server
        .ensure_entity_loaded(tenant_id, "Directory", target.directory_id)
        .await
    {
        return Ok(());
    }

    state
        .server
        .get_or_create_tenant_entity(
            tenant_id,
            "Directory",
            target.directory_id,
            serde_json::json!({}),
        )
        .await
        .map_err(|e| {
            format!(
                "failed to create Directory('{}') actor: {e}",
                target.directory_id
            )
        })?;
    state
        .server
        .dispatch(temper_server::state::DispatchCommand {
            tenant: tenant_id,
            entity_type: "Directory",
            entity_id: target.directory_id,
            action: "Create",
            params: serde_json::json!({
                "name": target.name,
                "path": target.path,
                "parent_id": target.parent_id,
                "workspace_id": target.workspace_id,
            }),
            agent_ctx,
            await_integration: false,
        })
        .await
        .map_err(|e| {
            format!(
                "failed to initialize Directory('{}'): {e}",
                target.directory_id
            )
        })?;

    if let Some(parent_id) = target.parent_id {
        state
            .server
            .dispatch(temper_server::state::DispatchCommand {
                tenant: tenant_id,
                entity_type: "Directory",
                entity_id: parent_id,
                action: "AddChild",
                params: serde_json::json!({}),
                agent_ctx,
                await_integration: false,
            })
            .await
            .map_err(|e| {
                format!(
                    "failed to register child directory '{}': {e}",
                    target.directory_id
                )
            })?;
    }

    Ok(())
}

pub(super) struct MarkdownFileBootstrapTarget<'a> {
    pub(super) file_id: &'a str,
    pub(super) name: &'a str,
    pub(super) path: &'a str,
    pub(super) directory_id: &'a str,
    pub(super) workspace_id: &'a str,
}

pub(super) async fn ensure_markdown_file(
    state: &PlatformState,
    tenant_id: &TenantId,
    agent_ctx: &temper_server::request_context::AgentContext,
    target: MarkdownFileBootstrapTarget<'_>,
    content: &[u8],
) -> Result<(), String> {
    let existed = state
        .server
        .ensure_entity_loaded(tenant_id, "File", target.file_id)
        .await;
    if !existed {
        state
            .server
            .get_or_create_tenant_entity(tenant_id, "File", target.file_id, serde_json::json!({}))
            .await
            .map_err(|e| format!("failed to create File('{}') actor: {e}", target.file_id))?;
        state
            .server
            .dispatch(temper_server::state::DispatchCommand {
                tenant: tenant_id,
                entity_type: "File",
                entity_id: target.file_id,
                action: "Create",
                params: serde_json::json!({
                    "name": target.name,
                    "path": target.path,
                    "directory_id": target.directory_id,
                    "workspace_id": target.workspace_id,
                    "mime_type": "text/markdown",
                }),
                agent_ctx,
                await_integration: false,
            })
            .await
            .map_err(|e| format!("failed to initialize File('{}'): {e}", target.file_id))?;
        state
            .server
            .dispatch(temper_server::state::DispatchCommand {
                tenant: tenant_id,
                entity_type: "Directory",
                entity_id: target.directory_id,
                action: "AddChild",
                params: serde_json::json!({}),
                agent_ctx,
                await_integration: false,
            })
            .await
            .map_err(|e| {
                format!(
                    "failed to register file '{}' with parent directory: {e}",
                    target.file_id
                )
            })?;
        state
            .server
            .dispatch(temper_server::state::DispatchCommand {
                tenant: tenant_id,
                entity_type: "Workspace",
                entity_id: target.workspace_id,
                action: "IncrementFileCount",
                params: serde_json::json!({}),
                agent_ctx,
                await_integration: false,
            })
            .await
            .map_err(|e| {
                format!(
                    "failed to increment file count for workspace '{}': {e}",
                    target.workspace_id
                )
            })?;
    }

    let desired_hash = content_sha256(content);
    if file_already_contains(state, tenant_id, target.file_id, &desired_hash).await? {
        return Ok(());
    }

    state
        .server
        .put_file_stream_content(
            tenant_id,
            target.file_id,
            content,
            "text/markdown",
            agent_ctx,
        )
        .await
        .map_err(|e| format!("failed to upload File('{}') content: {e}", target.file_id))?;

    Ok(())
}

async fn file_already_contains(
    state: &PlatformState,
    tenant_id: &TenantId,
    file_id: &str,
    desired_hash: &str,
) -> Result<bool, String> {
    let response = state
        .server
        .get_tenant_entity_state(tenant_id, "File", file_id)
        .await
        .map_err(|e| format!("failed to inspect File('{file_id}'): {e}"))?;

    let has_content = response
        .state
        .booleans
        .get("has_content")
        .copied()
        .unwrap_or(false);
    let current_hash = response
        .state
        .fields
        .get("content_hash")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    Ok(has_content && current_hash == desired_hash)
}

fn content_sha256(content: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content);
    format!("sha256:{:x}", hasher.finalize())
}

pub(super) fn slug_fragment(value: &str) -> String {
    let mut slug = String::new();
    let mut last_was_sep = true;
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            last_was_sep = false;
        } else if !last_was_sep {
            slug.push('-');
            last_was_sep = true;
        }
    }
    slug.trim_matches('-').to_string()
}

/// Bootstrap seed data instances into the tenant.
///
/// For each seed instance:
/// 1. Check if the entity type is registered
/// 2. Create the entity
/// 3. Dispatch each action in order
///
/// Returns descriptions of successfully created instances.
async fn bootstrap_seed_data(
    state: &PlatformState,
    tenant_id: &TenantId,
    tenant: &str,
    instances: &[SeedInstance],
) -> Vec<String> {
    if instances.is_empty() {
        return Vec::new();
    }

    let agent_ctx = temper_server::request_context::AgentContext::for_service("platform-bootstrap");
    let mut created = Vec::new();

    for instance in instances {
        // Check if entity type is registered.
        let type_exists = {
            let registry = state.registry.read().unwrap(); // ci-ok: infallible lock
            registry
                .get_spec(tenant_id, &instance.entity_type)
                .is_some()
        };
        if !type_exists {
            tracing::warn!(
                tenant,
                entity_type = %instance.entity_type,
                "Skipping seed instance — entity type not registered"
            );
            continue;
        }

        // Determine entity ID.
        let entity_id = instance.id.clone().unwrap_or_else(|| {
            // Generate a deterministic ID from type + fields.
            let hash_input = format!("{}-{}", instance.entity_type, instance.fields);
            format!(
                "seed-{}",
                &format!("{:x}", md5_like_hash(&hash_input))[..12]
            )
        });

        // Check if entity already exists (idempotent).
        if state
            .server
            .entity_exists(tenant_id, &instance.entity_type, &entity_id)
        {
            tracing::debug!(
                tenant,
                entity_type = %instance.entity_type,
                entity_id = %entity_id,
                "Seed entity already exists — skipping"
            );
            created.push(format!("{}({})", instance.entity_type, entity_id));
            continue;
        }

        // Create entity with initial fields.
        let initial_fields = if instance.fields.is_null() {
            serde_json::json!({})
        } else {
            instance.fields.clone()
        };

        match state
            .server
            .get_or_create_tenant_entity(
                tenant_id,
                &instance.entity_type,
                &entity_id,
                initial_fields,
            )
            .await
        {
            Ok(_) => {
                // Dispatch each action in order.
                for action in &instance.actions {
                    let params = if action.params.is_null() {
                        serde_json::json!({})
                    } else {
                        action.params.clone()
                    };
                    if let Err(e) = state
                        .server
                        .dispatch(temper_server::state::DispatchCommand {
                            tenant: tenant_id,
                            entity_type: &instance.entity_type,
                            entity_id: &entity_id,
                            action: &action.name,
                            params,
                            agent_ctx: &agent_ctx,
                            await_integration: false,
                        })
                        .await
                    {
                        tracing::warn!(
                            tenant,
                            entity_type = %instance.entity_type,
                            entity_id = %entity_id,
                            action = %action.name,
                            error = %e,
                            "Failed to dispatch seed action"
                        );
                    }
                }
                tracing::info!(
                    tenant,
                    entity_type = %instance.entity_type,
                    entity_id = %entity_id,
                    "Seed entity created"
                );
                created.push(format!("{}({})", instance.entity_type, entity_id));
            }
            Err(e) => {
                tracing::warn!(
                    tenant,
                    entity_type = %instance.entity_type,
                    entity_id = %entity_id,
                    error = %e,
                    "Failed to create seed entity"
                );
            }
        }
    }
    created
}

/// Simple hash for generating deterministic seed entity IDs.
fn md5_like_hash(input: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    input.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod mod_test;
