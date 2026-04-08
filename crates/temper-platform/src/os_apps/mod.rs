//! OS App Catalog — agent-installable pre-built application specs.
//!
//! OS apps are spec bundles (IOA TOML + CSDL + Cedar policies) loaded from
//! the `os-apps/` directory at runtime. Agents discover them via
//! `list_os_apps()` / `install_os_app()`.
//!
//! Backward-compatible skill aliases are preserved (`list_skills()`,
//! `install_skill()`) to avoid breaking older callers.
//!
//! Install reuses [`crate::bootstrap::bootstrap_tenant_specs`] so every app
//! goes through the same verification cascade as system specs.

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

/// Result of an app installation, categorising each spec by what happened.
#[derive(Debug, Clone, Serialize)]
pub struct InstallResult {
    /// Entity types registered for the first time.
    pub added: Vec<String>,
    /// Entity types that already existed but whose IOA source changed.
    pub updated: Vec<String>,
    /// Entity types whose IOA source was byte-for-byte identical — skipped.
    pub skipped: Vec<String>,
    /// WASM modules compiled and registered.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub wasm_modules: Vec<String>,
    /// Agent definitions bootstrapped.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub agents: Vec<String>,
    /// Skill definitions bootstrapped.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<String>,
    /// ADR markdown files bootstrapped into TemperFS.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub adrs_bootstrapped: Vec<String>,
    /// Seed data instances created.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub seed_instances: Vec<String>,
}

/// Parsed app.toml manifest.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct AppManifest {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_version")]
    pub version: String,
    #[serde(default)]
    pub dependencies: Vec<String>,
}

fn default_version() -> String {
    "0.1.0".to_string()
}

fn read_app_manifest(app_dir: &Path) -> Option<AppManifest> {
    let path = app_dir.join("app.toml");
    let content = std::fs::read_to_string(&path).ok()?;
    toml::from_str(&content).ok()
}

/// Metadata for an app in the catalog.
#[derive(Debug, Clone, Serialize)]
pub struct AppEntry {
    /// Short name used in CLI flags and API calls (e.g. `"project-management"`).
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// Entity types included in the app.
    pub entity_types: Vec<String>,
    /// Semantic version.
    pub version: String,
    /// Full app guide markdown (from `APP.md`/`app.md`/`skill.md`), if available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_guide: Option<String>,
    /// Declared dependencies (from app.toml).
    #[serde(default)]
    pub dependencies: Vec<String>,
}

// Backward-compatible alias: SkillEntry → AppEntry.
pub type SkillEntry = AppEntry;

