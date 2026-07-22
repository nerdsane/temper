use std::collections::BTreeMap;
use std::path::Path;

use axum::http::StatusCode;
use temper_spec::automaton::LintSeverity;
use temper_spec::cross_invariant::{
    CrossInvariantLintFinding, CrossInvariantLintSeverity, lint_cross_invariants,
    parse_cross_invariants,
};
use temper_spec::csdl::CsdlDocument;

use super::super::super::specs_helpers::{EntityLintFinding, lint_loaded_specs, to_pascal_case};

pub(super) struct LoadedSpecSources {
    pub(super) csdl_xml: String,
    pub(super) csdl: CsdlDocument,
    pub(super) ioa_sources: BTreeMap<String, String>,
    pub(super) cross_invariants_toml: Option<String>,
    pub(super) lint_findings: Vec<EntityLintFinding>,
    pub(super) cross_lint_findings: Vec<CrossInvariantLintFinding>,
    pub(super) ioa_lint_errors: usize,
    pub(super) ioa_lint_warnings: usize,
    pub(super) cross_lint_errors: usize,
    pub(super) cross_lint_warnings: usize,
}

pub(super) fn load_spec_sources(
    specs_path: &Path,
) -> Result<LoadedSpecSources, (StatusCode, String)> {
    let csdl_path = specs_path.join("model.csdl.xml");
    if !csdl_path.exists() {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("CSDL model not found at {}", csdl_path.display()),
        ));
    }

    let csdl_xml = std::fs::read_to_string(&csdl_path).map_err(|e| {
        // determinism-ok: HTTP handler reads spec files
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to read CSDL: {e}"),
        )
    })?;
    let csdl = temper_spec::csdl::parse_csdl(&csdl_xml).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            format!("Failed to parse CSDL: {e}"),
        )
    })?;

    let mut ioa_sources = BTreeMap::new();
    let entries = std::fs::read_dir(specs_path).map_err(|e| {
        // determinism-ok: HTTP handler reads spec directory
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to read specs directory: {e}"),
        )
    })?;
    for entry in entries {
        let entry = entry.map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to read directory entry: {e}"),
            )
        })?;
        let path = entry.path();
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if file_name.ends_with(".ioa.toml") {
            let entity_name = file_name.strip_suffix(".ioa.toml").unwrap_or_default();
            let entity_name = to_pascal_case(entity_name);
            let source = std::fs::read_to_string(&path).map_err(|e| {
                // determinism-ok: HTTP handler reads spec files
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Failed to read {}: {e}", path.display()),
                )
            })?;
            ioa_sources.insert(entity_name, source);
        }
    }
    if ioa_sources.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "No .ioa.toml files found in specs directory".to_string(),
        ));
    }

    let legacy_reactions_path = specs_path.join("reactions.toml");
    if legacy_reactions_path.exists() {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "Legacy {} is no longer supported; migrate to inline [[action.triggers]]",
                legacy_reactions_path.display()
            ),
        ));
    }

    let cross_invariants_toml = {
        let path = specs_path.join("cross-invariants.toml");
        if path.exists() {
            Some(std::fs::read_to_string(&path).map_err(|e| {
                // determinism-ok: HTTP handler reads cross-invariants
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Failed to read {}: {e}", path.display()),
                )
            })?)
        } else {
            None
        }
    };

    let lint_findings = lint_loaded_specs(&csdl, &ioa_sources)?;
    let cross_lint_findings = if let Some(source) = cross_invariants_toml.as_deref() {
        let spec = parse_cross_invariants(source).map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                format!("Failed to parse cross-invariants.toml: {e}"),
            )
        })?;
        lint_cross_invariants(&spec)
    } else {
        Vec::new()
    };

    let ioa_lint_errors = lint_findings
        .iter()
        .filter(|f| matches!(f.severity, LintSeverity::Error))
        .count();
    let ioa_lint_warnings = lint_findings
        .iter()
        .filter(|f| matches!(f.severity, LintSeverity::Warning))
        .count();
    let cross_lint_errors = cross_lint_findings
        .iter()
        .filter(|f| matches!(f.severity, CrossInvariantLintSeverity::Error))
        .count();
    let cross_lint_warnings = cross_lint_findings
        .iter()
        .filter(|f| matches!(f.severity, CrossInvariantLintSeverity::Warning))
        .count();

    Ok(LoadedSpecSources {
        csdl_xml,
        csdl,
        ioa_sources,
        cross_invariants_toml,
        lint_findings,
        cross_lint_findings,
        ioa_lint_errors,
        ioa_lint_warnings,
        cross_lint_errors,
        cross_lint_warnings,
    })
}
