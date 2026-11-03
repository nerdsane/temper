//! Policy and compiled module checks for `temper verify`.
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Require each module the specs reference among the compiled `modules`
/// its install makes available (`AppFiles::closure_modules`: the app's own
/// and those of the apps it depends on), then parse
/// all `policies` and return their text, with a warning for each entity no
/// permit reaches. A permit in any policy file counts for any entity.
pub(super) fn validate(
    ioa_sources: &BTreeMap<String, String>,
    modules: &BTreeMap<String, PathBuf>,
    policies: &[PathBuf],
) -> Result<(String, Vec<String>)> {
    anyhow::ensure!(
        !ioa_sources.is_empty(),
        "source contains no IOA specifications"
    );
    for source in ioa_sources.values() {
        let automaton = temper_spec::automaton::parse_automaton(source)?;
        for module in automaton
            .actions
            .iter()
            .flat_map(|a| &a.triggers)
            .filter_map(|t| t.module.as_deref())
            .chain(
                automaton
                    .integrations
                    .iter()
                    .filter_map(|i| i.module.as_deref()),
            )
        {
            anyhow::ensure!(
                !module.is_empty()
                    && module
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
                "invalid module name"
            );
            anyhow::ensure!(
                modules.contains_key(module),
                "missing compiled WASM module {module}: app.toml, or that of an app it \
                 depends on, must declare it in [[wasm_modules]], with its binary under \
                 wasm/{module}/"
            );
        }
    }
    let text = validate_policy_files(policies)?;
    let reached = temper_authz::permitted_resource_types(&text).context("invalid Cedar policy")?;
    let warnings = ioa_sources
        .keys()
        .filter(|entity| {
            reached
                .as_ref()
                .is_some_and(|types| !types.contains(*entity))
        })
        .map(|entity| format!("no Cedar permit reaches {entity}"))
        .collect();
    Ok((text, warnings))
}

/// Parse `policies`, in the order the server loads them, and return their
/// text. Source collections may have no policies before application
/// packaging.
pub(super) fn validate_policy_files(policies: &[PathBuf]) -> Result<String> {
    let mut texts = Vec::new();
    for path in policies {
        texts.push(
            std::fs::read_to_string(path)
                .with_context(|| format!("failed to read Cedar policy {}", path.display()))?,
        );
    }
    let text = texts.join("\n\n");
    temper_authz::AuthzEngine::new(&text).context("invalid Cedar policy")?;
    Ok(text)
}