/// Full spec bundle for an app (owned, loaded from disk).
pub struct AppBundle {
    /// IOA spec sources as `(entity_type, ioa_toml_source)` pairs.
    pub specs: Vec<(String, String)>,
    /// CSDL XML source (None if app has no IOA specs).
    pub csdl: Option<String>,
    /// Cedar policy sources (may be empty).
    pub cedar_policies: Vec<String>,
    /// WASM module binaries as `(module_name, wasm_bytes)` pairs.
    pub wasm_modules: BTreeMap<String, Vec<u8>>,
    /// Agent definitions discovered from `agents/` subdirectories.
    pub agents: Vec<AgentDefinition>,
    /// Skill definitions discovered from `skills/` subdirectories.
    pub skills: Vec<AppSkillDefinition>,
    /// ADR markdown files discovered from `adrs/`.
    pub adrs: Vec<AdrEntry>,
    /// Seed data instances discovered from `seed-data/` TOML files.
    pub seed_instances: Vec<SeedInstance>,
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

/// A skill definition discovered in the app's `skills/{name}/` directory.
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
    /// Scope for injection filtering. Read from TOML frontmatter
    /// (`+++scope = "Paw"+++`) or defaults to `"global"`.
    pub scope: String,
    /// Companion files in the skill directory (everything except SKILL.md).
    #[serde(skip)]
    pub companion_files: Vec<CompanionFile>,
}

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
fn find_wasm_modules(app_dir: &Path) -> BTreeMap<String, Vec<u8>> {
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
        let candidates = [
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
        ];

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

/// Discover skill definitions from `skills/{name}/` subdirectories.
///
/// Each subdirectory must contain a `SKILL.md` file as the main document.
/// All other files are collected as companion files.
fn find_app_skills(app_dir: &Path) -> Vec<AppSkillDefinition> {
    let skills_dir = app_dir.join("skills");
    if !skills_dir.is_dir() {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(&skills_dir) else {
        return Vec::new();
    };

    let mut dirs: Vec<_> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .collect();
    dirs.sort_by_key(|e| e.file_name());

    let mut results = Vec::new();
    for dir_entry in dirs {
        let skill_name = dir_entry.file_name().to_string_lossy().to_string();
        let skill_dir = dir_entry.path();

        // Main skill document.
        let skill_path = skill_dir.join("SKILL.md");
        let content = match std::fs::read_to_string(&skill_path) {
            Ok(c) => c,
            Err(_) => continue, // Skip directories without SKILL.md
        };

        let description =
            extract_description(&content).unwrap_or_else(|| format!("Skill: {skill_name}"));

        // Extract scope from TOML frontmatter if present.
        let scope = extract_scope(&content).unwrap_or_else(|| "global".to_string());

        // Collect companion files (everything except SKILL.md).
        let companion_files = collect_companion_files(&skill_dir);

        results.push(AppSkillDefinition {
            name: skill_name,
            content,
            description,
            scope,
            companion_files,
        });
    }
    results
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

/// Extract a `scope` value from TOML frontmatter (`+++...+++`).
fn extract_scope(content: &str) -> Option<String> {
    let rest = content.strip_prefix("+++")?;
    let end = rest.find("+++")?;
    let frontmatter = &rest[..end];
    for line in frontmatter.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("scope")
            && let Some(val) = trimmed.split('=').nth(1)
        {
            let val = val.trim().trim_matches('"');
            if !val.is_empty() {
                return Some(val.to_string());
            }
        }
    }
    None
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

    // Read Cedar policies.
    let cedar_policies: Vec<String> = find_cedar_policies(app_dir)
        .into_iter()
        .filter_map(|p| std::fs::read_to_string(&p).ok())
        .collect();

    // Read WASM module binaries from wasm/*/target/wasm32-unknown-unknown/release/*.wasm.
    let wasm_modules = find_wasm_modules(app_dir);

    // Discover agents, skills, and seed data.
    let agents = find_agents(app_dir);
    let skills = find_app_skills(app_dir);
    let adrs = find_adrs(app_dir);
    let seed_instances = find_seed_data(app_dir);

    // Read app guide to check if there's anything at all.
    let app_guide = read_app_guide(app_dir);

    // Return None only if the app has nothing at all.
    if specs.is_empty()
        && cedar_policies.is_empty()
        && wasm_modules.is_empty()
        && agents.is_empty()
        && skills.is_empty()
        && adrs.is_empty()
        && seed_instances.is_empty()
        && app_guide.is_none()
        && csdl.is_none()
    {
        return None;
    }

    Some(AppBundle {
        specs,
        csdl,
        cedar_policies,
        wasm_modules,
        agents,
        skills,
        adrs,
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
    for dependency in os_app_dependencies(app_name) {
        install_os_app_without_dependencies(state, tenant, &dependency).await?;
    }
    install_os_app_without_dependencies(state, tenant, app_name).await
}

async fn install_os_app_without_dependencies(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
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
    // merged CSDL — both require the registry read lock, so we do them together.
    let (mut added, mut updated, mut skipped, merged_csdl) = {
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
    };
    // Sort for deterministic output.
    added.sort();
    updated.sort();
    skipped.sort();

    // Build the full Cedar policy text for this tenant (existing + new).
    let combined_policy = if !bundle.cedar_policies.is_empty() {
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
    if let Some(ref store) = state.server.event_store
        && let Some(turso) = store.platform_turso_store()
    {
        let mut spec_sources: BTreeMap<String, String> = turso
            .load_specs()
            .await
            .map_err(|e| format!("Failed to load existing specs for tenant '{tenant}': {e}"))?
            .into_iter()
            .filter(|row| row.tenant == tenant)
            .map(|row| (row.entity_type, row.ioa_source))
            .collect();

        for (entity_type, ioa_source) in &bundle.specs {
            spec_sources.insert(entity_type.clone(), ioa_source.clone());
        }

        if let Some(ref merged) = merged_csdl {
            // Collect all specs with their content hashes for a single
            // transactional write — eliminates the crash window between
            // individual upserts (committed=0) and the commit step.
            let owned: Vec<(String, String, String, String)> = spec_sources
                .into_iter()
                .map(|(et, ioa)| {
                    let hash = temper_store_turso::spec_content_hash(&ioa);
                    (et, ioa, merged.clone(), hash)
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
        }
    } else if let Some(ref store) = state.server.event_store
        && let Some(ps) = store.platform_store()
    {
        if let Some(ref merged) = merged_csdl {
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
        ps.record_installed_app(tenant, app_name)
            .await
            .map_err(|e| format!("Failed to record os-app installation: {e}"))?;
        // Commit all specs atomically after all writes succeed.
        ps.commit_specs(tenant)
            .await
            .map_err(|e| format!("Failed to commit specs: {e}"))?;
    }

    // ── Step 2: Bootstrap into memory (verification + registry). ────
    // Only process specs whose content has changed (added or updated);
    // skipped specs are already loaded with identical content.
    if !bundle.specs.is_empty() {
        let specs_to_bootstrap: Vec<(&str, &str)> = bundle
            .specs
            .iter()
            .filter(|(entity_type, _)| !skipped.contains(entity_type))
            .map(|(et, src)| (et.as_str(), src.as_str()))
            .collect();

        if !specs_to_bootstrap.is_empty() {
            let verified_cache = if let Some(ref store) = state.server.event_store
                && let Some(turso) = store.platform_turso_store()
            {
                turso
                    .load_verification_cache(tenant)
                    .await
                    .unwrap_or_default()
            } else if let Some(ref store) = state.server.event_store
                && let Some(ps) = store.platform_store()
            {
                ps.load_verification_cache(tenant).await.unwrap_or_default()
            } else {
                std::collections::BTreeMap::new()
            };

            if let Some(ref merged) = merged_csdl {
                bootstrap::bootstrap_tenant_specs(
                    state,
                    tenant,
                    merged,
                    &specs_to_bootstrap,
                    true,
                    &format!("OsApp({app_name})"),
                    &verified_cache,
                );
            }
        }
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

    // ── Step 4: Compile and register WASM modules. ──────────────────
    let mut wasm_registered = Vec::new();
    for (module_name, wasm_bytes) in &bundle.wasm_modules {
        match state.server.wasm_engine.compile_and_cache(wasm_bytes) {
            Ok(hash) => {
                // Persist to Turso FIRST for durability.
                if let Err(e) = state
                    .server
                    .upsert_wasm_module(tenant, module_name, wasm_bytes, &hash)
                    .await
                {
                    tracing::warn!(
                        tenant,
                        module = %module_name,
                        error = %e,
                        "Failed to persist WASM module to durable store (continuing in-memory only)"
                    );
                }
                // Register in module registry.
                {
                    let mut wasm_reg = state.server.wasm_module_registry.write().unwrap(); // ci-ok: infallible lock
                    wasm_reg.register(&tenant_id, module_name, &hash);
                }
                tracing::info!(
                    tenant,
                    module = %module_name,
                    hash = %hash,
                    size = wasm_bytes.len(),
                    "WASM module loaded from OS app"
                );
                wasm_registered.push(module_name.clone());
            }
            Err(e) => {
                tracing::warn!(
                    tenant,
                    module = %module_name,
                    error = %e,
                    "Failed to compile WASM module from OS app"
                );
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

    // ── Step 5: Bootstrap agents. ──────────────────────────────────────
    let agents_bootstrapped = bootstrap_agents(state, &tenant_id, tenant, &bundle.agents).await;

    // ── Step 6: Bootstrap skills. ────────────────────────────────────
    let skills_bootstrapped = bootstrap_skills(state, &tenant_id, tenant, &bundle.skills).await;

    // ── Step 7: Bootstrap ADRs into TemperFS. ────────────────────────
    let adrs_bootstrapped = bootstrap_adrs(state, &tenant_id, tenant, app_name, &bundle.adrs).await;

    // ── Step 8: Create seed instances. ───────────────────────────────
    let seed_created = bootstrap_seed_data(state, &tenant_id, tenant, &bundle.seed_instances).await;

    Ok(InstallResult {
        added,
        updated,
        skipped,
        wasm_modules: wasm_registered,
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

/// Bootstrap agent definitions into the tenant by creating Soul entities.
///
/// For each agent definition in the app's `agents/` directory:
/// 1. Check if the Soul entity type is registered (skip gracefully if not)
/// 2. Check if a Soul with this name already exists (idempotent)
/// 3. Create a TemperFS File entity with the agent's concatenated content
/// 4. Create a Soul entity pointing to that file
///
/// Returns the names of successfully bootstrapped agents.
async fn bootstrap_agents(
    state: &PlatformState,
    tenant_id: &TenantId,
    tenant: &str,
    agents: &[AgentDefinition],
) -> Vec<String> {
    if agents.is_empty() {
        return Vec::new();
    }

    // Check if Soul entity type is registered.
    let has_souls = {
        let registry = state.registry.read().unwrap(); // ci-ok: infallible lock
        registry.get_spec(tenant_id, "Soul").is_some()
    };
    if !has_souls {
        if !agents.is_empty() {
            tracing::info!(
                tenant,
                count = agents.len(),
                "Skipping agent bootstrap — Soul entity type not registered (install paw-agent first)"
            );
        }
        return Vec::new();
    }

    // Check if File entity type is registered (for TemperFS).
    let has_files = {
        let registry = state.registry.read().unwrap(); // ci-ok: infallible lock
        registry.get_spec(tenant_id, "File").is_some()
    };
    if !has_files {
        tracing::info!(
            tenant,
            "Skipping agent bootstrap — File entity type not registered (install temper-fs first)"
        );
        return Vec::new();
    }

    let agent_ctx = temper_server::request_context::AgentContext::system();
    let mut bootstrapped = Vec::new();

    for agent in agents {
        // Check if Soul already exists by listing and filtering.
        let existing_ids = state.server.list_entity_ids(tenant_id, "Soul");
        let mut already_exists = false;
        for id in &existing_ids {
            if let Ok(resp) = state
                .server
                .get_tenant_entity_state(tenant_id, "Soul", id)
                .await
                && let Some(name) = resp.state.fields.get("Name").and_then(|v| v.as_str())
                && name.eq_ignore_ascii_case(&agent.name)
            {
                if let Some(file_id) = resp
                    .state
                    .fields
                    .get("ContentFileId")
                    .or_else(|| resp.state.fields.get("content_file_id"))
                    .and_then(|v| v.as_str())
                    && let Err(error) = ensure_inline_file_uploaded(
                        state,
                        tenant_id,
                        &agent_ctx,
                        file_id,
                        &format!("{}.soul.md", agent.name.to_lowercase().replace(' ', "-")),
                        agent.content.as_bytes(),
                    )
                    .await
                {
                    tracing::warn!(
                        tenant,
                        agent = %agent.name,
                        file_id = %file_id,
                        error = %error,
                        "Failed to refresh existing agent soul content"
                    );
                }
                tracing::debug!(tenant, agent = %agent.name, "Soul already exists — skipping");
                already_exists = true;
                bootstrapped.push(agent.name.clone());
                break;
            }
        }
        if already_exists {
            continue;
        }

        // Create TemperFS File entity for the content.
        let file_name = format!("{}.soul.md", agent.name.to_lowercase().replace(' ', "-"));
        let file_id = format!(
            "app-soul-file-{}",
            agent.name.to_lowercase().replace(' ', "-")
        );
        if let Err(error) = ensure_inline_file_uploaded(
            state,
            tenant_id,
            &agent_ctx,
            &file_id,
            &file_name,
            agent.content.as_bytes(),
        )
        .await
        {
            tracing::warn!(
                tenant,
                agent = %agent.name,
                error = %error,
                "Failed to create TemperFS File for agent"
            );
            continue;
        }

        // Create Soul entity.
        let soul_id = format!("app-soul-{}", agent.name.to_lowercase().replace(' ', "-"));
        match state
            .server
            .get_or_create_tenant_entity(tenant_id, "Soul", &soul_id, serde_json::json!({}))
            .await
        {
            Ok(resp) => {
                if resp.state.status == "Draft" || resp.state.status == "Created" {
                    // Try to register the soul with metadata.
                    if let Err(e) = state
                        .server
                        .dispatch(temper_server::state::DispatchCommand {
                            tenant: tenant_id,
                            entity_type: "Soul",
                            entity_id: &soul_id,
                            action: "Create",
                            params: serde_json::json!({
                                "name": agent.name,
                                "description": agent.description,
                                "content_file_id": file_id,
                            }),
                            agent_ctx: &agent_ctx,
                            await_integration: false,
                        })
                        .await
                    {
                        tracing::warn!(
                            tenant,
                            agent = %agent.name,
                            error = %e,
                            "Failed to register Soul entity"
                        );
                        continue;
                    }
                    // Publish the soul.
                    let _ = state
                        .server
                        .dispatch(temper_server::state::DispatchCommand {
                            tenant: tenant_id,
                            entity_type: "Soul",
                            entity_id: &soul_id,
                            action: "Publish",
                            params: serde_json::json!({}),
                            agent_ctx: &agent_ctx,
                            await_integration: false,
                        })
                        .await;
                }
                tracing::info!(tenant, agent = %agent.name, "Agent soul bootstrapped");
                bootstrapped.push(agent.name.clone());
            }
            Err(e) => {
                tracing::warn!(
                    tenant,
                    agent = %agent.name,
                    error = %e,
                    "Failed to create Soul entity"
                );
            }
        }
    }
    bootstrapped
}

/// Bootstrap skill definitions into the tenant by creating Skill entities.
///
/// For each skill definition in the app's `skills/` directory:
/// 1. Check if the Skill entity type is registered
/// 2. Check if a Skill with this name already exists (idempotent)
/// 3. Create a TemperFS File entity with the skill content
/// 4. Create a Skill entity pointing to that file
///
/// Returns the names of successfully bootstrapped skills.
async fn bootstrap_skills(
    state: &PlatformState,
    tenant_id: &TenantId,
    tenant: &str,
    skills: &[AppSkillDefinition],
) -> Vec<String> {
    if skills.is_empty() {
        return Vec::new();
    }

    let has_skills = {
        let registry = state.registry.read().unwrap(); // ci-ok: infallible lock
        registry.get_spec(tenant_id, "Skill").is_some()
    };
    if !has_skills {
        tracing::info!(
            tenant,
            count = skills.len(),
            "Skipping skill bootstrap — Skill entity type not registered (install paw-agent first)"
        );
        return Vec::new();
    }

    let has_files = {
        let registry = state.registry.read().unwrap(); // ci-ok: infallible lock
        registry.get_spec(tenant_id, "File").is_some()
    };
    if !has_files {
        tracing::info!(
            tenant,
            "Skipping skill bootstrap — File entity type not registered (install temper-fs first)"
        );
        return Vec::new();
    }

    let agent_ctx = temper_server::request_context::AgentContext::system();
    let mut bootstrapped = Vec::new();

    for skill in skills {
        // Check if Skill already exists by name.
        let existing_ids = state.server.list_entity_ids(tenant_id, "Skill");
        let mut already_exists = false;
        for id in &existing_ids {
            if let Ok(resp) = state
                .server
                .get_tenant_entity_state(tenant_id, "Skill", id)
                .await
                && let Some(name) = resp.state.fields.get("Name").and_then(|v| v.as_str())
                && name.eq_ignore_ascii_case(&skill.name)
            {
                if let Some(file_id) = resp
                    .state
                    .fields
                    .get("ContentFileId")
                    .or_else(|| resp.state.fields.get("content_file_id"))
                    .and_then(|v| v.as_str())
                    && let Err(error) = ensure_inline_file_uploaded(
                        state,
                        tenant_id,
                        &agent_ctx,
                        file_id,
                        &format!("{}.skill.md", skill.name.to_lowercase().replace(' ', "-")),
                        skill.content.as_bytes(),
                    )
                    .await
                {
                    tracing::warn!(
                        tenant,
                        skill = %skill.name,
                        file_id = %file_id,
                        error = %error,
                        "Failed to refresh existing skill content"
                    );
                }
                tracing::debug!(tenant, skill = %skill.name, "Skill already exists — skipping");
                already_exists = true;
                bootstrapped.push(skill.name.clone());
                break;
            }
        }
        if already_exists {
            continue;
        }

        // Create TemperFS File for skill content.
        let file_id = format!(
            "app-skill-file-{}",
            skill.name.to_lowercase().replace(' ', "-")
        );
        let file_name = format!("{}.skill.md", skill.name.to_lowercase().replace(' ', "-"));
        if let Err(error) = ensure_inline_file_uploaded(
            state,
            tenant_id,
            &agent_ctx,
            &file_id,
            &file_name,
            skill.content.as_bytes(),
        )
        .await
        {
            tracing::warn!(
                tenant,
                skill = %skill.name,
                error = %error,
                "Failed to create TemperFS File for skill"
            );
            continue;
        }

        // Create Skill entity.
        let skill_id = format!("app-skill-{}", skill.name.to_lowercase().replace(' ', "-"));
        match state
            .server
            .get_or_create_tenant_entity(tenant_id, "Skill", &skill_id, serde_json::json!({}))
            .await
        {
            Ok(resp) => {
                if resp.state.status == "Active" || resp.state.status == "Created" {
                    // Register the skill with metadata.
                    let _ = state
                        .server
                        .dispatch(temper_server::state::DispatchCommand {
                            tenant: tenant_id,
                            entity_type: "Skill",
                            entity_id: &skill_id,
                            action: "Register",
                            params: serde_json::json!({
                                "name": skill.name,
                                "description": skill.description,
                                "content_file_id": file_id,
                                "scope": skill.scope,
                                "agent_filter": "",
                            }),
                            agent_ctx: &agent_ctx,
                            await_integration: false,
                        })
                        .await;
                }
                tracing::info!(tenant, skill = %skill.name, scope = %skill.scope, "Skill bootstrapped");
                bootstrapped.push(skill.name.clone());
            }
            Err(e) => {
                tracing::warn!(
                    tenant,
                    skill = %skill.name,
                    error = %e,
                    "Failed to create Skill entity"
                );
            }
        }
    }
    bootstrapped
}

async fn ensure_inline_file_uploaded(
    state: &PlatformState,
    tenant_id: &TenantId,
    agent_ctx: &temper_server::request_context::AgentContext,
    file_id: &str,
    file_name: &str,
    content: &[u8],
) -> Result<(), String> {
    let response = state
        .server
        .get_or_create_tenant_entity(tenant_id, "File", file_id, serde_json::json!({}))
        .await
        .map_err(|e| format!("failed to create File('{file_id}') actor: {e}"))?;

    if response.state.status == "Created" {
        state
            .server
            .dispatch(temper_server::state::DispatchCommand {
                tenant: tenant_id,
                entity_type: "File",
                entity_id: file_id,
                action: "Create",
                params: serde_json::json!({
                    "name": file_name,
                    "path": "",
                    "directory_id": "",
                    "workspace_id": "",
                    "mime_type": "text/markdown",
                }),
                agent_ctx,
                await_integration: false,
            })
            .await
            .map_err(|e| format!("failed to initialize File('{file_id}'): {e}"))?;
    }

    state
        .server
        .put_file_stream_content(tenant_id, file_id, content, "text/markdown", agent_ctx)
        .await
        .map_err(|e| format!("failed to upload File('{file_id}') content: {e}"))?;

    Ok(())
}

const APP_DOCS_WORKSPACE_ID: &str = "os-app-docs";
const APP_DOCS_WORKSPACE_NAME: &str = "apps";
const APP_DOCS_ROOT_DIR_ID: &str = "os-app-docs-root";
const APP_DOCS_ROOT_PATH: &str = "/apps";
const APP_DOCS_QUOTA_BYTES: i64 = 1_099_511_627_776;

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

    let agent_ctx = temper_server::request_context::AgentContext::system();
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

async fn ensure_app_docs_workspace(
    state: &PlatformState,
    tenant_id: &TenantId,
    agent_ctx: &temper_server::request_context::AgentContext,
) -> Result<(), String> {
    if !state
        .server
        .entity_exists(tenant_id, "Workspace", APP_DOCS_WORKSPACE_ID)
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

struct DirectoryBootstrapTarget<'a> {
    directory_id: &'a str,
    name: &'a str,
    path: &'a str,
    parent_id: Option<&'a str>,
    workspace_id: &'a str,
}

async fn ensure_directory(
    state: &PlatformState,
    tenant_id: &TenantId,
    agent_ctx: &temper_server::request_context::AgentContext,
    target: DirectoryBootstrapTarget<'_>,
) -> Result<(), String> {
    if state
        .server
        .entity_exists(tenant_id, "Directory", target.directory_id)
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

struct MarkdownFileBootstrapTarget<'a> {
    file_id: &'a str,
    name: &'a str,
    path: &'a str,
    directory_id: &'a str,
    workspace_id: &'a str,
}

async fn ensure_markdown_file(
    state: &PlatformState,
    tenant_id: &TenantId,
    agent_ctx: &temper_server::request_context::AgentContext,
    target: MarkdownFileBootstrapTarget<'_>,
    content: &[u8],
) -> Result<(), String> {
    let existed = state
        .server
        .entity_exists(tenant_id, "File", target.file_id);
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

fn slug_fragment(value: &str) -> String {
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

    let agent_ctx = temper_server::request_context::AgentContext::system();
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
