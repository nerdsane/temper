//! Verification cascade command for `temper verify`.
//!
//! Loads specifications and runs validation checks. Full model checking
//! integration with temper-verify will be added in a future release.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use temper_spec::automaton::{LintSeverity, lint_automata_bundle, lint_automaton};
use temper_spec::csdl::parse_csdl;
use temper_spec::model::build_spec_model;

use crate::util::to_pascal_case;

/// Run the `temper verify` command.
///
/// Each directory is linted and cascade-checked on its own. Composite
/// verification then runs once over the *union* of every directory's
/// automata, so a guard in app A that reads an entity in app B is a
/// concrete joint proof, not a free boolean. An INCOMPLETE joint BFS
/// fails the command.
pub fn run(specs_dirs: &[String], composite_budget: usize) -> Result<()> {
    if specs_dirs.is_empty() {
        anyhow::bail!("temper verify requires at least one --specs-dir");
    }

    println!("Running verification cascade...");
    for dir in specs_dirs {
        println!("  Specs directory: {dir}");
    }

    let mut all_automata = std::collections::BTreeMap::new();
    for specs_dir in specs_dirs {
        verify_one_directory(specs_dir, &mut all_automata)?;
    }

    if all_automata.len() >= 2 {
        run_composite_verification(&all_automata, composite_budget)?;
    }

    Ok(())
}

