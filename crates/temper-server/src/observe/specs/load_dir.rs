use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Json;
use temper_spec::automaton::LintSeverity;
use temper_spec::cross_invariant::{
    CrossInvariantLintSeverity, lint_cross_invariants, parse_cross_invariants,
};

use crate::reaction::registry::parse_reactions;
use crate::state::ServerState;

use super::super::specs_helpers::{
    build_ndjson_response, cross_lint_ndjson_line, lint_loaded_specs, lint_ndjson_line,
    to_pascal_case,
};
use super::types::LoadDirRequest;
use super::verification_stream::build_verification_stream_response;

/// POST /api/specs/load-dir -- hot-load specs from a directory into the running server.///
/// Reads CSDL and IOA files from `specs_dir`, registers them under `tenant`,
/// emits design-time SSE events for each entity, and spawns background
/// verification tasks that stream progress via SSE.
pub(crate) async fn handle_load_dir(
    State(state): State<ServerState>,
    Json(body): Json<LoadDirRequest>,
) -> Result<axum::response::Response, (StatusCode, String)> {
    let specs_path = std::path::Path::new(&body.specs_dir);

    if !specs_path.is_dir() {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("Specs directory not found: {}", specs_path.display()),
        ));
    }

    // Read CSDL model
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

    // Read all *.ioa.toml files
    let mut ioa_sources: std::collections::BTreeMap<String, String> =
        std::collections::BTreeMap::new();
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

    // Optional reactions.toml.
    let reactions = {
        let path = specs_path.join("reactions.toml");
        if path.exists() {
            let source = std::fs::read_to_string(&path).map_err(|e| {
                // determinism-ok: HTTP handler reads reactions file
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Failed to read {}: {e}", path.display()),
                )
            })?;
            parse_reactions(&source).map_err(|e| {
                (
                    StatusCode::BAD_REQUEST,
                    format!("Failed to parse {}: {e}", path.display()),
                )
            })?
        } else {
            Vec::new()
        }
    };

    // Optional cross-invariants.toml.
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
    let lint_errors = ioa_lint_errors + cross_lint_errors;
    let lint_warnings = ioa_lint_warnings + cross_lint_warnings;

    // Register names once so both failure and success paths can report them.
    let entity_names: Vec<String> = ioa_sources.keys().cloned().collect();

    // Abort early on lint errors (no persistence, no registry registration).
    if lint_errors > 0 {
        let mut lines = vec![serde_json::json!({
            "type": "specs_loaded",
            "tenant": &body.tenant,
            "entities": &entity_names,
        })];
        lines.extend(lint_findings.iter().map(lint_ndjson_line));
        lines.extend(cross_lint_findings.iter().map(cross_lint_ndjson_line));
        lines.push(serde_json::json!({
            "type": "summary",
            "tenant": &body.tenant,
            "all_passed": false,
            "lint_errors": lint_errors,
            "lint_warnings": lint_warnings,
            "ioa_lint_errors": ioa_lint_errors,
            "ioa_lint_warnings": ioa_lint_warnings,
            "cross_lint_errors": cross_lint_errors,
            "cross_lint_warnings": cross_lint_warnings,
        }));
        return build_ndjson_response(StatusCode::BAD_REQUEST, lines);
    }

    // Persist loaded specs first when Postgres is configured.
    let csdl_xml_for_db = csdl_xml.clone();
    for (entity_type, ioa_source) in &ioa_sources {
        state
            .upsert_spec_source(&body.tenant, entity_type, ioa_source, &csdl_xml_for_db)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    }
    state
        .upsert_tenant_constraints(&body.tenant, cross_invariants_toml.as_deref())
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;

    // Register into shared registry after persistence succeeds.
    let ioa_pairs: Vec<(&str, &str)> = ioa_sources
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    {
        let mut registry = state.registry.write().unwrap(); // ci-ok: infallible lock
        registry
            .try_register_tenant_with_reactions_and_constraints(
                body.tenant.as_str(),
                csdl,
                csdl_xml,
                &ioa_pairs,
                reactions,
                cross_invariants_toml.clone(),
            )
            .map_err(|e| {
                (
                    StatusCode::BAD_REQUEST,
                    format!("Failed to register specs: {e}"),
                )
            })?;
    }
    state.rebuild_reaction_dispatcher();

    if !state.data_dir.as_os_str().is_empty() {
        let registry_path = state.data_dir.join("specs-registry.json");
        let mut specs_registry = std::collections::BTreeMap::<String, String>::new();

        if let Ok(content) = std::fs::read_to_string(&registry_path) {
            // determinism-ok: HTTP handler reads specs registry
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&content)
                && let Some(obj) = value.as_object()
            {
                for (tenant, specs_dir) in obj {
                    if let Some(specs_dir) = specs_dir.as_str() {
                        specs_registry.insert(tenant.clone(), specs_dir.to_string());
                    }
                }
            }
        }

        specs_registry.insert(body.tenant.clone(), body.specs_dir.clone());

        if let Ok(encoded) = serde_json::to_string_pretty(&specs_registry) {
            let _ = std::fs::create_dir_all(&state.data_dir); // determinism-ok: HTTP handler creates data dir
            let _ = std::fs::write(registry_path, encoded); // determinism-ok: HTTP handler writes specs registry
        }
    }

    // Stream NDJSON response: verification runs inline and results are streamed per-entity.
    // Any agent calling this endpoint gets verification results without polling.
    let lint_warning_lines: Vec<serde_json::Value> = lint_findings
        .into_iter()
        .filter(|f| matches!(f.severity, LintSeverity::Warning))
        .map(|f| lint_ndjson_line(&f))
        .collect();
    let cross_lint_warning_lines: Vec<serde_json::Value> = cross_lint_findings
        .into_iter()
        .filter(|f| matches!(f.severity, CrossInvariantLintSeverity::Warning))
        .map(|f| cross_lint_ndjson_line(&f))
        .collect();

    Ok(build_verification_stream_response(
        state,
        body.tenant,
        entity_names,
        ioa_sources,
        lint_warning_lines,
        cross_lint_warning_lines,
    ))
}
