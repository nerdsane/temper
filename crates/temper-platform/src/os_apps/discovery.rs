//! Filesystem discovery of OS app components.
//!
//! Scans an app directory for the manifest, WASM module binaries, agent
//! definitions, skills, ADRs, seed data, and the app guide. IOA/CSDL/Cedar
//! spec discovery lives in `temper_spec::loader`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::{
    AdrEntry, AgentDefinition, AppManifest, AppSkillDefinition, CompanionFile, SeedInstance,
    WasmModuleManifest,
};

pub(super) fn read_app_manifest(app_dir: &Path) -> Option<AppManifest> {
    let path = app_dir.join("app.toml");
    let content = std::fs::read_to_string(&path).ok()?;
    toml::from_str(&content).ok()
}

/// Container for parsing `seed-data/*.toml` files.
#[derive(Debug, serde::Deserialize)]
struct SeedFile {
    #[serde(rename = "instance", default)]
    instances: Vec<SeedInstance>,
}

/// Find compiled WASM module binaries in an app directory.
///
/// Scans both `wasm32-unknown-unknown` and `wasm32-wasip1` release outputs
/// because some OS apps mix pure WASM modules with WASI modules such as
/// sandboxed tool runners.
pub(super) fn find_wasm_modules(
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
pub(super) fn find_agents(app_dir: &Path) -> Vec<AgentDefinition> {
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
pub(super) fn find_app_skills(app_dir: &Path) -> Vec<AppSkillDefinition> {
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

        let description = extract_frontmatter_description(&content)
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
pub(super) fn find_adrs(app_dir: &Path) -> Vec<AdrEntry> {
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

/// Extract the `description` field from SKILL.md frontmatter.
/// Supports YAML (`---`) and TOML (`+++`) frontmatter blocks.
fn extract_frontmatter_description(content: &str) -> Option<String> {
    let block = if let Some(rest) = content.strip_prefix("---") {
        let end = rest.find("\n---")?;
        &rest[..end]
    } else if let Some(rest) = content.strip_prefix("+++") {
        let end = rest.find("+++")?;
        &rest[..end]
    } else {
        return None;
    };

    for line in block.lines() {
        let trimmed = line.trim();
        // YAML `description: value` or TOML `description = "value"`.
        let val = trimmed.strip_prefix("description:").or_else(|| {
            trimmed
                .strip_prefix("description")
                .and_then(|r| r.trim_start().strip_prefix('='))
        });
        if let Some(val) = val {
            let val = val.trim().trim_matches('"').trim_matches('\'');
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
pub(super) fn find_seed_data(app_dir: &Path) -> Vec<SeedInstance> {
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
pub(super) fn read_app_guide(app_dir: &Path) -> Option<String> {
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
pub(super) fn extract_description(guide: &str) -> Option<String> {
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
