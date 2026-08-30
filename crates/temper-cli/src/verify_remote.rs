//! Remote verification command for `temper verify-remote`.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::Write as _;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;
use temper_spec::automaton::{
    LintSeverity, lint_automata_bundle, lint_automata_csdl_bundle, lint_automaton,
};
use temper_spec::csdl::{CsdlDocument, parse_csdl};

use crate::util::to_pascal_case;

#[derive(Serialize)]
struct ValidateIoaRequest<'a> {
    ioa_source: &'a str,
    sim_seeds: u64,
    prop_test_cases: u32,
}

/// Lint local IOA specs, then ask a running Temper server to run its cascade.
pub async fn run(
    specs_dir: &str,
    base_url: &str,
    sim_seeds: u64,
    prop_test_cases: u32,
) -> Result<()> {
    let specs_path = Path::new(specs_dir);
    write_stdout_line("Remote verification");
    write_stdout_line(format!("  Specs directory: {}", specs_path.display()));
    write_stdout_line(format!("  Server: {}", base_url.trim_end_matches('/')));

    let ioa_sources = read_ioa_sources(specs_path)?;
    if ioa_sources.is_empty() {
        anyhow::bail!("No .ioa.toml files found in {}", specs_path.display());
    }

    let csdl_path = specs_path.join("model.csdl.xml");
    let csdl_xml = fs::read_to_string(&csdl_path)
        .with_context(|| format!("Failed to read {}", csdl_path.display()))?;
    let csdl = parse_csdl(&csdl_xml)
        .with_context(|| format!("Failed to parse CSDL from {}", csdl_path.display()))?;
    lint_ioa_sources(&ioa_sources, &csdl)?;

    let endpoint = format!("{}/api/specs/validate-ioa", base_url.trim_end_matches('/'));
    let client = reqwest::Client::new();
    for (entity_name, ioa_source) in &ioa_sources {
        write_stdout_line(format!("\n  Remote verifying {entity_name}..."));
        let response = client
            .post(&endpoint)
            .header("X-Temper-Principal-Kind", "admin")
            .json(&ValidateIoaRequest {
                ioa_source,
                sim_seeds,
                prop_test_cases,
            })
            .send()
            .await
            .with_context(|| format!("failed to call {endpoint}"))?;

        let status = response.status();
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "<unreadable response body>".to_string());
        if !status.is_success() {
            anyhow::bail!("remote verification request failed for {entity_name}: {status} {body}");
        }

        let result: temper_verify::cascade::CascadeResult = serde_json::from_str(&body)
            .with_context(|| {
                format!("failed to decode remote verification result for {entity_name}")
            })?;
        for level in &result.levels {
            let status = if level.passed { "PASS" } else { "FAIL" };
            write_stdout_line(format!("    [{status}] {}", level.summary));
        }
        if !result.all_passed {
            anyhow::bail!("remote IOA verification failed for entity '{entity_name}'");
        }
    }

    write_stdout_line("\nRemote IOA verification cascade: ALL PASSED");
    Ok(())
}

fn lint_ioa_sources(ioa_sources: &HashMap<String, String>, csdl: &CsdlDocument) -> Result<()> {
    let mut parsed_automata = BTreeMap::new();
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
                        "{entity_name}: {} - {}",
                        finding.code, finding.message
                    ));
                    write_stdout_line(format!(
                        "\n  [lint:error] {entity_name}: {} - {}",
                        finding.code, finding.message
                    ));
                }
                LintSeverity::Warning => {
                    write_stdout_line(format!(
                        "\n  [lint:warn] {entity_name}: {} - {}",
                        finding.code, finding.message
                    ));
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
                    "{}: {} - {}",
                    finding.entity, finding.code, finding.message
                ));
                write_stdout_line(format!(
                    "\n  [lint:error] {}: {} - {}",
                    finding.entity, finding.code, finding.message
                ));
            }
            LintSeverity::Warning => {
                write_stdout_line(format!(
                    "\n  [lint:warn] {}: {} - {}",
                    finding.entity, finding.code, finding.message
                ));
            }
        }
    }

    for finding in lint_automata_csdl_bundle(&parsed_automata, csdl) {
        match finding.severity {
            LintSeverity::Error => {
                lint_error_count += 1;
                lint_error_lines.push(format!(
                    "{}: {} - {}",
                    finding.entity, finding.code, finding.message
                ));
                write_stdout_line(format!(
                    "\n  [lint:error] {}: {} - {}",
                    finding.entity, finding.code, finding.message
                ));
            }
            LintSeverity::Warning => write_stdout_line(format!(
                "\n  [lint:warn] {}: {} - {}",
                finding.entity, finding.code, finding.message
            )),
        }
    }

    if lint_error_count > 0 {
        anyhow::bail!(
            "IOA lint failed with {lint_error_count} error(s): {}",
            lint_error_lines.join(" | ")
        );
    }

    Ok(())
}

fn write_stdout_line(message: impl std::fmt::Display) {
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{message}");
}

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