fn verify_one_directory(
    specs_dir: &str,
    all_automata: &mut std::collections::BTreeMap<String, temper_spec::automaton::Automaton>,
) -> Result<()> {
    let specs_path = Path::new(specs_dir);

    println!("\n--- {}", specs_path.display());

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

    // Read IOA TOML specs (preferred) and TLA+ specs (legacy)
    let ioa_sources = read_ioa_sources(specs_path)?;
    let tla_sources = read_tla_sources(specs_path)?;

    // Run IOA verification cascade if IOA files found
    if !ioa_sources.is_empty() {
        let mut parsed_automata = std::collections::BTreeMap::new();
        let mut lint_error_count = 0usize;
        let mut lint_error_lines = Vec::new();

        for (entity_name, ioa_source) in &ioa_sources {
            let automaton = temper_spec::automaton::parse_automaton(ioa_source)
                .with_context(|| format!("Failed to parse IOA spec for '{entity_name}'"))?;

            for finding in lint_automaton(&automaton) {
                match finding.severity {
                    LintSeverity::Error => {
                        lint_error_count += 1;
                        lint_error_lines.push(format!(
                            "{entity_name}: {} — {}",
                            finding.code, finding.message
                        ));
                        println!(
                            "\n  [lint:error] {entity_name}: {} — {}",
                            finding.code, finding.message
                        );
                    }
                    LintSeverity::Warning => {
                        println!(
                            "\n  [lint:warn] {entity_name}: {} — {}",
                            finding.code, finding.message
                        );
                    }
                }
            }

            parsed_automata.insert(entity_name.clone(), automaton);
        }

        for finding in lint_automata_bundle(&parsed_automata) {
            match finding.severity {
                LintSeverity::Error => {
                    lint_error_count += 1;
                    lint_error_lines.push(format!(
                        "{}: {} — {}",
                        finding.entity, finding.code, finding.message
                    ));
                    println!(
                        "\n  [lint:error] {}: {} — {}",
                        finding.entity, finding.code, finding.message
                    );
                }
                LintSeverity::Warning => {
                    println!(
                        "\n  [lint:warn] {}: {} — {}",
                        finding.entity, finding.code, finding.message
                    );
                }
            }
        }

        if lint_error_count > 0 {
            anyhow::bail!(
                "IOA lint failed with {lint_error_count} error(s): {}",
                lint_error_lines.join(" | ")
            );
        }

        println!("\nRunning IOA verification cascade...");
        for (entity_name, ioa_source) in &ioa_sources {
            println!("\n  Verifying {entity_name}...");
            let cascade = temper_verify::cascade::VerificationCascade::from_ioa(ioa_source)
                .with_sim_seeds(5)
                .with_prop_test_cases(100);
            let result = cascade.run();
            for level in &result.levels {
                let status = if level.passed { "PASS" } else { "FAIL" };
                println!("    [{status}] {}", level.summary);
            }
            if !result.all_passed {
                anyhow::bail!("IOA verification failed for entity '{entity_name}'");
            }
        }
        println!("\nIOA verification cascade: ALL PASSED");

        for (name, automaton) in parsed_automata {
            if all_automata.contains_key(&name) {
                anyhow::bail!(
                    "entity '{name}' appears in more than one --specs-dir; composite needs one automaton per type"
                );
            }
            all_automata.insert(name, automaton);
        }
    }

    // Build spec model (which includes cross-validation)
    let spec = build_spec_model(csdl, tla_sources);

    // Report results
    println!("\nVerification Report");
    println!("{}", "=".repeat(50));

    // Schema summary
    let entity_count: usize = spec.csdl.schemas.iter().map(|s| s.entity_types.len()).sum();
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

/// Run always-on composite cross-entity verification over the parsed
/// automata (ADR-0150).
///
/// Seeds the joint-state BFS from the root of each weakly-connected component
/// of the entity trigger graph (so every entity is covered), checks the
/// `no_dropped_reaction` property, and reports every dropped reaction with
/// enough detail to name it. A dropped reaction GATES — it fails the command.
/// An INCOMPLETE run (budget exhausted) fails the command. It is not a pass.
fn run_composite_verification(
    parsed_automata: &std::collections::BTreeMap<String, temper_spec::automaton::Automaton>,
    composite_budget: usize,
) -> Result<()> {
    use temper_verify::composite::{CompositeOutcome, verify_all_with_budget};

    let automaton_refs: Vec<&temper_spec::automaton::Automaton> =
        parsed_automata.values().collect();

    println!(
        "\nRunning composite cross-entity verification (ADR-0150, budget {composite_budget})..."
    );
    let results = verify_all_with_budget(&automaton_refs, composite_budget);

    let mut any_violation = false;
    let mut any_incomplete = false;
    let mut dropped_lines: Vec<String> = Vec::new();

    for result in &results {
        let scope = result.scope.join(", ");
        match result.outcome {
            CompositeOutcome::Verified => {
                println!(
                    "    [PASS] seed={} scope=[{}] — {} joint states, no dropped reactions",
                    result.seed, scope, result.states_explored,
                );
            }
            CompositeOutcome::Violated => {
                any_violation = true;
                println!(
                    "    [FAIL] seed={} scope=[{}] — {} joint states, {} dropped reaction(s)",
                    result.seed,
                    scope,
                    result.states_explored,
                    result.dropped_reactions.len(),
                );
                for drop in &result.dropped_reactions {
                    let line = format!(
                        "{}.{} fired trigger '{}' targeting {}.{}, but {} was in '{}' (action not enabled) — reaction DROPPED",
                        drop.source_entity,
                        drop.source_action,
                        drop.trigger_name,
                        drop.target_entity,
                        drop.target_action,
                        drop.target_entity,
                        drop.target_state,
                    );
                    println!("           - {line}");
                    dropped_lines.push(line);
                }
                for other in &result.other_violations {
                    println!("           - other violated property: {other}");
                }
            }
            CompositeOutcome::Incomplete => {
                any_incomplete = true;
                let reason = if !result.other_violations.is_empty() && result.states_explored == 0 {
                    result.other_violations.join("; ")
                } else {
                    format!(
                        "BFS budget exhausted ({} joint states)",
                        result.states_explored
                    )
                };
                println!(
                    "    [INCOMPLETE] seed={} scope=[{}] — {}; proof is PARTIAL (not a pass)",
                    result.seed, scope, reason,
                );
                if result.states_explored > 0 {
                    for other in &result.other_violations {
                        println!("           - {other}");
                    }
                }
            }
        }
    }

    if any_violation {
        anyhow::bail!(
            "composite cross-entity verification failed: {} dropped reaction(s):\n  - {}",
            dropped_lines.len(),
            dropped_lines.join("\n  - "),
        );
    }
    if any_incomplete {
        anyhow::bail!(
            "composite cross-entity verification INCOMPLETE (budget {composite_budget}). Some seeds did not finish a joint proof — a failed plan (missing trigger target) or an exhausted BFS. This is not a pass. Re-run with every --specs-dir the machines join, or raise --composite-budget."
        );
    }
    println!("\nComposite cross-entity verification: ALL PASSED");

    Ok(())
}

/// Read all `.ioa.toml` files from the specs directory.
fn read_ioa_sources(specs_dir: &Path) -> Result<HashMap<String, String>> {
    let mut sources = HashMap::new();

    if !specs_dir.is_dir() {
        return Ok(sources);
    }

    for entry in fs::read_dir(specs_dir)
        .with_context(|| format!("Failed to read specs directory: {}", specs_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();

        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();

        if file_name.ends_with(".ioa.toml") {
            let entity_name = file_name.strip_suffix(".ioa.toml").unwrap_or_default();
            let entity_name = to_pascal_case(entity_name);

            let source = fs::read_to_string(&path)
                .with_context(|| format!("Failed to read IOA file: {}", path.display()))?;

            sources.insert(entity_name, source);
        }
    }

    Ok(sources)
}

// read_tla_sources is shared via crate::util::read_tla_sources
use crate::util::read_tla_sources;

#[cfg(test)]
mod tests;
