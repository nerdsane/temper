use std::collections::BTreeMap;

use serde::Serialize;

use super::{AdrEntry, AgentDefinition, AppSkillDefinition, SeedInstance, SystemFileEntry};

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
    /// WASM modules intentionally skipped (for example, optional modules
    /// without a bundled artifact).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub wasm_skipped: Vec<String>,
    /// WASM modules that failed validation or eager warm-up.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub wasm_failures: Vec<String>,
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
    pub startup_install: StartupInstallMode,
    #[serde(default)]
    pub dependencies: Vec<String>,
    #[serde(default)]
    pub wasm_modules: Vec<WasmModuleManifest>,
}

fn default_version() -> String {
    "0.1.0".to_string()
}

/// Whether an app should be installed during the default OpenPaw startup path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum StartupInstallMode {
    /// The app is required for the core platform boot surface.
    Core,
    /// The app is available for install, but not part of the default boot path.
    #[default]
    Manual,
}

/// Whether a WASM module should be eagerly compiled when its app is installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum WasmStartupLoading {
    /// Compile immediately when the app is installed.
    Eager,
    /// Register and persist only; compile lazily on first invoke.
    #[default]
    Lazy,
}

/// Capability criticality for a WASM module in an app bundle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum WasmModuleCriticality {
    PlatformRequired,
    AppRequired,
    #[default]
    Optional,
}

/// Manifest-declared WASM module contract.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct WasmModuleManifest {
    pub name: String,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub criticality: WasmModuleCriticality,
    #[serde(default)]
    pub startup_loading: WasmStartupLoading,
    #[serde(default)]
    pub provenance: Option<String>,
    #[serde(default)]
    pub import_class: Option<String>,
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
    /// Whether the app belongs to the default startup install surface.
    #[serde(default)]
    pub startup_install: StartupInstallMode,
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
    /// Optional tenant-scoped cross-invariants source.
    pub cross_invariants_toml: Option<String>,
    /// Cedar policy sources (may be empty).
    pub cedar_policies: Vec<String>,
    /// Reaction rules loaded from the app's `reactions/reactions.toml`
    /// (or `specs/reactions.toml`) if present. Empty when the app uses
    /// ADR-0046 inline `[[action.triggers]]` exclusively. Closes the
    /// ADR-0045 install-path bug where `AppBundle` had no reactions
    /// field and `install_os_app` silently dropped reactions on Railway.
    pub reactions: Vec<temper_server::reaction::ReactionRule>,
    /// WASM module binaries as `(module_name, wasm_bytes)` pairs.
    pub wasm_modules: BTreeMap<String, Vec<u8>>,
    /// WASM module contracts declared in `app.toml`.
    pub wasm_module_configs: BTreeMap<String, WasmModuleManifest>,
    /// Agent definitions discovered from `agents/` subdirectories.
    pub agents: Vec<AgentDefinition>,
    /// Skill definitions discovered from `skills/` subdirectories.
    pub skills: Vec<AppSkillDefinition>,
    /// ADR markdown files discovered from `adrs/`.
    pub adrs: Vec<AdrEntry>,
    /// System files discovered from `system/` directory tree.
    pub system_files: Vec<SystemFileEntry>,
    /// Seed data instances discovered from `seed-data/` TOML files.
    pub seed_instances: Vec<SeedInstance>,
}
