//! Directory composite step for `temper verify` (ADR-0150, ADR-0171).

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use temper_spec::automaton::Automaton;
use temper_spec::cross_invariant::{
    CrossInvariantLintSeverity, CrossInvariantSpec, InvariantKind, lint_cross_invariants,
    parse_cross_invariants,
};

/// Run always-on composite verification over the parsed automata.
///
/// A dropped reaction or a hard related-field violation gates the command.
/// Budget `Incomplete` is a warning. A plan that cannot include a `related()`
/// target is not a silent pass.
pub(super) fn run_composite_verification(
    parsed_automata: &std::collections::BTreeMap<String, Automaton>,
    sidecar: Option<&CrossInvariantSpec>,
) -> Result<()> {
    use temper_verify::composite::{CompositeOutcome, verify_all};

    if let Some(spec) = sidecar {
        let eventual: Vec<&str> = spec
            .invariants
            .iter()
            .filter(|inv| inv.kind == InvariantKind::Eventual)
            .map(|inv| inv.name.as_str())
            .collect();
        if !eventual.is_empty() {
            println!(
                "\nWARNING: eventual related-field constraint(s) are runtime-only and not checked here: {}",
                eventual.join(", ")
            );
        }
    }

    let automaton_refs: Vec<&Automaton> = parsed_automata.values().collect();

    println!("\nRunning composite cross-entity verification (ADR-0150)...");
    let results = verify_all(&automaton_refs, sidecar);

    let mut any_violation = false;
    let mut any_incomplete = false;
    let mut fail_lines: Vec<String> = Vec::new();

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
                let related_names: Vec<&str> = result
                    .related_field_violations
                    .iter()
                    .map(|v| v.name.as_str())
                    .collect();
                if !related_names.is_empty() {
                    println!(
                        "    [FAIL] seed={} scope=[{}] — {} joint states, related-field constraint {} violated",
                        result.seed,
                        scope,
                        result.states_explored,
                        related_names.join(", "),
                    );
                } else {
                    println!(
                        "    [FAIL] seed={} scope=[{}] — {} joint states, {} dropped reaction(s)",
                        result.seed,
                        scope,
                        result.states_explored,
                        result.dropped_reactions.len(),
                    );
                }
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
                    fail_lines.push(line);
                }
                for violation in &result.related_field_violations {
                    let line = violation.to_string();
                    println!("           - {line}");
                    fail_lines.push(line);
                }
                for other in &result.other_violations {
                    if result
                        .related_field_violations
                        .iter()
                        .any(|v| v.name == *other)
                    {
                        continue;
                    }
                    println!("           - other violated property: {other}");
                    fail_lines.push(format!("other violated property: {other}"));
                }
            }
            CompositeOutcome::Incomplete => {
                any_incomplete = true;
                println!(
                    "    [INCOMPLETE] seed={} scope=[{}] — explored {} joint states; proof is PARTIAL (not a pass)",
                    result.seed, scope, result.states_explored,
                );
                for other in &result.other_violations {
                    println!("           - {other}");
                    if other.starts_with("plan build failed") {
                        fail_lines.push(other.clone());
                    }
                }
            }
        }
    }

    if any_violation
        || fail_lines
            .iter()
            .any(|l| l.starts_with("plan build failed"))
    {
        anyhow::bail!(
            "composite cross-entity verification failed:\n  - {}",
            fail_lines.join("\n  - "),
        );
    }
    if any_incomplete {
        println!(
            "\nWARNING: composite verification was INCOMPLETE for one or more seeds (budget exhausted). The cross-entity proof is partial — narrow the spec or raise the budget to fully verify."
        );
    } else {
        println!("\nComposite cross-entity verification: ALL PASSED");
    }

    Ok(())
}

/// Load optional `cross-invariants.toml` with the same parse + lint as serve.
pub(super) fn load_related_field_sidecar(specs_path: &Path) -> Result<Option<CrossInvariantSpec>> {
    let path = specs_path.join("cross-invariants.toml");
    if !path.exists() {
        return Ok(None);
    }
    let source =
        fs::read_to_string(&path).with_context(|| format!("Failed to read {}", path.display()))?;
    let spec = parse_cross_invariants(&source)
        .with_context(|| format!("Failed to parse {}", path.display()))?;
    let mut lint_errors = Vec::new();
    for finding in lint_cross_invariants(&spec) {
        match finding.severity {
            CrossInvariantLintSeverity::Error => {
                lint_errors.push(format!("{}: {}", finding.code, finding.message));
                println!("\n  [xinv:error] {}: {}", finding.code, finding.message);
            }
            CrossInvariantLintSeverity::Warning => {
                println!("\n  [xinv:warn] {}: {}", finding.code, finding.message);
            }
        }
    }
    if !lint_errors.is_empty() {
        anyhow::bail!(
            "related-field sidecar lint failed: {}",
            lint_errors.join(" | ")
        );
    }
    println!(
        "\n  Loaded related-field sidecar ({}) with {} hard row(s)",
        path.display(),
        spec.invariants
            .iter()
            .filter(|i| i.kind == InvariantKind::Hard)
            .count()
    );
    Ok(Some(spec))
}
