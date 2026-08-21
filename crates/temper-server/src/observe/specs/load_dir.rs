use std::collections::BTreeMap;
use std::fs; // determinism-ok: authenticated server-local spec management boundary
use std::io::Read as _;
use std::path::{Path, PathBuf};

use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::response::Json;
use temper_authz::AuthenticatedRequestContext;
use temper_spec::automaton::LintSeverity;
use temper_spec::cross_invariant::{
    CrossInvariantLintSeverity, lint_cross_invariants, parse_cross_invariants,
};
use tracing::instrument;

use super::super::specs_helpers::{
    build_ndjson_response, cross_lint_ndjson_line, lint_loaded_specs, lint_ndjson_line,
    to_pascal_case,
};
use super::types::LoadDirRequest;
use super::verification_stream::build_verification_stream_response;
use crate::authz::{
    GovernedMutationAuth, require_authenticated_context, require_governed_mutation_auth,
    require_tenant_match,
};
use crate::state::ServerState;

const SPEC_DIRECTORY_PATH_BUDGET: usize = 4 * 1024;
const SPEC_DIRECTORY_ENTRY_BUDGET: usize = 512;
const SPEC_FILE_COUNT_BUDGET: usize = 128;
const SPEC_FILE_BYTE_BUDGET: usize = 1024 * 1024;
const SPEC_DIRECTORY_BYTE_BUDGET: usize = 16 * 1024 * 1024;

pub(super) struct ValidatedSpecDirectory {
    path: PathBuf,
    resource_id: String,
}

fn validate_spec_directory_request(requested: &str) -> Result<(), (StatusCode, String)> {
    if requested.trim().is_empty() || requested.len() > SPEC_DIRECTORY_PATH_BUDGET {
        return Err((
            StatusCode::BAD_REQUEST,
            "Specs directory path is empty or exceeds its budget".to_string(),
        ));
    }
    Ok(())
}

pub(super) fn validate_spec_directory(
    requested: &str,
) -> Result<ValidatedSpecDirectory, (StatusCode, String)> {
    validate_spec_directory_request(requested)?;
    let requested_path = Path::new(requested);
    let metadata = fs::symlink_metadata(requested_path).map_err(|error| {
        (
            StatusCode::BAD_REQUEST,
            format!("Specs directory is unavailable: {error}"),
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err((
            StatusCode::BAD_REQUEST,
            "Specs directory must be a real directory, not a file or symbolic link".to_string(),
        ));
    }
    let path = fs::canonicalize(requested_path).map_err(|error| {
        (
            StatusCode::BAD_REQUEST,
            format!("Specs directory cannot be canonicalized: {error}"),
        )
    })?;
    let resource_id = path.to_string_lossy().to_string();
    if resource_id.len() > SPEC_DIRECTORY_PATH_BUDGET {
        return Err((
            StatusCode::BAD_REQUEST,
            "Canonical specs directory path exceeds its budget".to_string(),
        ));
    }
    Ok(ValidatedSpecDirectory { path, resource_id })
}

/// POST /api/specs/load-dir -- hot-load specs from a server-local directory.
/// Reads CSDL and IOA files from `specs_dir`, registers them under `tenant`,
/// emits design-time SSE events for each entity, and spawns background
/// verification tasks that stream progress via SSE. The dedicated
/// `load_specs_from_directory` Cedar action is separate from inline spec
/// submission because it grants access to a host filesystem path.
#[instrument(skip_all, fields(otel.name = "POST /api/specs/load-dir"))]
pub(crate) async fn handle_load_dir(
    State(state): State<ServerState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    Json(body): Json<LoadDirRequest>,
) -> Result<axum::response::Response, (StatusCode, String)> {
    let authenticated = require_authenticated_context(authenticated.as_deref())
        .map_err(|status| (status, "authentication required".to_string()))?;
    require_tenant_match(authenticated, &body.tenant)
        .map_err(|status| (status, "credential tenant mismatch".to_string()))?;
    validate_spec_directory_request(&body.specs_dir)?;

    // Authorize the caller-controlled path before asking the host filesystem
    // whether it exists. A second check below binds authority to the canonical
    // target so aliases and symbolic links cannot redirect a permitted path.
    let requested_resource_attrs = BTreeMap::from([
        (
            "id".to_string(),
            serde_json::Value::String(body.specs_dir.clone()),
        ),
        (
            "targetTenant".to_string(),
            serde_json::Value::String(body.tenant.clone()),
        ),
        (
            "requestedPath".to_string(),
            serde_json::Value::String(body.specs_dir.clone()),
        ),
    ]);
    if let Some(denial) = require_governed_mutation_auth(
        &state,
        authenticated,
        GovernedMutationAuth {
            tenant: &body.tenant,
            action: "load_specs_from_directory",
            resource_type: "SpecDirectory",
            resource_id: &body.specs_dir,
            resource_attrs: requested_resource_attrs,
            module_name: None,
            from_status: None,
        },
    )
    .await
    {
        return Err(denial);
    }

    let directory = validate_spec_directory(&body.specs_dir)?;
    let resource_attrs = BTreeMap::from([
        (
            "id".to_string(),
            serde_json::Value::String(directory.resource_id.clone()),
        ),
        (
            "targetTenant".to_string(),
            serde_json::Value::String(body.tenant.clone()),
        ),
        (
            "canonicalPath".to_string(),
            serde_json::Value::String(directory.resource_id.clone()),
        ),
    ]);
    if let Some(denial) = require_governed_mutation_auth(
        &state,
        authenticated,
        GovernedMutationAuth {
            tenant: &body.tenant,
            action: "load_specs_from_directory",
            resource_type: "SpecDirectory",
            resource_id: &directory.resource_id,
            resource_attrs,
            module_name: None,
            from_status: None,
        },
    )
    .await
    {
        return Err(denial);
    }
    load_specs_from_directory(state, body, directory).await
}

fn read_bounded_spec_text(
    path: &Path,
    label: &str,
    remaining_bytes: &mut usize,
) -> Result<String, (StatusCode, String)> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        (
            StatusCode::BAD_REQUEST,
            format!("{label} is unavailable: {error}"),
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("{label} must be a regular file, not a symbolic link"),
        ));
    }
    let byte_length = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
    if byte_length > SPEC_FILE_BYTE_BUDGET || byte_length > *remaining_bytes {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("{label} exceeds the spec-loading byte budget"),
        ));
    }
    let read_budget = SPEC_FILE_BYTE_BUDGET.min(*remaining_bytes);
    let file = fs::File::open(path).map_err(|error| {
        (
            StatusCode::BAD_REQUEST,
            format!("Failed to open {label}: {error}"),
        )
    })?;
    let mut content = String::new();
    file.take((read_budget as u64).saturating_add(1))
        .read_to_string(&mut content)
        .map_err(|error| {
            (
                StatusCode::BAD_REQUEST,
                format!("Failed to read UTF-8 {label}: {error}"),
            )
        })?;
    if content.len() > byte_length || content.len() > read_budget {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("{label} changed or exceeded the spec-loading byte budget while reading"),
        ));
    }
    *remaining_bytes -= content.len();
    Ok(content)
}

