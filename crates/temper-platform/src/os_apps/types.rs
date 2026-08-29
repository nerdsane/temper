use std::collections::BTreeMap;

use serde::Serialize;
use temper_wasm_sdk::data::{ModuleDataGrant, ModuleSdkManifest};

use super::{AdrEntry, AgentDefinition, AppSkillDefinition, SeedInstance, SystemFileEntry};

/// Result of an app installation, categorising each spec by what happened.
#[derive(Debug, Clone, Default, Serialize)]
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

/// Component phases an OS-app install/reconcile should execute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OsAppInstallPlan {
    pub specs: bool,
    pub policies: bool,
    pub wasm: bool,
    pub content: bool,
    pub seed: bool,
}

impl OsAppInstallPlan {
    pub(crate) const fn all() -> Self {
        Self {
            specs: true,
            policies: true,
            wasm: true,
            content: true,
            seed: true,
        }
    }
}

/// Stable digest breakdown for an OS app bundle.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct OsAppBundleDigest {
    pub app_name: String,
    pub app_version: String,
    pub bundle_digest: String,
    pub spec_digest: String,
    pub policy_digest: String,
    pub wasm_digest: String,
    pub content_digest: String,
    pub seed_digest: String,
}

/// Outcome of digest-aware app reconcile.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum OsAppReconcileResult {
    Skipped {
        app_name: String,
        bundle_digest: String,
    },
    Installed {
        app_name: String,
        bundle_digest: String,
        install: Box<InstallResult>,
    },
    /// Spec activation is fenced until the governed kernel migration completes.
    MigrationRequired {
        app_name: String,
        semantic_digest: String,
        capability_digest: String,
        descriptor_contract_version: u16,
    },
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
    pub mode: AppDeploymentMode,
    #[serde(default)]
    pub startup_install: StartupInstallMode,
    #[serde(default)]
    pub dependencies: Vec<String>,
    #[serde(default)]
    pub wasm_modules: Vec<WasmModuleManifest>,
}

impl AppManifest {
    /// Validate a pre-compilation candidate manifest.
    ///
    /// Candidate builds may not have a final artifact binding yet. Every
    /// declared grant is still validated before code generation.
    pub fn validate_candidate(&self) -> Result<(), String> {
        let mut module_names = std::collections::BTreeSet::new();
        for module in &self.wasm_modules {
            if module.name.trim().is_empty() {
                return Err("WASM module name must not be empty".into());
            }
            if !module_names.insert(module.name.as_str()) {
                return Err(format!("duplicate WASM module '{}'", module.name));
            }
            if let Some(data) = &module.data {
                data.validate()
                    .map_err(|error| format!("WASM module '{}': {error}", module.name))?;
            } else if module.data_binding.is_some() {
                return Err(format!(
                    "WASM module '{}' data_binding requires a data grant",
                    module.name
                ));
            }
        }
        Ok(())
    }

    /// Validate manifest invariants before any bundle contents are installed.
    pub fn validate(&self) -> Result<(), String> {
        self.validate_candidate()?;
        for module in &self.wasm_modules {
            if module.data.is_some() {
                let binding = module.data_binding.as_ref().ok_or_else(|| {
                    format!(
                        "WASM module '{}' data grant requires data_binding",
                        module.name
                    )
                })?;
                binding
                    .verify_binding()
                    .map_err(|error| format!("WASM module '{}': {error}", module.name))?;
                if binding.module_name != module.name {
                    return Err(format!(
                        "WASM module '{}' data binding does not match its declaration",
                        module.name
                    ));
                }
            }
        }
        Ok(())
    }
}

fn default_version() -> String {
    "0.1.0".to_string()
}

/// Deployment mode for app-local policy overlays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum AppDeploymentMode {
    /// Single-operator install with the app's normal policy surface.
    #[default]
    Operator,
    /// Public commons install with extra guardrail policies.
    Commons,
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

impl WasmModuleCriticality {
    pub fn is_required(self) -> bool {
        self != Self::Optional
    }
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
    /// Least-authority application-data capabilities granted to this module.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<ModuleDataGrant>,
    /// Host-readable SDK binding covered by the compiled artifact identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_binding: Option<ModuleSdkManifest>,
}

impl WasmModuleManifest {
    pub fn is_required(&self) -> bool {
        self.criticality.is_required()
    }
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

/// Cedar policy source bundled with an OS app.
#[derive(Debug, Clone)]
pub struct CedarPolicySource {
    /// App-relative source path, for example `policies/default.cedar`.
    pub relative_path: String,
    /// Raw Cedar policy text.
    pub text: String,
}

/// Full spec bundle for an app (owned, loaded from disk).
pub struct AppBundle {
    /// Effective deployment mode used while loading the bundle.
    pub deployment_mode: AppDeploymentMode,
    /// IOA spec sources as `(entity_type, ioa_toml_source)` pairs.
    pub specs: Vec<(String, String)>,
    /// CSDL XML source (None if app has no IOA specs).
    pub csdl: Option<String>,
    /// Optional tenant-scoped cross-invariants source.
    pub cross_invariants_toml: Option<String>,
    /// Cedar policy sources (may be empty).
    pub cedar_policies: Vec<String>,
    /// Cedar policy sources with their app-relative paths for durable row IDs.
    pub cedar_policy_sources: Vec<CedarPolicySource>,
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
