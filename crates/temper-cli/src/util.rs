//! Shared utilities for the CLI crate.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

pub(crate) use temper_spec::naming::{to_pascal_case, to_snake_case};

/// Read all `.tla` files from a specs directory and return a map of
/// entity name (PascalCase from file stem) to TLA+ source text.
pub(crate) fn read_tla_sources(specs_dir: &Path) -> Result<HashMap<String, String>> {
    let mut sources = HashMap::new();

    if !specs_dir.is_dir() {
        return Ok(sources);
    }

    for entry in fs::read_dir(specs_dir)
        .with_context(|| format!("Failed to read specs directory: {}", specs_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();

        if path.extension().and_then(|e| e.to_str()) == Some("tla") {
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default();

            let entity_name = to_pascal_case(stem);
            let source = fs::read_to_string(&path)
                .with_context(|| format!("Failed to read TLA+ file: {}", path.display()))?;

            sources.insert(entity_name, source);
        }
    }

    Ok(sources)
}
