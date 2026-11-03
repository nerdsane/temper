//! OS App Catalog — dev/local pre-built application specs.
//!
//! OS apps are spec bundles (IOA TOML + CSDL + Cedar policies) loaded from
//! the `os-apps/` directory in local development and tests. Agents discover them via
//! `list_os_apps()` / `install_os_app()`.
//!
//! Install reuses [`crate::bootstrap::bootstrap_tenant_specs`] so every app goes through the same verification cascade as system specs.

use std::collections::BTreeMap;
use std::ops::ControlFlow;
use std::path::Path;

use serde::Serialize;
use temper_runtime::tenant::TenantId;
use temper_spec::automaton;
use temper_spec::csdl::emit_csdl_xml;

use crate::bootstrap;
use crate::state::PlatformState;

mod agent_bootstrap;
mod app_catalog;
mod app_files;
mod apps_dir;
mod bundle_files;
mod bundle_modules;
mod closure_bootstrap;
mod entity_aliases;
mod entity_owners;
mod file_source;
mod install_checks;
pub(crate) mod install_guard;
mod install_lines;
mod materialized_closure;
mod module_review_step;
mod own_schema;
mod policy_rows;
mod proofs;
mod reconcile;
mod runtime_heal;
mod system_files;
mod types;
mod wasm_replacement;
pub(super) use app_catalog::catalog;
pub use app_catalog::{
    add_os_apps_dir, add_tenant_written_apps, add_written_apps_preferred, get_os_app, list_os_apps,
    list_startup_os_apps, list_tenant_os_apps, reload_os_apps, set_os_apps_dir, tenant_app_guide,
    tenant_os_app,
};
pub(crate) use app_catalog::{load_tenant_os_app, tenant_os_app_dir};
pub use app_files::{AppFiles, Shadowed, app_dir_of};
pub(crate) use bundle_files::find_ioa_files;
use bundle_files::{find_adrs, find_agents, find_app_skills, find_seed_data, find_wasm_modules};
use bundle_modules::{BundleModules, register_bundle_modules};
pub use closure_bootstrap::{
    ClosureBootstrapResult, OS_APP_CLOSURE_RESOLVER_VERSION, OsAppClosure,
    bootstrap_closure_manifest, os_app_closure_for_roots, parse_bootstrap_manifest_str,
    startup_os_app_closure,
};
use entity_aliases::{
    content_sha256, ensure_created_file_initialized, ensure_entity_field_aliases,
    file_already_contains, slug_fragment,
};
pub use entity_owners::unspecified_entity_types;
#[cfg(test)]
pub(crate) use file_source::seam as app_files_seam;
pub use file_source::{AppFileSource, list_install_dir};
pub use install_lines::{installed_line, stored_line, waiting_line};
pub(crate) use materialized_closure::resolve_materialized_app_closure;
pub use module_review_step::ReviewAsked;
pub(crate) use own_schema::converge_installed_schemas;
#[cfg(test)]
pub(super) use policy_rows::os_app_policy_row_id;
pub(crate) use reconcile::{
    InstallOrigin, reconcile_os_app_swapping, tenant_has_active_policies_for_bundle,
    tenant_has_registered_wasm_for_bundle,
};
pub use reconcile::{
    os_app_bundle_digest, reconcile_os_app, resolve_os_app_install_order,
    resolve_tenant_os_app_install_order, tenant_os_app_bundle_digest, written_apps_bundle_digests,
};
pub(crate) use runtime_heal::{
    restore_app_specs_from_matching_digest, tenant_has_ready_app_specs_for_bundle,
};
pub use types::*;
use wasm_replacement::{UploadedWasmReplacementContext, uploaded_wasm_replacement_context};

pub(crate) fn read_app_manifest(app_dir: &Path) -> Option<AppManifest> {
    let path = app_dir.join("app.toml");
    let content = std::fs::read_to_string(&path).ok()?;
    toml::from_str(&content).ok()
}

fn cached_or_active_tenant_policy_text(state: &PlatformState, tenant: &str) -> String {
    if let Some(active_text) = state
        .server
        .authz
        .get_tenant_policy_text(tenant)
        .filter(|policy_text| !policy_text.trim().is_empty())
    {
        return active_text;
    }

    state
        .server
        .tenant_policies
        .read()
        .ok()
        .and_then(|policies| policies.get(tenant).cloned())
        .unwrap_or_default()
}

