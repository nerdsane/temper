//! Verification cascade command for `temper verify`.
//!
//! Loads I/O Automaton specs, parses each once to `Automaton`, and prints
//! the L0–L3 cascade results.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use temper_spec::automaton::{LintSeverity, lint_automata_bundle, lint_automaton};
use temper_spec::csdl::parse_csdl;

use crate::util::to_pascal_case;

mod joint;
use joint::{load_related_field_sidecar, run_composite_verification};

/// Run the `temper verify` command.
///
/// Loads specs from the given directory, parses each IOA file once to
/// `Automaton`, and prints the L0–L3 cascade that already ran.
pub fn run(specs_dir: &str) -> Result<()> {
    let specs_path = Path::new(specs_dir);

    println!("Running verification cascade...");
    println!("  Specs directory: {}", specs_path.display());

    // Read the CSDL model file (still required to serve; ARN-383 replaces authoring).
    let csdl_path = specs_path.join("model.csdl.xml");
    if !csdl_path.exists() {
        anyhow::bail!(
            "CSDL model file not found at {}. Run `temper init` first.",
            csdl_path.display()
        );
    }

    let csdl_xml = fs::read_to_string(&csdl_path)
        .with_context(|| format!("Failed to read {}", csdl_path.display()))?;
    let _csdl = parse_csdl(&csdl_xml)
        .with_context(|| format!("Failed to parse CSDL from {}", csdl_path.display()))?;

    let ioa_sources = read_ioa_sources(specs_path)?;
    if ioa_sources.is_empty() {
        anyhow::bail!(
            "No .ioa.toml files found in {}. TLA+ is not an authored format.",
            specs_path.display()
        );
    }

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

    let sidecar = load_related_field_sidecar(specs_path)?;

    println!("\nRunning IOA verification cascade...");
    let mut cascade_summaries: Vec<(String, Vec<(bool, String)>)> = Vec::new();
    for (entity_name, automaton) in &parsed_automata {
        println!("\n  Verifying {entity_name}...");
        let result = temper_verify::cascade::VerificationCascade::from_automaton(automaton.clone())
            .with_sim_seeds(5)
            .with_prop_test_cases(100)
            .run();
        let mut level_lines = Vec::new();
        for level in &result.levels {
            let status = if level.passed { "PASS" } else { "FAIL" };
            println!("    [{status}] {}", level.summary);
            level_lines.push((level.passed, level.summary.clone()));
        }
        cascade_summaries.push((entity_name.clone(), level_lines));
        if !result.all_passed {
            anyhow::bail!("IOA verification failed for entity '{entity_name}'");
        }
    }
    println!("\nIOA verification cascade: ALL PASSED");

    // ADR-0150 / ADR-0171: directory verification always runs composite
    // when there are two or more entities, or when a related-field sidecar
    // is present (so a missing related() target is not a silent pass).
    // `temper verify-ioa` stays per-entity.
    if parsed_automata.len() >= 2 || sidecar.is_some() {
        run_composite_verification(&parsed_automata, sidecar.as_ref())?;
    }

    println!("\nVerification Report");
    println!("{}", "=".repeat(50));
    println!("\nL0–L3 cascade:");
    for (entity_name, levels) in &cascade_summaries {
        println!("  {entity_name}");
        for (passed, summary) in levels {
            let status = if *passed { "PASS" } else { "FAIL" };
            println!("    [{status}] {summary}");
        }
    }
    println!("\n{}", "=".repeat(50));
    println!("Result: PASS — L0–L3 cascade passed.");

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_verify_reference_specs() {
        let specs_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../test-fixtures/specs");

        if !Path::new(specs_dir).join("model.csdl.xml").exists() {
            eprintln!("Skipping verify test: reference specs not found");
            return;
        }

        let result = run(specs_dir);
        result.expect("verify should pass on reference specs");
    }

    #[test]
    fn test_verify_fails_on_broken_spawn_contract_with_exact_lint_code() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let specs_dir = tmp.path();

        let csdl = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Broken" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Plan">
        <Key><PropertyRef Name="Id" /></Key>
        <Property Name="Id" Type="Edm.Guid" Nullable="false" />
        <Property Name="status" Type="Edm.String" />
      </EntityType>
      <EntityType Name="Task">
        <Key><PropertyRef Name="Id" /></Key>
        <Property Name="Id" Type="Edm.Guid" Nullable="false" />
        <Property Name="status" Type="Edm.String" />
      </EntityType>
      <EntityContainer Name="Service">
        <EntitySet Name="Plans" EntityType="Temper.Broken.Plan" />
        <EntitySet Name="Tasks" EntityType="Temper.Broken.Task" />
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;
        let plan = r#"
[automaton]
name = "Plan"
states = ["Active"]
initial = "Active"

[[action]]
name = "AddTask"
kind = "input"
from = ["Active"]
params = ["title"]
effect = [{ type = "spawn", entity_type = "Task", entity_id_source = "{uuid}", initial_action = "Create" }]
"#;
        let task = r#"
[automaton]
name = "Task"
states = ["Open"]
initial = "Open"

[[action]]
name = "Create"
kind = "input"
from = ["Open"]
params = ["title", "description", "plan_id"]
"#;

        fs::write(specs_dir.join("model.csdl.xml"), csdl).expect("write csdl");
        fs::write(specs_dir.join("plan.ioa.toml"), plan).expect("write plan");
        fs::write(specs_dir.join("task.ioa.toml"), task).expect("write task");

        let result = run(specs_dir.to_str().expect("tmp path utf-8"));
        let err = result.expect_err("verify should fail on broken spawn contract");
        let msg = err.to_string();
        assert!(
            msg.contains("spawn_initial_action_params_unmapped"),
            "expected exact lint code in error, got: {msg}"
        );
    }

    #[test]
    fn test_multi_entity_dir_runs_composite_as_gating_step() {
        // A two-entity directory whose cross-entity reaction can be dropped:
        // Workspace.Freeze moves Workspace out of Active before File.Touch
        // fires Workspace.IncrementUsage (enabled only from Active). The
        // dropped reaction must FAIL the command (composite is gating).
        let tmp = tempfile::tempdir().expect("tempdir");
        let specs_dir = tmp.path();

        let csdl = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Fs" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="File">
        <Key><PropertyRef Name="Id" /></Key>
        <Property Name="Id" Type="Edm.Guid" Nullable="false" />
        <Property Name="status" Type="Edm.String" />
        <Property Name="workspace_id" Type="Edm.String" />
      </EntityType>
      <EntityType Name="Workspace">
        <Key><PropertyRef Name="Id" /></Key>
        <Property Name="Id" Type="Edm.Guid" Nullable="false" />
        <Property Name="status" Type="Edm.String" />
      </EntityType>
      <EntityContainer Name="Service">
        <EntitySet Name="Files" EntityType="Temper.Fs.File" />
        <EntitySet Name="Workspaces" EntityType="Temper.Fs.Workspace" />
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;
        let file = r#"
[automaton]
name = "File"
states = ["New", "Updated"]
initial = "New"

[[action]]
name = "Touch"
kind = "input"
from = ["New"]
to = "Updated"

[[action.triggers]]
name = "touch_increments_usage"
kind = "entity"
target_entity = "Workspace"
target_action = "IncrementUsage"

[action.triggers.resolve_target]
type = "field"
field = "workspace_id"
"#;
        let workspace = r#"
[automaton]
name = "Workspace"
states = ["Active", "Frozen"]
initial = "Active"

[[action]]
name = "IncrementUsage"
kind = "input"
from = ["Active"]
to = "Active"

[[action]]
name = "Freeze"
kind = "internal"
from = ["Active"]
to = "Frozen"
"#;

        fs::write(specs_dir.join("model.csdl.xml"), csdl).expect("write csdl");
        fs::write(specs_dir.join("file.ioa.toml"), file).expect("write file");
        fs::write(specs_dir.join("workspace.ioa.toml"), workspace).expect("write workspace");

        let result = run(specs_dir.to_str().expect("tmp path utf-8"));
        let err = result.expect_err("composite gating step must fail on a dropped reaction");
        let msg = err.to_string();
        assert!(
            msg.contains("composite cross-entity verification failed"),
            "expected composite gating failure, got: {msg}"
        );
        assert!(
            msg.contains("IncrementUsage") && msg.contains("Frozen"),
            "failure should name the dropped reaction + wrong state, got: {msg}"
        );
    }

    #[test]
    fn test_verify_fails_on_unguarded_related_field_sidecar() {
        let specs_dir = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/study/publish-needs-review"
        );
        let result = run(specs_dir);
        let err = result.expect_err("unguarded Publish must fail related-field check");
        let msg = err.to_string();
        assert!(
            msg.contains("PublishNeedsThisReviewRecorded"),
            "FAIL must name the sidecar row, got: {msg}"
        );
    }

    #[test]
    fn test_verify_passes_guarded_related_field_sidecar() {
        let specs_dir = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/study/publish-needs-review/fixed"
        );
        run(specs_dir).expect("Publish guarded on ReviewAgent VerdictRecorded must pass");
    }
}