pub(super) async fn load_specs_from_directory(
    state: ServerState,
    mut body: LoadDirRequest,
    directory: ValidatedSpecDirectory,
) -> Result<axum::response::Response, (StatusCode, String)> {
    let specs_path = &directory.path;
    body.specs_dir = directory.resource_id;
    let mut remaining_bytes = SPEC_DIRECTORY_BYTE_BUDGET;

    // Read CSDL model
    let csdl_path = specs_path.join("model.csdl.xml");
    let csdl_xml = read_bounded_spec_text(&csdl_path, "model.csdl.xml", &mut remaining_bytes)?;
    let csdl = temper_spec::csdl::parse_csdl(&csdl_xml).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            format!("Failed to parse CSDL: {e}"),
        )
    })?;

    // Read all *.ioa.toml files
    let mut ioa_sources: std::collections::BTreeMap<String, String> =
        std::collections::BTreeMap::new();
    let entries = fs::read_dir(specs_path).map_err(|e| {
        // determinism-ok: HTTP handler reads spec directory
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to read specs directory: {e}"),
        )
    })?;
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to read directory entry: {e}"),
            )
        })?;
        if paths.len() >= SPEC_DIRECTORY_ENTRY_BUDGET {
            return Err((
                StatusCode::PAYLOAD_TOO_LARGE,
                "Specs directory exceeds its entry budget".to_string(),
            ));
        }
        paths.push(entry.path());
    }
    paths.sort();
    let mut spec_file_count = 0usize;
    for path in paths {
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if file_name.ends_with(".ioa.toml") {
            spec_file_count += 1;
            if spec_file_count > SPEC_FILE_COUNT_BUDGET {
                return Err((
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "Specs directory exceeds its IOA file-count budget".to_string(),
                ));
            }
            let entity_name = file_name.strip_suffix(".ioa.toml").unwrap_or_default();
            let entity_name = to_pascal_case(entity_name);
            let source = read_bounded_spec_text(
                &path,
                &format!("IOA spec {}", path.display()),
                &mut remaining_bytes,
            )?;
            if ioa_sources.insert(entity_name.clone(), source).is_some() {
                return Err((
                    StatusCode::BAD_REQUEST,
                    format!("Multiple IOA filenames normalize to entity {entity_name}"),
                ));
            }
        }
    }

    if ioa_sources.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "No .ioa.toml files found in specs directory".to_string(),
        ));
    }

    let legacy_reactions_path = specs_path.join("reactions.toml");
    if fs::symlink_metadata(&legacy_reactions_path).is_ok() {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "Legacy {} is no longer supported; migrate to inline [[action.triggers]]",
                legacy_reactions_path.display()
            ),
        ));
    }

    // Optional cross-invariants.toml.
    let cross_invariants_toml = {
        let path = specs_path.join("cross-invariants.toml");
        match fs::symlink_metadata(&path) {
            Ok(_) => Some(read_bounded_spec_text(
                &path,
                "cross-invariants.toml",
                &mut remaining_bytes,
            )?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    format!("Unable to inspect cross-invariants.toml: {error}"),
                ));
            }
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

    state
        .audit_reference_contract_activation(
            &temper_runtime::tenant::TenantId::new(&body.tenant),
            &ioa_sources,
            10_000,
        )
        .await
        .map_err(|error| (StatusCode::CONFLICT, error))?;

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
                Vec::new(),
                cross_invariants_toml.clone(),
                body.merge,
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

        if let Ok(content) = fs::read_to_string(&registry_path) {
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
            let _ = fs::create_dir_all(&state.data_dir);
            let _ = fs::write(registry_path, encoded);
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