fn merge_bundle_policies(existing: &str, cedar_policies: &[String]) -> String {
    let mut policy_text = existing.trim_end().to_string();
    for policy in cedar_policies {
        let policy = policy.trim();
        if policy.is_empty() || policy_text.contains(policy) {
            continue;
        }
        if !policy_text.is_empty() {
            policy_text.push('\n');
        }
        policy_text.push_str(policy);
    }
    policy_text
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

// Backward-compatible type aliases.
pub type OsAppEntry = AppEntry;
pub type OsAppBundle = AppBundle;

fn app_relative_path(app_dir: &Path, path: &Path) -> String {
    path.strip_prefix(app_dir)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn effective_app_deployment_mode(manifest: &AppManifest) -> AppDeploymentMode {
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

/// Read the app guide markdown (APP.md/app.md first, then skill.md/SKILL.md fallback).
pub(crate) fn read_app_guide(app_dir: &Path) -> Option<String> {
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

/// Load a complete app bundle from its files, or say why not. A file the
/// app has that cannot be read is an error naming it.
fn load_app_bundle(files: &AppFileSource) -> Result<AppBundle, String> {
    let app_dir = files.dir();
    let manifest = read_app_manifest(app_dir).ok_or("its app.toml is missing or invalid")?;
    let deployment_mode = effective_app_deployment_mode(&manifest);
    let legacy_reaction_paths = ["reactions/reactions.toml", "specs/reactions.toml"];
    if let Some(path) = legacy_reaction_paths.iter().find(|path| files.has(path)) {
        tracing::error!(
            path = %app_dir.join(path).display(),
            "legacy reactions.toml is no longer supported; migrate this app to inline [[action.triggers]]"
        );
        return Err("legacy reactions.toml is no longer supported".to_string());
    }

    let selected = AppFiles::from_source(files);

    // Read IOA specs, extracting entity type from the parsed automaton name.
    let mut specs = Vec::new();
    for path in &selected.ioa {
        let path = app_relative_path(app_dir, path);
        let source = files.read_text(&path)?;
        let parsed = automaton::parse_automaton(&source)
            .map_err(|error| format!("{path} is refused: {error}"))?;
        specs.push((parsed.automaton.name, source));
    }

    // Read CSDL (optional: apps without specs won't have CSDL).
    let csdl = selected
        .csdl
        .as_ref()
        .map(|path| files.read_text(&app_relative_path(app_dir, path)))
        .transpose()?;
    let cross_invariants = "specs/cross-invariants.toml";
    let cross_invariants_toml = if files.has(cross_invariants) {
        Some(files.read_text(cross_invariants)?)
    } else {
        None
    };

    // Read Cedar policies.
    let mut cedar_policy_sources = Vec::new();
    for path in &selected.policies {
        let relative_path = app_relative_path(app_dir, path);
        let text = files.read_text(&relative_path)?;
        cedar_policy_sources.push(CedarPolicySource {
            text: policy_rows::labelled_policy_text(&manifest.name, &relative_path, text),
            relative_path,
        });
    }
    let cedar_policies: Vec<String> = cedar_policy_sources
        .iter()
        .map(|source| source.text.clone())
        .collect();

    // Build module configs first so find_wasm_modules can use declared targets.
    let wasm_module_configs: BTreeMap<String, WasmModuleManifest> = manifest
        .wasm_modules
        .into_iter()
        .map(|module| (module.name.clone(), module))
        .collect();

    // Read WASM module binaries, respecting declared targets from app.toml.
    let wasm_modules = find_wasm_modules(files, &wasm_module_configs)?;

    // Discover agents, skills, and seed data.
    let agents = find_agents(files)?;
    let skills = find_app_skills(files)?;
    let adrs = find_adrs(files)?;
    let system_files = system_files::find_system_files(files)?;
    let seed_instances = find_seed_data(files)?;

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
        return Err("it holds nothing to install".to_string());
    }

    Ok(AppBundle {
        deployment_mode,
        specs,
        csdl,
        cross_invariants_toml,
        cedar_policies,
        cedar_policy_sources,
        wasm_modules,
        wasm_module_configs,
        agents,
        skills,
        adrs,
        system_files,
        seed_instances,
    })
}

fn os_app_dependencies(tenant: Option<&str>, name: &str) -> Vec<String> {
    // Check manifest first.
    let cat = catalog().read().unwrap(); // ci-ok: infallible lock
    dependencies_for_entry(name, cat.app_entry(tenant, name))
}

fn dependencies_for_entry(name: &str, entry: Option<&AppEntry>) -> Vec<String> {
    if let Some(entry) = entry
        && !entry.dependencies.is_empty()
    {
        return entry
            .dependencies
            .iter()
            .filter_map(|dependency| local_os_app_dependency_name(dependency))
            .collect();
    }
    // Hardcoded fallback.
    match name {
        // TemperAgent persists conversation/files in TemperFS entities.
        "temper-agent" => vec!["temper-fs".to_string()],
        _ => vec![],
    }
}

pub(super) fn local_os_app_dependency_name(dependency: &str) -> Option<String> {
    let unpinned = dependency
        .trim()
        .split_once('@')
        .map_or(dependency.trim(), |(left, _)| left);
    let name = unpinned
        .rsplit_once('/')
        .map_or(unpinned, |(_, name)| name)
        .trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
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
    install_os_app_and_dependencies(state, tenant, app_name, InstallOrigin::Request).await
}

/// Replay a stored install of `app_name` and its dependencies at boot.
///
/// It runs the same installer as [`install_os_app`], but a replay is not a new
/// install: it leaves the stored record's origin (Genesis registry and version,
/// or local) as recorded, so any number of restarts converge on the same record.
pub async fn restore_os_app(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
) -> Result<InstallResult, String> {
    install_os_app_and_dependencies(state, tenant, app_name, InstallOrigin::Replay).await
}

async fn install_os_app_and_dependencies(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
    origin: InstallOrigin,
) -> Result<InstallResult, String> {
    let order = resolve_tenant_os_app_install_order(tenant, &[app_name.to_string()])?;

    let mut final_result = None;
    for app in order {
        let result =
            install_os_app_with_plan(state, tenant, &app, OsAppInstallPlan::all(), origin).await?;
        result.publish_reviews();
        if app == app_name {
            final_result = Some(result);
        }
    }
    final_result.ok_or_else(|| format!("OS app '{app_name}' produced no install result"))
}

pub(super) async fn install_os_app_with_plan(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
    plan: OsAppInstallPlan,
    origin: InstallOrigin,
) -> Result<InstallResult, String> {
    install_os_app_swapping(
        state,
        tenant,
        app_name,
        plan,
        origin,
        Swap::for_origin(origin),
    )
    .await
}

/// Which half of a new app version goes in first: its WASM modules or its
/// specs. An integration's call is fixed by the spec its action ran under,
/// and its module is looked up by name only when the call runs, so the newer
/// module must be in place whenever the newer spec can run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Swap {
    /// A new version: its modules go in, then its specs. Until the specs go
    /// live, the old spec's calls reach the new module.
    ModulesFirst,
    /// An older version put back, after a failed install or at boot: its
    /// specs go back, then its modules, so the newer module answers both
    /// specs meanwhile. A call the newer spec had already started when the
    /// modules go back still reaches the older module.
    SpecsFirst,
}

impl Swap {
    /// A request installs a new version; a replay puts a recorded one back.
    pub(crate) fn for_origin(origin: InstallOrigin) -> Self {
        match origin {
            InstallOrigin::Request => Self::ModulesFirst,
            InstallOrigin::Replay => Self::SpecsFirst,
        }
    }
}

/// [`install_os_app_with_plan`] with the order its modules and specs go in.
pub(super) async fn install_os_app_swapping(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
    plan: OsAppInstallPlan,
    origin: InstallOrigin,
    swap: Swap,
) -> Result<InstallResult, String> {
    // A proof that is still walking is waited for without the tenant's
    // install lock; the install then starts again under it and finds the
    // verdict in `attempt`, or a new walk when the tenant changed meanwhile.
    let mut attempt = proofs::Attempt::default();
    loop {
        match install_once(state, tenant, app_name, plan, origin, swap, &attempt).await? {
            ControlFlow::Break(result) => return Ok(result),
            ControlFlow::Continue(proof) => proof.ended(&mut attempt).await?,
        }
    }
}

/// One try at an install: done, or the proof it must wait for. Everything
/// before the proof only reads, so a try that waits leaves nothing behind.
async fn install_once(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
    plan: OsAppInstallPlan,
    origin: InstallOrigin,
    swap: Swap,
    attempt: &proofs::Attempt,
) -> Result<ControlFlow<InstallResult, proofs::Wait>, String> {
    install_checks::refuse_unavailable_tenant(state, tenant, app_name)?;
    let mut install = install_guard::tenant(&state.server, tenant).await;
    let app_files = {
        let cat = catalog().read().unwrap(); // ci-ok: infallible lock
        cat.app_files(Some(tenant), app_name)
            .cloned()
            .ok_or_else(|| {
                format!("OS app '{app_name}' not found in catalog for tenant '{tenant}'")
            })?
    };
    let app_dir = app_files.dir();
    let bundle = load_app_bundle(&app_files).map_err(|why| {
        format!(
            "OS app '{app_name}' is registered at '{}' but its bundle failed to load: {why}",
            app_dir.display()
        )
    })?;
    if plan.specs && origin == InstallOrigin::Request {
        install_checks::trigger_target_args(app_name, &bundle.specs)?;
    }
    if origin == InstallOrigin::Replay {
        reconcile::ensure_replay_bundle_is_recorded(state, tenant, app_name, &bundle).await?;
    }
    // A bundle without the binary of a required module is refused before any
    // spec, policy or install row is written, rather than installing an app
    // that answers 503 at request time (ARN-61/ARN-273).
    let missing = reconcile::missing_required_wasm_modules(&bundle);
    if plan.wasm && !missing.is_empty() {
        return Err(format!(
            "Refusing to activate os-app '{app_name}' for tenant '{tenant}': \
             required WASM module artifact(s) missing from the app bundle: {}. \
             The published bundle must include a compiled binary for every module \
             declared platform-required/app-required in app.toml.",
            missing.join(", ")
        ));
    }
    // A spec its own app does not declare, or an entity set another app
    // already declares, serves one app's records under the other's schema.
    // A request that would newly do so is refused before any write. A pair
    // the tenant holds from before goes ahead with a warning, and a boot
    // replay only logs, since refusing either would take a running app away.
    let ownership = if plan.specs {
        entity_owners::ownership(state, tenant, app_name, &bundle).await?
    } else {
        entity_owners::Ownership::default()
    };
    if !ownership.refused.is_empty() {
        let problems = ownership.refused.join("; ");
        if origin == InstallOrigin::Request {
            return Err(format!(
                "Refusing to install os-app '{app_name}' for tenant '{tenant}': {problems}"
            ));
        }
        tracing::warn!(tenant, app = %app_name, problems, "replayed app has entity ownership problems");
    }
    for standing in &ownership.standing {
        tracing::warn!(tenant, app = %app_name, "{standing}");
    }
    if bundle.deployment_mode == AppDeploymentMode::Commons {
        state.server.enable_commons_guardrails(tenant);
    }
    let tenant_id = TenantId::new(tenant);
    let upload_replacement = if plan.wasm && !bundle.wasm_modules.is_empty() {
        uploaded_wasm_replacement_context(state, tenant, app_name, &bundle).await
    } else {
        UploadedWasmReplacementContext::default()
    };

    if bundle.adrs.is_empty() {
        tracing::warn!(
            tenant,
            app = %app_name,
            "App installed with no ADRs; add adrs/*.md to record design decisions"
        );
    }

    // The tenant's schema once this app's own contribution is replaced; only
    // when the reconcile plan needs spec work.
    let own = match bundle.csdl.as_deref() {
        Some(csdl) if plan.specs => {
            Some(own_schema::for_install(state, tenant, app_name, csdl).await?)
        }
        _ => None,
    };
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
        // Parsed before any write, with the CSDL below (see `install_checks`).
        install_checks::parse_cross_invariants(bundle.cross_invariants_toml.as_deref(), app_name)?;
        // The tenant's whole schema, computed above from every installed app.
        let merged_csdl = match &own {
            Some(own) => Some(own.csdl_xml.clone()),
            // No CSDL in bundle; keep existing if any.
            None => registry
                .get_tenant(&tenant_id)
                .map(|t| emit_csdl_xml(&t.csdl)),
        };
        (added, updated, skipped, merged_csdl)
    } else {
        (Vec::new(), Vec::new(), Vec::new(), None)
    };
    // Sort for deterministic output.
    added.sort();
    updated.sort();
    skipped.sort();
    let new_specs = plan.specs && !(added.is_empty() && updated.is_empty());

    // Build the full Cedar policy text for this tenant: the live text without
    // what this app's previous install wrote, plus the bundle's policies.
    let superseded = if plan.policies {
        policy_rows::superseded_app_policies(state, tenant, app_name, &bundle).await?
    } else {
        policy_rows::SupersededAppPolicies::default()
    };
    let combined_policy =
        if plan.policies && (!bundle.cedar_policies.is_empty() || superseded.has_statements()) {
            let existing = cached_or_active_tenant_policy_text(state, tenant);
            Some(superseded.replace_in(state, &existing, &bundle)?)
        } else {
            None
        };
    install_guard::refuse_uncompilable(
        &state.server.authz,
        app_name,
        tenant,
        combined_policy.as_deref(),
    )?;
    let policies = combined_policy.as_deref();
    // A carry's then move is asked of the policies the install serves with.
    let mut warnings = install_checks::check_spec_changes(
        state,
        &tenant_id,
        app_name,
        &bundle.specs,
        plan.specs,
        policies,
    )
    .await?;
    if plan.specs {
        warnings.extend(entity_owners::unrun_declarations(
            &app_files, app_name, &bundle,
        )?);
    }
    warnings.extend(own.into_iter().flat_map(|own| own.warnings));
    warnings.extend(
        ownership
            .standing
            .into_iter()
            .map(InstallWarning::entity_ownership),
    );
    let specs = &bundle.specs;
    let checked = install_checks::model_warnings(
        state, tenant, app_name, specs, new_specs, policies, attempt,
    );
    match checked.await? {
        ControlFlow::Continue(proof) => return Ok(ControlFlow::Continue(proof)),
        ControlFlow::Break(found) => warnings.extend(found),
    }
    // Jev reads the changed modules beside the install's writes, and goes on
    // after it answers. It starts once no proof is left to wait for, since a
    // try that waits drops it, and its paid calls with it.
    let review =
        module_review_step::start(state, tenant, app_name, app_dir, &bundle, plan.wasm).await;
    if plan.policies {
        policy_rows::rewrite_primary_snapshot(state, tenant, &superseded).await?;
    }

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
        if plan.specs
            && let Some(ref merged) = merged_csdl
        {
            // Commit exactly this app's rows, not another install's.
            let hashes: Vec<(&str, String)> = bundle
                .specs
                .iter()
                .map(|(et, ioa)| (et.as_str(), temper_store_turso::spec_content_hash(ioa)))
                .collect();
            let versions: Vec<(&str, &str, &str)> = hashes
                .iter()
                .map(|(et, hash)| (*et, hash.as_str(), merged.as_str()))
                .collect();
            if !ps
                .commit_spec_versions(tenant, &versions, &[])
                .await
                .map_err(|e| format!("Failed to commit specs: {e}"))?
            {
                return Err("Failed to commit specs: another install replaced them".to_string());
            }
        }
    }

    if plan.policies {
        policy_rows::persist_bundle_policy_rows(state, tenant, app_name, &bundle, &superseded)
            .await?;
        policy_rows::delete_dropped_rows(state, tenant, &superseded).await?;
    }

    // ── Modules first, unless the specs go first (see Swap). ───────
    // An integration's call is fixed by the spec its action ran under, and its
    // module is looked up by name only when the call runs: a new version's
    // spec live before its module calls the old binary with what only the new
    // one understands, and the record it moved breaks.
    let modules = if plan.wasm && swap == Swap::ModulesFirst {
        Some(register_bundle_modules(state, tenant, &tenant_id, &bundle, &upload_replacement).await)
    } else {
        None
    };

    // ── Step 2: Bootstrap into memory (verification + registry). ────
    // Only process specs whose content has changed (added or updated);
    // skipped specs are already loaded with identical content.
    if plan.specs && !bundle.specs.is_empty() {
        let specs_to_bootstrap: Vec<(&str, &str)> = bundle
            .specs
            .iter()
            .map(|(et, src)| (et.as_str(), src.as_str()))
            .collect();
        let verified_cache = bootstrap::load_verification_cache(state, tenant).await;

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
                    // `merged` is the tenant's whole schema, app by app.
                    csdl_mode: bootstrap::CsdlMode::Whole,
                    label: &format!("OsApp({app_name})"),
                    verified_cache: &verified_cache,
                    cross_invariants_source: bundle.cross_invariants_toml.as_deref(),
                    verification_mode: bootstrap::BootstrapSpecVerificationMode::TrustBundle,
                },
            )?;

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
    }

    // Installed specs can change triggers and cross-entity reactions: refresh the dispatcher.
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
            return Err(format!("os-app '{app_name}' Cedar did not load: {e}"));
        } else {
            let mut policies = state.server.tenant_policies.write().unwrap(); // ci-ok: infallible lock
            policies.insert(tenant.to_string(), policy_text.clone());
        }
    }

    // ── Step 4: modules, when the specs went first (Swap::SpecsFirst). ──
    let modules = match modules {
        Some(modules) => modules,
        None if plan.wasm => {
            register_bundle_modules(state, tenant, &tenant_id, &bundle, &upload_replacement).await
        }
        None => BundleModules::default(),
    };
    let BundleModules {
        registered: wasm_registered,
        skipped: wasm_skipped,
        failures: wasm_failures,
    } = modules;

    // Overdue timers may fire at once: reconcile only now that policies and modules are in.
    install.installed(&state.server, plan.specs);

    tracing::info!(
        "Installed os-app '{app_name}' for tenant '{tenant}': added={added:?} updated={updated:?} skipped={skipped:?} wasm={wasm_registered:?}"
    );

    // ARN-68: re-key changed-key-set types after registration (boot-race fix; see the helper + ADR-0153).
    if plan.specs && (!added.is_empty() || !updated.is_empty()) {
        reconcile::spawn_key_index_rekey_after_spec_change(state, &tenant_id);
    }

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

    reconcile::record_app_install_metadata_for_bundle(state, tenant, app_name, &bundle, origin)
        .await;
    let (reviewing, review_answers) = review.left_running();

    Ok(ControlFlow::Break(InstallResult {
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
        warnings,
        modules_reviewed: Vec::new(),
        reviewing,
        review_answers,
    }))
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
        cat.app_entry(Some(tenant), app_name).cloned()
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
                await_reactions: true,
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
                    await_reactions: true,
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
                await_reactions: true,
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
        ensure_entity_field_aliases(
            state,
            tenant_id,
            "Directory",
            target.directory_id,
            serde_json::json!({
                "Name": target.name,
                "Path": target.path,
                "ParentId": target.parent_id,
                "WorkspaceId": target.workspace_id,
            }),
        )
        .await?;
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
                "Name": target.name,
                "path": target.path,
                "Path": target.path,
                "parent_id": target.parent_id,
                "ParentId": target.parent_id,
                "workspace_id": target.workspace_id,
                "WorkspaceId": target.workspace_id,
            }),
            agent_ctx,
            await_integration: false,
            await_reactions: true,
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
                await_reactions: true,
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
    let create_fields = serde_json::json!({
        "name": target.name,
        "Name": target.name,
        "path": target.path,
        "Path": target.path,
        "directory_id": target.directory_id,
        "DirectoryId": target.directory_id,
        "workspace_id": target.workspace_id,
        "WorkspaceId": target.workspace_id,
        "mime_type": "text/markdown",
        "MimeType": "text/markdown",
    });
    let canonical_fields = serde_json::json!({
        "Name": target.name,
        "Path": target.path,
        "DirectoryId": target.directory_id,
        "WorkspaceId": target.workspace_id,
        "MimeType": "text/markdown",
    });
    let existed = state
        .server
        .ensure_entity_loaded(tenant_id, "File", target.file_id)
        .await;
    if !existed {
        state
            .server
            .create_file_with_initial_stream_content(
                tenant_id,
                target.file_id,
                create_fields.clone(),
                content,
                "text/markdown",
                agent_ctx,
            )
            .await
            .map_err(|e| {
                format!(
                    "failed to create File('{}') with initial content: {e}",
                    target.file_id
                )
            })?;
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
                await_reactions: true,
            })
            .await
            .map_err(|e| {
                format!(
                    "failed to register file '{}' with parent directory: {e}",
                    target.file_id
                )
            })?;
        return Ok(());
    }

    ensure_created_file_initialized(
        state,
        tenant_id,
        agent_ctx,
        target.file_id,
        create_fields.clone(),
    )
    .await?;
    ensure_entity_field_aliases(state, tenant_id, "File", target.file_id, canonical_fields).await?;

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
                            await_reactions: true,
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
