//! Spec loading helpers for MCP runtime context.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};
use temper_spec::{parse_automaton, parse_csdl, parse_var_initial_json};

use super::AppConfig;
use super::runtime::AppMetadata;

pub(super) fn load_apps(apps: &[AppConfig]) -> Result<(Value, BTreeMap<String, AppMetadata>)> {
    let mut root = Map::<String, Value>::new();
    let mut metadata = BTreeMap::<String, AppMetadata>::new();

    for app in apps {
        let mut entities = Map::<String, Value>::new();

        for path in find_files_with_suffix(&app.specs_dir, ".ioa.toml")? {
            let source = fs::read_to_string(&path)
                .with_context(|| format!("failed to read IOA spec {}", path.display()))?;
            let automaton = parse_automaton(&source)
                .with_context(|| format!("failed to parse IOA spec {}", path.display()))?;
            entities.insert(
                automaton.automaton.name.clone(),
                automaton_to_json(&automaton),
            );
        }

        root.insert(app.name.clone(), json!({ "entities": entities }));

        let csdl_path = app.specs_dir.join("model.csdl.xml");
        if csdl_path.exists() {
            let csdl_xml = fs::read_to_string(&csdl_path)
                .with_context(|| format!("failed to read CSDL {}", csdl_path.display()))?;
            let csdl = parse_csdl(&csdl_xml)
                .with_context(|| format!("failed to parse CSDL {}", csdl_path.display()))?;

            let mut app_meta = AppMetadata::default();
            for schema in &csdl.schemas {
                for container in &schema.entity_containers {
                    for set in &container.entity_sets {
                        let short_type = set
                            .entity_type
                            .rsplit('.')
                            .next()
                            .unwrap_or(&set.entity_type)
                            .to_string();

                        app_meta
                            .entity_set_to_type
                            .insert(set.name.clone(), short_type.clone());
                        app_meta
                            .entity_type_to_set
                            .entry(short_type)
                            .or_insert_with(|| set.name.clone());
                    }
                }
            }

            metadata.insert(app.name.clone(), app_meta);
        }
    }

    Ok((Value::Object(root), metadata))
}

fn automaton_to_json(automaton: &temper_spec::Automaton) -> Value {
    let vars = automaton
        .state
        .iter()
        .map(|var| {
            (
                var.name.clone(),
                json!({
                    "type": var.var_type,
                    "init": parse_var_initial_json(&var.var_type, &var.initial)
                }),
            )
        })
        .collect::<Map<String, Value>>();

    let actions = automaton
        .actions
        .iter()
        .map(|action| {
            json!({
                "name": action.name,
                "kind": action.kind,
                "from": action.from,
                "to": action.to,
                "guards": action.guard,
                "effects": action.effect,
                "params": action.params,
                "hint": action.hint,
            })
        })
        .collect::<Vec<_>>();

    json!({
        "states": automaton.automaton.states,
        "initial": automaton.automaton.initial,
        "actions": actions,
        "vars": vars,
    })
}
fn find_files_with_suffix(root: &Path, suffix: &str) -> Result<Vec<PathBuf>> {
    if !root.exists() {
        bail!("specs path does not exist: {}", root.display());
    }

    let mut stack = vec![root.to_path_buf()];
    let mut files = Vec::new();

    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir)
            .with_context(|| format!("failed to read directory {}", dir.display()))?
        {
            let entry = entry
                .with_context(|| format!("failed to read directory entry in {}", dir.display()))?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(suffix))
            {
                files.push(path);
            }
        }
    }

    files.sort();
    Ok(files)
}
