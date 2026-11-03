//! Which of an app's files make each part of its bundle: its specs, CSDL,
//! policies, WASM modules, agents, skills, ADRs and seed data. Each part is
//! picked from the app's [`AppFileSource`] and read by its path.

use std::collections::BTreeMap;
use std::path::Path;

use super::file_source::AppFileSource;
use super::{
    AdrEntry, AgentDefinition, AppFiles, AppSkillDefinition, CompanionFile, SeedFile, SeedInstance,
    WasmModuleManifest, extract_description,
};

/// Every IOA spec of the app: `*.ioa.toml` in its own folder first, then in
/// `specs/`. A name in both is taken from the app's own folder.
pub(crate) fn find_ioa_files(files: &AppFileSource) -> Vec<String> {
    AppFiles::from_source(files)
        .ioa
        .iter()
        .map(|path| super::app_relative_path(files.dir(), path))
        .collect()
}

/// The compiled WASM module of every module `app.toml` declares, selected
/// by the same reader used by `temper verify`.
pub(super) fn find_wasm_modules(
    files: &AppFileSource,
    module_configs: &BTreeMap<String, WasmModuleManifest>,
) -> Result<BTreeMap<String, Vec<u8>>, String> {
    let mut modules = BTreeMap::new();
    for (module_name, config) in module_configs {
        if let Some(path) = super::app_files::module_binary(files, config) {
            let bytes = files.read(&super::app_relative_path(files.dir(), &path))?;
            modules.insert(module_name.clone(), bytes);
        }
    }
    warn_undeclared_wasm_artifacts(files, module_configs);
    Ok(modules)
}

fn warn_undeclared_wasm_artifacts(
    files: &AppFileSource,
    module_configs: &BTreeMap<String, WasmModuleManifest>,
) {
    for module_name in files.dirs_in("wasm") {
        if module_name == "target" || module_configs.contains_key(&module_name) {
            continue;
        }
        let sibling = format!("wasm/{module_name}/{module_name}.wasm");
        if files.has(&sibling) {
            tracing::warn!(
                module = %module_name,
                path = %sibling,
                "Ignoring undeclared bundled WASM artifact; declare it in app.toml [[wasm_modules]] to install it"
            );
        }
    }
}

/// Agent definitions from `agents/{name}/`.
///
/// Each folder is one agent. Its `.md` files are concatenated in name order.
/// The platform is filename-agnostic: conventions like SOUL.md, STYLE.md and
/// AGENT.md are for humans.
pub(super) fn find_agents(files: &AppFileSource) -> Result<Vec<AgentDefinition>, String> {
    let mut results = Vec::new();
    for agent_name in files.dirs_in("agents") {
        let agent_dir = format!("agents/{agent_name}");
        let md_files: Vec<String> = files
            .files_in(&agent_dir)
            .into_iter()
            .filter(|name| name.to_lowercase().ends_with(".md"))
            .collect();
        if md_files.is_empty() {
            continue;
        }
        let has_soul = md_files.iter().any(|name| name == "SOUL.md");

        let mut content = String::new();
        for name in &md_files {
            if !content.is_empty() {
                content.push_str("\n\n");
            }
            content.push_str(&files.read_text(&format!("{agent_dir}/{name}"))?);
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
    Ok(results)
}

/// Skill definitions: `system/skills/{name}/` (system-level) and
/// `agents/{agent}/skills/{name}/` (agent-scoped).
///
/// A skill's folder holds its `SKILL.md`; every other file in it is a
/// companion file. A folder without a `SKILL.md` is not a skill.
pub(super) fn find_app_skills(files: &AppFileSource) -> Result<Vec<AppSkillDefinition>, String> {
    let mut results = Vec::new();
    scan_skill_dirs(files, "system/skills", None, &mut results)?;
    for agent_name in files.dirs_in("agents") {
        let skills_dir = format!("agents/{agent_name}/skills");
        scan_skill_dirs(files, &skills_dir, Some(&agent_name), &mut results)?;
    }
    Ok(results)
}

fn scan_skill_dirs(
    files: &AppFileSource,
    skills_dir: &str,
    agent_name: Option<&str>,
    results: &mut Vec<AppSkillDefinition>,
) -> Result<(), String> {
    for skill_name in files.dirs_in(skills_dir) {
        let skill_dir = format!("{skills_dir}/{skill_name}");
        let skill_path = format!("{skill_dir}/SKILL.md");
        if !files.has(&skill_path) {
            continue;
        }
        let content = files.read_text(&skill_path)?;
        let frontmatter = super::extract_frontmatter(&content);
        let description = frontmatter
            .description
            .or_else(|| extract_description(&content))
            .unwrap_or_else(|| format!("Skill: {skill_name}"));

        let mut companion_files = Vec::new();
        for name in files.files_under(&skill_dir) {
            if Path::new(&name)
                .file_name()
                .is_some_and(|f| f == "SKILL.md")
            {
                continue;
            }
            companion_files.push(CompanionFile {
                content: files.read(&format!("{skill_dir}/{name}"))?,
                mime_type: super::mime_from_extension(Path::new(&name)),
                name,
            });
        }

        results.push(AppSkillDefinition {
            name: skill_name,
            content,
            description,
            agent_name: agent_name.map(String::from),
            companion_files,
        });
    }
    Ok(())
}

/// App-local ADRs from `adrs/*.md`, in name order.
pub(super) fn find_adrs(files: &AppFileSource) -> Result<Vec<AdrEntry>, String> {
    let mut results = Vec::new();
    for file_name in files.files_in("adrs") {
        let path = Path::new(&file_name);
        let is_markdown = path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("md"));
        if !is_markdown {
            continue;
        }
        let name = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or(&file_name)
            .to_string();
        results.push(AdrEntry {
            name,
            content: files.read_text(&format!("adrs/{file_name}"))?,
            file_name,
        });
    }
    Ok(results)
}

/// Seed data instances from `seed-data/*.toml`, each holding `[[instance]]`
/// blocks of entities to create on first install. A file that does not
/// parse is left out with a warning.
pub(super) fn find_seed_data(files: &AppFileSource) -> Result<Vec<SeedInstance>, String> {
    let mut all_instances = Vec::new();
    for name in files.files_in("seed-data") {
        if !name.ends_with(".toml") {
            continue;
        }
        let path = format!("seed-data/{name}");
        match toml::from_str::<SeedFile>(&files.read_text(&path)?) {
            Ok(seed_file) => all_instances.extend(seed_file.instances),
            Err(e) => {
                tracing::warn!(path = %path, error = %e, "Failed to parse seed data file");
            }
        }
    }
    Ok(all_instances)
}
