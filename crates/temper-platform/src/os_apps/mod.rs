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

use serde::Serialize;

mod agent_bootstrap;
mod app_catalog;
mod bootstrap;
mod bundle;
mod discovery;
pub mod git_sources;
mod install;
mod reconcile;
mod runtime_heal;
mod system_files;
mod types;
pub(super) use app_catalog::catalog;
pub use app_catalog::{
    add_os_apps_dir, list_startup_os_apps, reload_os_apps, reload_skills, set_os_apps_dir,
    set_skills_dir,
};
use bootstrap::{
    APP_DOCS_ROOT_DIR_ID, APP_DOCS_WORKSPACE_ID, DirectoryBootstrapTarget,
    MarkdownFileBootstrapTarget, content_sha256, ensure_app_docs_workspace, ensure_directory,
    ensure_markdown_file, slug_fragment, state_field_str,
};
use bundle::{load_app_bundle, os_app_dependencies};
use discovery::{extract_description, read_app_guide, read_app_manifest};
use install::install_os_app_with_plan;
pub use install::{install_os_app, install_skill};
pub use reconcile::{os_app_bundle_digest, reconcile_os_app, resolve_os_app_install_order};
pub(crate) use runtime_heal::{
    restore_app_specs_from_matching_digest, tenant_has_ready_app_specs_for_bundle,
};
pub use types::*;

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

// Backward-compatible alias: SkillBundle → AppBundle.
pub type SkillBundle = AppBundle;

// Backward-compatible type aliases.
pub type OsAppEntry = AppEntry;
pub type OsAppBundle = AppBundle;

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

#[cfg(test)]
mod mod_test;
