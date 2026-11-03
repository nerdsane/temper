//! Verification cascade command for `temper verify`.
//!
//! Validates application files and requires a complete verification result.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use temper_platform::os_apps::{AppFiles, app_dir_of, unspecified_entity_types};
use temper_spec::automaton::{LintSeverity, lint_automata_bundle, lint_automaton};
use temper_spec::csdl::parse_csdl;
use temper_spec::model::build_spec_model;

mod input;
mod package;

/// Run the `temper verify` command.
///
/// Loads CSDL and IOA, validates policies and compiled module references, and
/// runs the verification cascade. Missing files and incomplete proofs fail.
pub fn run(specs_dir: &str) -> Result<()> {
    let specs_path = Path::new(specs_dir);
    println!("Running verification cascade...");
    println!("  Specs directory: {}", specs_path.display());
    // The specs/ of an app is read as the whole app, the files its install
    // reads (`AppFiles`), so what passes here is what installs.
    let app_dir = app_dir_of(specs_path);
    if app_dir != specs_path {
        println!(
            "  Reading the app at {}, as its install does",
            app_dir.display()
        );
    }
    let files = AppFiles::find(app_dir);

    let csdl_path = files.csdl.clone().with_context(|| {
        format!(
            "CSDL model file not found in {}. Run `temper init` first.",
            app_dir.display()
        )
    })?;
    let csdl_xml = fs::read_to_string(&csdl_path)
        .with_context(|| format!("Failed to read {}", csdl_path.display()))?;
    input::validate_xml_document(&csdl_xml)?;
    let csdl = parse_csdl(&csdl_xml)
        .with_context(|| format!("Failed to parse CSDL from {}", csdl_path.display()))?;

    anyhow::ensure!(!csdl.schemas.is_empty(), "CSDL must contain a Schema");

    // Read IOA TOML specs (preferred) and TLA+ specs (legacy)
    let ioa_sources = input::read_ioa_files(&files.ioa)?;
    // A spec may call a module of an app it depends on, which its install
    // installs first, so modules resolve through the dependencies as there.
    let modules = AppFiles::closure_modules(app_dir).map_err(anyhow::Error::msg)?;
    let tla_sources = read_tla_sources(specs_path)?;
    input::validate_ioa_entities(&csdl, &ioa_sources)?;
    let (policy_text, mut policy_warnings) =
        package::validate(&ioa_sources, &modules, &files.policies)?;
    policy_warnings.extend(files.unread_warnings(app_dir));
    let specs = ioa_sources.keys().map(String::as_str).collect();
    policy_warnings.extend(unspecified_entity_types(&csdl, &specs));

    // Each reports on its own: one that fails does not hide what the other
    // would find.
    match (
        verify_ioa_sources(&ioa_sources),
        verify_joint(&ioa_sources, &policy_text),
    ) {
        (Err(ioa), Err(senders)) => anyhow::bail!("{ioa}\n{senders}"),
        (Err(failed), Ok(())) | (Ok(()), Err(failed)) => return Err(failed),
        (Ok(()), Ok(())) => {}
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
    let warnings: Vec<&String> = spec
        .validation
        .warnings
        .iter()
        .chain(&policy_warnings)
        .collect();
    if !warnings.is_empty() {
        println!("\nWarnings ({}):", warnings.len());
        for warn in warnings {
            println!("  WARN: {warn}");
        }
    }

    // Summary
    println!("\n{}", "=".repeat(50));
    if spec.validation.is_valid() {
        println!("Result: PASS -- all cross-validation checks passed.");
    } else {
        println!(
            "Result: FAIL -- {} error(s) found.",
            spec.validation.errors.len()
        );
        anyhow::bail!("Verification failed.");
    }

    Ok(())
}

/// Verify IOA behavior independently of artifact packaging. Unit tests also use
/// this to check source collections whose modules and policies are supplied later.
fn verify_ioa_sources(ioa_sources: &std::collections::BTreeMap<String, String>) -> Result<()> {
    // Run IOA verification cascade if IOA files found
    if !ioa_sources.is_empty() {
        let mut parsed_automata = std::collections::BTreeMap::new();
        let mut lint_error_count = 0usize;
        let mut lint_error_lines = Vec::new();

        for (entity_name, ioa_source) in ioa_sources {
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
        for (entity_name, ioa_source) in ioa_sources {
            println!("\n  Verifying {entity_name}...");
            let cascade = temper_verify::cascade::VerificationCascade::from_ioa(ioa_source)
                .with_sim_seeds(5)
                .with_prop_test_cases(100);
            let result = cascade.run();
            for level in &result.levels {
                let status = if level.passed { "PASS" } else { "FAIL" };
                println!("    [{status}] {}", level.summary);
                // Detail lines localise the failure. Without them a failed
                // level reports only a count, and bisecting the spec is the
                // only way to find out which property broke.
                for line in &level.diagnostics {
                    println!("           {line}");
                }
            }
            if !result.all_passed {
                anyhow::bail!("IOA verification failed for entity '{entity_name}'");
            }
        }
        println!("\nIOA verification cascade: ALL PASSED");

        // ADR-0150: directory verification ALWAYS runs composite cross-entity
        // verification as a first-class, gating step. It composes every
        // entity's joint state machine and BFS-checks that no cross-entity
        // reaction is dropped (target not in its required from-state). This is
        // only meaningful with two or more entities — a single spec has nothing
        // to compose (and stdin verification stays per-entity by design).
        // The rules over several records are checked with the app's Cedar
        // (`verify_joint`).
        if parsed_automata.len() >= 2 {
            run_composite_verification(&parsed_automata)?;
        }
    }

    Ok(())
}

/// Where a joint check takes its memory: what this host leaves free, as a
/// server's checks do. Its time is not bounded: a walk that fits finishes.
fn joint_bound(
    progress: &temper_verify::composite::Progress,
) -> temper_verify::composite::Bound<'_> {
    temper_verify::composite::Bound {
        room: &temper_server::host_room::HOST_ROOM,
        progress,
    }
}

/// Check every `[[invariant]]` that reads a related record and every
/// `[[liveness]]` with `eventually` on the joint model, with the app's Cedar
/// deciding each move as its sender. A broken invariant fails the command
/// with its shortest path, a condition the system's own moves do not reach
/// fails with how it comes to rest short of it, and a walk that could not
/// finish fails too, since a partial walk proves nothing. So do a walk that
/// never took an answer a module may give and a rule whose premise no walked
/// state meets. A spec that does
/// not parse is reported by the IOA cascade, so it is skipped here.
fn verify_joint(
    ioa_sources: &std::collections::BTreeMap<String, String>,
    policy_text: &str,
) -> Result<()> {
    let mut parsed = Vec::new();
    for ioa_source in ioa_sources.values() {
        let Ok(automaton) = temper_spec::automaton::parse_automaton(ioa_source) else {
            return Ok(());
        };
        parsed.push(automaton);
    }
    let analysis = temper_authz::SendAnalysis::new(policy_text).context("invalid Cedar policy")?;
    let refs: Vec<&temper_spec::automaton::Automaton> = parsed.iter().collect();
    let progress = temper_verify::composite::Progress::default();
    let invariants = temper_verify::composite::verify_joint_invariants(
        &refs,
        Some(&analysis),
        joint_bound(&progress),
    );
    let invariant_rows = invariants.iter().map(|result| {
        let tag = match result.outcome {
            temper_verify::composite::JointOutcome::Holds => "PASS",
            temper_verify::composite::JointOutcome::Broken { .. } => "FAIL",
            temper_verify::composite::JointOutcome::Vacuous { .. } => "VACUOUS",
            temper_verify::composite::JointOutcome::Incomplete { .. } => "INCOMPLETE",
        };
        (
            tag,
            result.to_string(),
            format!("{}.{}", result.entity, result.invariant),
        )
    });
    let invariants = print_joint_report("Cross-entity invariants", invariant_rows.collect());
    let progress = temper_verify::composite::Progress::default();
    let live = temper_verify::composite::verify_joint_liveness(
        &refs,
        Some(&analysis),
        joint_bound(&progress),
    );
    let live_rows = live.iter().map(|result| {
        let tag = match result.outcome {
            temper_verify::composite::LivenessOutcome::Holds => "PASS",
            temper_verify::composite::LivenessOutcome::Stuck { .. } => "FAIL",
            temper_verify::composite::LivenessOutcome::Vacuous { .. } => "VACUOUS",
            temper_verify::composite::LivenessOutcome::Incomplete { .. } => "INCOMPLETE",
        };
        (
            tag,
            result.to_string(),
            format!("{}.{}", result.entity, result.name),
        )
    });
    let liveness = print_joint_report("Liveness", live_rows.collect());
    // Each reports on its own: one that fails does not hide the other.
    match (invariants, liveness) {
        (Err(invariants), Err(liveness)) => anyhow::bail!("{invariants}\n{liveness}"),
        (Err(failed), Ok(())) | (Ok(()), Err(failed)) => Err(failed),
        (Ok(()), Ok(())) => Ok(()),
    }
}

/// Print one joint check's results, each row its tag, the result in words
/// and the property's name, and fail when a row did not pass.
fn print_joint_report(check: &str, rows: Vec<(&str, String, String)>) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let mut report = format!("\nRunning {} on the joint model...\n", check.to_lowercase());
    let mut failed = Vec::new();
    for (tag, result, name) in &rows {
        report.push_str(&format!("    [{tag}] {result}\n"));
        if *tag != "PASS" {
            failed.push(name.as_str());
        }
    }
    if failed.is_empty() {
        report.push_str(&format!("\n{check}: ALL PASSED"));
    }
    println!("{report}");
    if !failed.is_empty() {
        anyhow::bail!("{} not proven: {}", check.to_lowercase(), failed.join(", "));
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
/// An INCOMPLETE run (the walk did not fit in the memory this host leaves it)
/// fails complete-application verification.
fn run_composite_verification(
    parsed_automata: &std::collections::BTreeMap<String, temper_spec::automaton::Automaton>,
) -> Result<()> {
    use temper_verify::composite::verify_all;

    let automaton_refs: Vec<&temper_spec::automaton::Automaton> =
        parsed_automata.values().collect();

    println!("\nRunning composite cross-entity verification (ADR-0150)...");
    let progress = temper_verify::composite::Progress::default();
    let results = verify_all(&automaton_refs, joint_bound(&progress));
    report_composite_results(&results)
}

fn report_composite_results(
    results: &[temper_verify::composite::CompositeVerifyResult],
) -> Result<()> {
    use temper_verify::composite::CompositeOutcome;

    let mut any_violation = false;
    let mut any_incomplete = false;
    let mut dropped_lines: Vec<String> = Vec::new();

    for result in results {
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
            }
            CompositeOutcome::Incomplete => {
                any_incomplete = true;
                println!(
                    "    [INCOMPLETE] seed={} scope=[{}] — explored {} joint states; {}, proof is PARTIAL (not a pass)",
                    result.seed,
                    scope,
                    result.states_explored,
                    result
                        .incomplete
                        .as_deref()
                        .unwrap_or("not every state was walked"),
                );
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
        anyhow::bail!("composite verification incomplete: not every joint state was walked");
    }
    println!("\nComposite cross-entity verification: ALL PASSED");

    Ok(())
}

// read_tla_sources is shared via crate::util::read_tla_sources
use crate::util::read_tla_sources;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod migration_tests;

#[cfg(test)]
mod repository_tests;
