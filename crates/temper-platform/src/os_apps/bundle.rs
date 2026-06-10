//! App bundle loading and OS app dependency resolution.

use std::collections::BTreeMap;
use std::path::Path;

use super::discovery::{
    find_adrs, find_agents, find_app_skills, find_seed_data, find_wasm_modules, read_app_guide,
    read_app_manifest,
};
use super::{AppBundle, WasmModuleManifest, catalog, system_files};

/// Load a complete app bundle from a directory on disk.
pub(super) fn load_app_bundle(app_dir: &Path) -> Option<AppBundle> {
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
    // Discover IOA specs, CSDL, Cedar policies, and cross-invariants via the
    // shared spec-directory loader. Entity types come from each parsed
    // automaton's `name` field (with a filename-derived fallback label for
    // specs that fail to parse — bootstrap reports the parse error later).
    let spec_bundle = match temper_spec::loader::load_spec_dir(app_dir) {
        Ok(bundle) => bundle,
        Err(e) => {
            tracing::error!(path = %app_dir.display(), error = %e, "Failed to load app spec bundle");
            return None;
        }
    };
    let specs = spec_bundle.specs;
    let csdl = spec_bundle.csdl.map(|source| source.content);
    let cross_invariants_toml = spec_bundle.cross_invariants_toml;
    let cedar_policies: Vec<String> = spec_bundle
        .cedar_policies
        .into_iter()
        .map(|policy| policy.content)
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

pub(super) fn os_app_dependencies(name: &str) -> Vec<String> {
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
