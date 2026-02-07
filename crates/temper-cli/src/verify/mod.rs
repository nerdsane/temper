//! Verification cascade command for `temper verify`.
//!
//! Loads specifications and runs validation checks. Full model checking
//! integration with temper-verify will be added in a future release.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use temper_spec::csdl::parse_csdl;
use temper_spec::model::build_spec_model;

/// Run the `temper verify` command.
///
/// Loads specs from the given directory, builds the spec model, and reports
/// validation results. Full Stateright model checking will be integrated
/// once temper-verify exposes its public API.
pub fn run(specs_dir: &str) -> Result<()> {
    let specs_path = Path::new(specs_dir);

    println!("Running verification cascade...");
    println!("  Specs directory: {}", specs_path.display());

    // Read the CSDL model file
    let csdl_path = specs_path.join("model.csdl.xml");
    if !csdl_path.exists() {
        anyhow::bail!(
            "CSDL model file not found at {}. Run `temper init` first.",
            csdl_path.display()
        );
    }

    let csdl_xml = fs::read_to_string(&csdl_path)
        .with_context(|| format!("Failed to read {}", csdl_path.display()))?;
    let csdl = parse_csdl(&csdl_xml)
        .with_context(|| format!("Failed to parse CSDL from {}", csdl_path.display()))?;

    // Read TLA+ spec files
    let tla_sources = read_tla_sources(specs_path)?;

    // Build spec model (which includes cross-validation)
    let spec = build_spec_model(csdl, tla_sources);

    // Report results
    println!("\nVerification Report");
    println!("{}", "=".repeat(50));

    // Schema summary
    let entity_count: usize = spec
        .csdl
        .schemas
        .iter()
        .map(|s| s.entity_types.len())
        .sum();
    let action_count: usize = spec.csdl.schemas.iter().map(|s| s.actions.len()).sum();
    let function_count: usize = spec.csdl.schemas.iter().map(|s| s.functions.len()).sum();

    println!("\nSpecification Summary:");
    println!("  Entity types:    {entity_count}");
    println!("  Actions:         {action_count}");
    println!("  Functions:       {function_count}");
    println!("  State machines:  {}", spec.state_machines.len());

    // State machine details
    for (name, sm) in &spec.state_machines {
        println!("\n  State Machine: {name}");
        println!("    States:       {}", sm.states.len());
        println!("    Transitions:  {}", sm.transitions.len());
        println!("    Invariants:   {}", sm.invariants.len());
        println!("    Liveness:     {}", sm.liveness_properties.len());
    }

    // Validation errors
    if !spec.validation.errors.is_empty() {
        println!("\nErrors ({}):", spec.validation.errors.len());
        for err in &spec.validation.errors {
            println!("  FAIL: {err}");
        }
    }

    // Validation warnings
    if !spec.validation.warnings.is_empty() {
        println!("\nWarnings ({}):", spec.validation.warnings.len());
        for warn in &spec.validation.warnings {
            println!("  WARN: {warn}");
        }
    }

    // Summary
    println!("\n{}", "=".repeat(50));
    if spec.validation.is_valid() {
        println!("Result: PASS -- all cross-validation checks passed.");
        println!("\nNote: Full model checking (Stateright) is not yet integrated.");
        println!("      Run TLC separately for exhaustive state space exploration.");
    } else {
        println!(
            "Result: FAIL -- {} error(s) found.",
            spec.validation.errors.len()
        );
        anyhow::bail!("Verification failed.");
    }

    Ok(())
}

/// Read all `.tla` files from the specs directory.
fn read_tla_sources(specs_dir: &Path) -> Result<HashMap<String, String>> {
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

/// Convert a string to PascalCase.
fn to_pascal_case(s: &str) -> String {
    s.split(|c: char| c == '_' || c == '-')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => {
                    let upper: String = first.to_uppercase().collect();
                    format!("{}{}", upper, chars.collect::<String>())
                }
                None => String::new(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_verify_reference_specs() {
        let specs_dir = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../reference/ecommerce/specs"
        );

        if !Path::new(specs_dir).join("model.csdl.xml").exists() {
            eprintln!("Skipping verify test: reference specs not found");
            return;
        }

        let result = run(specs_dir);
        result.expect("verify should pass on reference specs");
    }
}
