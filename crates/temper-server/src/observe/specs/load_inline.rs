use std::path::{Component, Path, PathBuf};

use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::Json;
use serde_json::json;
use temper_evolution::records::{
    AnalysisRecord, ObservationClass, ObservationRecord, RecordHeader, RecordType, SolutionOption,
};
use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;
use temper_spec::csdl::parse_csdl;
use tracing::instrument;

use crate::authz::{DenialInput, record_authz_denial, security_context_from_headers};
use crate::state::{ServerState, TrajectoryEntry, TrajectorySource};

use super::load_dir::handle_load_dir;
use super::types::{LoadDirRequest, LoadInlineRequest};

/// POST /api/specs/load-inline -- load specs from inline content.
///
/// Accepts a JSON body with `tenant` and `specs` (map of filename -> content).
/// Cedar-gated: requires `submit_specs` action on `SpecRegistry` resource.
/// Records trajectory for every spec submission (success or denial).
#[instrument(skip_all, fields(otel.name = "POST /api/specs/load-inline"))]
pub(crate) async fn handle_load_inline(
    State(state): State<ServerState>,
    headers: HeaderMap,
    raw_body: String,
) -> Result<axum::response::Response, (StatusCode, String)> {
    let body: LoadInlineRequest = serde_json::from_str(&raw_body).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            format!(
                "Invalid load-inline request body: {e}. Ensure 'specs' is a map of filename strings to content strings."
            ),
        )
    })?;
    let tenant = body.tenant.clone();

    // Cedar authorization gate.
    let security_ctx = security_context_from_headers(&headers, None, None, None);
    let entity_names: Vec<String> = body
        .specs
        .keys()
        .filter(|k| k.ends_with(".ioa.toml"))
        .map(|k| k.strip_suffix(".ioa.toml").unwrap_or(k).to_string())
        .collect();

    let mut spec_resource_attrs = std::collections::BTreeMap::new();
    spec_resource_attrs.insert("id".to_string(), serde_json::json!(tenant));
    for (spec_key, spec_content) in &body.specs {
        if spec_key.ends_with(".ioa.toml")
            && let Ok(automaton) = temper_spec::automaton::parse_automaton(spec_content)
        {
            let metadata = automaton.extract_metadata();
            for (k, v) in metadata.to_flat_map() {
                spec_resource_attrs.insert(k, v);
            }
        }
    }

    let authz_result = state.authorize_with_context(
        &security_ctx,
        "submit_specs",
        "SpecRegistry",
        &spec_resource_attrs,
        &tenant,
    );
    if let Err(denial) = authz_result {
        let reason = denial.to_string();
        // Record denials and use the first decision ID as the primary chain anchor.
        // Record a single denial per authz check. The resource_id must match
        // what Cedar evaluated: SpecRegistry::"<tenant>".
        let pd = record_authz_denial(
            &state,
            DenialInput {
                tenant: &tenant,
                security_ctx: &security_ctx,
                agent_id_override: None,
                action: "submit_specs",
                resource_type: "SpecRegistry",
                resource_id: &tenant,
                resource_attrs: serde_json::json!({"entity_types": entity_names}),
                reason: &reason,
                module_name: None,
                from_status: None,
            },
        )
        .await;
        let primary_decision_id = pd.id.clone();
        let decision_ids = vec![pd.id];

        // Create O-Record for the denied spec submission.
        let o_record = ObservationRecord {
            header: RecordHeader::new(RecordType::Observation, "cedar:spec_submission"),
            source: "cedar:spec_submission".to_string(),
            classification: ObservationClass::AuthzDenied,
            evidence_query: format!(
                "Agent '{}' proposed spec for entity types: {:?}",
                security_ctx.principal.id, entity_names,
            ),
            threshold_field: None,
            threshold_value: None,
            observed_value: None,
            context: serde_json::json!({
                "agent_id": security_ctx.principal.id,
                "tenant": tenant,
                "entity_types": entity_names,
                "decision_id": primary_decision_id.clone(),
                "spec_metadata": spec_resource_attrs,
            }),
        };
        let o_id = o_record.header.id.clone();
        let data_json = serde_json::to_string(&o_record).unwrap_or_default();
        let _ = state
            .insert_evolution_record(
                &o_record.header.id,
                "Observation",
                &format!("{:?}", o_record.header.status),
                &o_record.header.created_by,
                o_record.header.derived_from.as_deref(),
                &data_json,
            )
            .await;

        // Create A-Record with the proposed spec as spec_diff.
        let spec_summary: String = body
            .specs
            .keys()
            .map(|k| k.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let a_record = AnalysisRecord {
            header: RecordHeader::new(RecordType::Analysis, "cedar:spec_submission")
                .derived_from(o_id),
            root_cause: format!(
                "Agent proposed new entity types ({}) but lacks Cedar permission.",
                spec_summary,
            ),
            options: vec![SolutionOption {
                description: "Approve spec submission via Observe UI".to_string(),
                spec_diff: serde_json::to_string_pretty(&body.specs).unwrap_or_default(),
                tla_impact: "NEW".to_string(),
                risk: temper_evolution::SolutionRisk::Low,
                complexity: temper_evolution::Complexity::Low,
            }],
            recommendation: Some(0),
        };
        let a_record_id = a_record.header.id.clone();
        let data_json = serde_json::to_string(&a_record).unwrap_or_default();
        let _ = state
            .insert_evolution_record(
                &a_record.header.id,
                "Analysis",
                &format!("{:?}", a_record.header.status),
                &a_record.header.created_by,
                a_record.header.derived_from.as_deref(),
                &data_json,
            )
            .await;

        // Link the PendingDecision to the A-Record for O-A-D chain tracing.
        // Decisions are persisted to the tenant store; the evolution_record_id
        // link will be available when the decision is read back from the durable backend.
        if let Some(store) = state.metadata_store_for_tenant(&tenant).await {
            for decision_id in &decision_ids {
                if let Ok(Some(data_str)) = store.get_pending_decision(decision_id).await
                    && let Ok(mut pd) =
                        serde_json::from_str::<crate::state::PendingDecision>(&data_str)
                {
                    pd.evolution_record_id = Some(a_record_id.clone());
                    let _ = state.persist_pending_decision(&pd).await;
                }
            }
        }

        return Err((
            StatusCode::FORBIDDEN,
            serde_json::json!({
                "error": {
                    "code": "AuthorizationDenied",
                    "message": format!("{reason} Decision {}", primary_decision_id),
                }
            })
            .to_string(),
        ));
    }

    // Write specs to a private, per-request temp directory. The unpredictable
    // suffix (ARN-229) means a local attacker can't pre-plant a symlink at a fixed
    // path to redirect the writes, and nothing is shared across requests/tenants.
    let inline_id = uuid::Uuid::now_v7(); // determinism-ok: HTTP handler needs an unpredictable per-request temp dir
    // The tenant comes from the request body and is interpolated into the leaf
    // name, so require the leaf to be a single safe path component — a tenant with
    // `/` (permitted by TenantId::new) or `..` would otherwise create a predictable
    // intermediate dir or escape the temp root, reopening the symlink / arbitrary
    // write primitive this fix closes (ARN-229).
    let tmp_dir = safe_temp_dir_leaf(
        &std::env::temp_dir(),
        &format!("temper-inline-{tenant}-{inline_id}"),
    )?;
    #[rustfmt::skip]
    std::fs::create_dir_all(&tmp_dir).map_err(|e| { // determinism-ok: HTTP handler creates temp dir
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to create temp dir: {e}"),
        )
    })?;

    let specs_root = resolve_inline_specs_root(&tmp_dir, &body.specs)?;

    for (filename, content) in &body.specs {
        let path = safe_inline_spec_path(&tmp_dir, filename)?;
        if let Some(parent) = path.parent() {
            #[rustfmt::skip]
            std::fs::create_dir_all(parent).map_err(|e| { // determinism-ok: HTTP handler creates temp subdirectories for nested inline specs
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Failed to create parent directory for {filename}: {e}"),
                )
            })?;
        }
        #[rustfmt::skip]
        std::fs::write(&path, content).map_err(|e| { // determinism-ok: HTTP handler writes specs
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to write {filename}: {e}"),
            )
        })?;
    }

    if let Some(source) = body.cross_invariants_toml.as_deref() {
        #[rustfmt::skip]
        std::fs::write(specs_root.join("cross-invariants.toml"), source).map_err(|e| { // determinism-ok: HTTP handler writes cross-invariants
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to write cross-invariants.toml: {e}"),
            )
        })?;
    }

    // Delegate to load-dir logic with merge=true so agent-submitted specs
    // are added to the existing tenant config instead of replacing it.
    let cedar_policies = body.cedar_policies.clone();
    let dir_request = LoadDirRequest {
        tenant: tenant.clone(),
        specs_dir: specs_root.to_string_lossy().to_string(),
        merge: true,
    };
    let result = handle_load_dir(State(state.clone()), Json(dir_request)).await;

    // The loader has read the specs into the registry; the per-request dir is no
    // longer needed. Best-effort cleanup so unique temp dirs don't accumulate.
    let _ = std::fs::remove_dir_all(&tmp_dir); // determinism-ok: HTTP handler removes its private temp dir after loading

    if result.is_ok()
        && let Some(ref cedar_text) = cedar_policies
        && !cedar_text.trim().is_empty()
    {
        if let Err(e) = cedar_text.parse::<cedar_policy::PolicySet>() {
            tracing::warn!(error = %e, "bundled Cedar policies failed to parse, skipping");
        } else {
            // Update the in-memory text cache.
            if let Ok(mut policies) = state.tenant_policies.write() {
                let entry = policies.entry(tenant.clone()).or_default();
                *entry = merge_inline_cedar_policy_text(entry, cedar_text);
            }
            // Reload the per-tenant Cedar policy set.
            let full_text = state
                .tenant_policies
                .read()
                .ok()
                .and_then(|p| p.get(&tenant).cloned())
                .unwrap_or_default();
            if let Err(e) = state.authz.reload_tenant_policies(&tenant, &full_text) {
                tracing::error!(error = %e, "failed to reload policies with bundled Cedar");
            } else {
                tracing::info!(tenant = %tenant, "bundled Cedar policies loaded successfully");
            }
        }
    }

    if result.is_ok() {
        let warning_context = build_adr_warning_context(&state, &body, &tenant).await;
        for entity_name in &entity_names {
            let traj = TrajectoryEntry {
                timestamp: sim_now().to_rfc3339(),
                tenant: tenant.clone(),
                entity_type: entity_name.clone(),
                entity_id: String::new(),
                action: "SubmitSpec".to_string(),
                success: true,
                from_status: None,
                to_status: None,
                error: None,
                agent_id: Some(security_ctx.principal.id.clone()),
                session_id: None,
                authz_denied: None,
                denied_resource: None,
                denied_module: None,
                source: Some(TrajectorySource::Entity),
                spec_governed: None,
                agent_type: None,
                request_body: warning_context.clone(),
                intent: None,
                matched_policy_ids: None,
            };
            if !state.enqueue_trajectory_entry(traj) {
                tracing::warn!("failed to enqueue spec submission trajectory");
            }
        }
    }

    result
}

async fn build_adr_warning_context(
    state: &ServerState,
    body: &LoadInlineRequest,
    tenant: &str,
) -> Option<serde_json::Value> {
    let namespaces = extract_submitted_namespaces(&body.specs);
    let candidate_paths = adr_candidate_paths(body.app_name.as_deref(), &namespaces);
    if candidate_paths.is_empty() {
        return None;
    }

    let hits = find_existing_adr_paths(state, tenant, &candidate_paths).await;
    if !hits.is_empty() {
        return None;
    }

    tracing::warn!(
        tenant,
        app_name = body.app_name.as_deref().unwrap_or(""),
        namespaces = ?namespaces,
        candidate_paths = ?candidate_paths,
        "Spec submitted with no ADRs — design decisions should be recorded under /apps/<app>/adrs/"
    );

    Some(json!({
        "warnings": [{
            "code": "missing_adrs",
            "message": "Spec submitted with no ADRs — design decisions should be recorded under /apps/<app>/adrs/.",
            "candidate_paths": candidate_paths,
            "namespaces": namespaces,
            "app_name": body.app_name,
        }]
    }))
}

fn extract_submitted_namespaces(specs: &std::collections::BTreeMap<String, String>) -> Vec<String> {
    let mut namespaces = std::collections::BTreeSet::new();
    for (filename, content) in specs {
        if !filename.ends_with(".csdl.xml") {
            continue;
        }
        if let Ok(document) = parse_csdl(content) {
            for schema in document.schemas {
                if schema.namespace.ends_with(".Vocab") || schema.namespace == "Temper.Vocab" {
                    continue;
                }
                namespaces.insert(schema.namespace);
            }
        }
    }
    namespaces.into_iter().collect()
}

fn resolve_inline_specs_root(
    tmp_dir: &Path,
    specs: &std::collections::BTreeMap<String, String>,
) -> Result<PathBuf, (StatusCode, String)> {
    let model_paths: Vec<&str> = specs
        .keys()
        .filter_map(|path| path.ends_with("model.csdl.xml").then_some(path.as_str()))
        .collect();

    if model_paths.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "Inline spec submission must include model.csdl.xml".to_string(),
        ));
    }

    if model_paths.len() > 1 {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "Inline spec submission must contain exactly one model.csdl.xml, found {}",
                model_paths.len()
            ),
        ));
    }

    // Validate the model path the same way as every other spec key, then take the
    // parent of the *validated* path so `specs_root` is always under `tmp_dir`.
    let validated_model = safe_inline_spec_path(tmp_dir, model_paths[0])?;
    Ok(validated_model
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| tmp_dir.to_path_buf()))
}

/// Resolve a caller-supplied inline-spec filename to a path guaranteed to stay
/// under `tmp_dir`. The `specs` keys come from the request body, so a traversal
/// (`../`) or absolute name must be rejected before any filesystem write escapes
/// the private temp dir (ARN-229).
fn safe_inline_spec_path(tmp_dir: &Path, filename: &str) -> Result<PathBuf, (StatusCode, String)> {
    let rel = Path::new(filename);
    if rel.as_os_str().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "inline spec path must not be empty".to_string(),
        ));
    }
    // Every component must be a plain name: `..` is `ParentDir`, an absolute path
    // starts with `RootDir`/`Prefix`, `.` is `CurDir` — all rejected, so the join
    // can only ever land under `tmp_dir`. Nested relative paths (app subdirs) are
    // allowed, since inline submissions legitimately carry them.
    let mut safe = PathBuf::new();
    for component in rel.components() {
        match component {
            Component::Normal(part) => safe.push(part),
            _ => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    format!(
                        "inline spec path '{filename}' must be a relative path with no '..', \
                         '.', or absolute segments"
                    ),
                ));
            }
        }
    }
    Ok(tmp_dir.join(safe))
}

/// Resolve the per-request temp-dir leaf name to a direct child of `temp_root`.
/// Unlike `safe_inline_spec_path` (which permits nested spec paths), the leaf must
/// be exactly ONE `Component::Normal`: it embeds the caller-supplied tenant, and a
/// tenant containing `/` would make only the final component uuid-suffixed, leaving
/// a *predictable* intermediate dir a local attacker could pre-plant a symlink at
/// (ARN-229). A single component keeps the whole dir unpredictable.
fn safe_temp_dir_leaf(temp_root: &Path, leaf: &str) -> Result<PathBuf, (StatusCode, String)> {
    let mut components = Path::new(leaf).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(part)), None) => Ok(temp_root.join(part)),
        _ => Err((
            StatusCode::BAD_REQUEST,
            "invalid inline spec request: tenant must be a single safe path component \
             (no '/', '..', or absolute segments)"
                .to_string(),
        )),
    }
}

fn merge_inline_cedar_policy_text(existing: &str, incoming: &str) -> String {
    let mut policy_text = existing.trim_end().to_string();
    let incoming = incoming.trim();
    if incoming.is_empty() || policy_text.contains(incoming) {
        return policy_text;
    }
    if !policy_text.is_empty() {
        policy_text.push('\n');
    }
    policy_text.push_str(incoming);
    policy_text
}

fn adr_candidate_paths(app_name: Option<&str>, namespaces: &[String]) -> Vec<String> {
    let mut candidates = std::collections::BTreeSet::new();
    if let Some(app_name) = app_name {
        let normalized = normalize_app_slug(app_name);
        if !normalized.is_empty() {
            candidates.insert(format!("/apps/{normalized}/adrs/"));
        }
    }

    for namespace in namespaces {
        for candidate in namespace_to_app_candidates(namespace) {
            if !candidate.is_empty() {
                candidates.insert(format!("/apps/{candidate}/adrs/"));
            }
        }
    }

    candidates.into_iter().collect()
}

fn namespace_to_app_candidates(namespace: &str) -> Vec<String> {
    let parts: Vec<&str> = namespace
        .split('.')
        .filter(|part| !part.is_empty())
        .collect();
    if parts.is_empty() {
        return Vec::new();
    }

    let full = normalize_app_slug(namespace);
    let remainder = if parts.len() > 1 {
        kebab_join(&parts[1..])
    } else {
        normalize_app_slug(parts[0])
    };

    let mut candidates = std::collections::BTreeSet::new();
    if !full.is_empty() {
        candidates.insert(full);
    }
    if !remainder.is_empty() {
        candidates.insert(remainder.clone());
    }
    match parts[0].to_ascii_lowercase().as_str() {
        "openpaw" | "paw" => {
            if !remainder.is_empty() {
                candidates.insert(format!("paw-{remainder}"));
            }
        }
        "temper" => {
            if !remainder.is_empty() {
                candidates.insert(format!("temper-{remainder}"));
            }
        }
        _ => {}
    }
    candidates.into_iter().collect()
}

fn kebab_join(parts: &[&str]) -> String {
    parts
        .iter()
        .map(|part| normalize_app_slug(part))
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

fn normalize_app_slug(value: &str) -> String {
    let mut slug = String::new();
    let mut prev_was_sep = true;
    let mut prev_was_lower_or_digit = false;

    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            let is_upper = ch.is_ascii_uppercase();
            if is_upper && prev_was_lower_or_digit && !prev_was_sep {
                slug.push('-');
            }
            slug.push(ch.to_ascii_lowercase());
            prev_was_sep = false;
            prev_was_lower_or_digit = ch.is_ascii_lowercase() || ch.is_ascii_digit();
        } else if !prev_was_sep {
            slug.push('-');
            prev_was_sep = true;
            prev_was_lower_or_digit = false;
        }
    }

    slug.trim_matches('-').to_string()
}

async fn find_existing_adr_paths(
    state: &ServerState,
    tenant: &str,
    candidate_paths: &[String],
) -> Vec<String> {
    let tenant_id = TenantId::new(tenant);
    let has_files = {
        let registry = state.registry.read().unwrap(); // ci-ok: infallible lock
        registry.get_spec(&tenant_id, "File").is_some()
    };
    if !has_files {
        return Vec::new();
    }

    let mut hits = Vec::new();
    for file_id in state.list_entity_ids(&tenant_id, "File") {
        let Ok(resp) = state
            .get_tenant_entity_state(&tenant_id, "File", &file_id)
            .await
        else {
            continue;
        };
        if resp.state.status == "Archived" {
            continue;
        }
        let path = resp
            .state
            .fields
            .get("Path")
            .and_then(|value| value.as_str())
            .or_else(|| {
                resp.state
                    .fields
                    .get("path")
                    .and_then(|value| value.as_str())
            });
        let Some(path) = path else { continue };
        if candidate_paths
            .iter()
            .any(|prefix| path.starts_with(prefix))
        {
            hits.push(path.to_string());
        }
    }
    hits.sort();
    hits.dedup();
    hits
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{
        adr_candidate_paths, merge_inline_cedar_policy_text, namespace_to_app_candidates,
        normalize_app_slug, safe_inline_spec_path, safe_temp_dir_leaf,
    };

    #[test]
    fn safe_inline_spec_path_rejects_traversal_and_absolute() {
        // ARN-229: `specs` keys come from the request body. A traversal or absolute
        // key must be rejected before it escapes the private temp dir.
        let root = Path::new("/var/tmp/temper-inline-abc");
        safe_inline_spec_path(root, "../../etc/cron.d/evil")
            .expect_err("traversal key must be rejected");
        safe_inline_spec_path(root, "/etc/passwd").expect_err("absolute key must be rejected");
        safe_inline_spec_path(root, "..").expect_err("parent key must be rejected");
        safe_inline_spec_path(root, "").expect_err("empty key must be rejected");
        safe_inline_spec_path(root, "a/../../b").expect_err("mid-path traversal must be rejected");
        // Legitimate relative spec paths (including nested app dirs) are allowed.
        assert_eq!(
            safe_inline_spec_path(root, "model.csdl.xml").expect("simple key allowed"),
            root.join("model.csdl.xml")
        );
        assert_eq!(
            safe_inline_spec_path(root, "app/order.ioa.toml").expect("nested key allowed"),
            root.join("app/order.ioa.toml")
        );
    }

    #[test]
    fn safe_temp_dir_leaf_requires_single_component() {
        // ARN-229: the tenant is interpolated into the temp-dir leaf name, which must
        // stay a single unpredictable directory.
        let temp_root = Path::new("/var/folders/tmp");
        // Traversal tenant escapes the temp root.
        safe_temp_dir_leaf(temp_root, "temper-inline-a/../../etc-01890000")
            .expect_err("tenant traversal in the leaf name must be rejected");
        // A tenant containing `/` (permitted by TenantId::new) would leave a
        // predictable intermediate dir — reject the nested leaf.
        safe_temp_dir_leaf(temp_root, "temper-inline-evil/x-01890000")
            .expect_err("nested leaf (tenant with '/') must be rejected");
        // A normal tenant yields a single child dir under the temp root.
        assert_eq!(
            safe_temp_dir_leaf(temp_root, "temper-inline-default-01890000")
                .expect("normal tenant leaf allowed"),
            temp_root.join("temper-inline-default-01890000")
        );
    }

    #[test]
    fn normalize_app_slug_kebab_cases_namespaces() {
        assert_eq!(
            normalize_app_slug("Temper.ProjectManagement"),
            "temper-project-management"
        );
        assert_eq!(normalize_app_slug("OpenPaw"), "open-paw");
        assert_eq!(normalize_app_slug("llm-wiki"), "llm-wiki");
    }

    #[test]
    fn namespace_to_app_candidates_adds_platform_aware_variants() {
        let paw = namespace_to_app_candidates("OpenPaw.Foresight");
        assert!(paw.contains(&"open-paw-foresight".to_string()));
        assert!(paw.contains(&"foresight".to_string()));
        assert!(paw.contains(&"paw-foresight".to_string()));

        let temper = namespace_to_app_candidates("Temper.ProjectManagement");
        assert!(temper.contains(&"temper-project-management".to_string()));
        assert!(temper.contains(&"project-management".to_string()));
    }

    #[test]
    fn adr_candidate_paths_prefers_explicit_app_name() {
        let paths =
            adr_candidate_paths(Some("llm-wiki"), &["Temper.ProjectManagement".to_string()]);
        assert!(paths.contains(&"/apps/llm-wiki/adrs/".to_string()));
        assert!(paths.contains(&"/apps/project-management/adrs/".to_string()));
    }

    #[test]
    fn inline_cedar_policy_merge_deduplicates_repeated_bundle_text() {
        let policy = "permit(principal, action, resource);";
        let merged_once = merge_inline_cedar_policy_text("", policy);
        let merged_twice = merge_inline_cedar_policy_text(&merged_once, policy);

        assert_eq!(merged_twice, policy);
    }

    #[test]
    fn inline_cedar_policy_merge_preserves_distinct_existing_policy() {
        let existing = "permit(principal, action == Action::\"read\", resource);";
        let incoming = "permit(principal, action == Action::\"write\", resource);";

        assert_eq!(
            merge_inline_cedar_policy_text(existing, incoming),
            format!("{existing}\n{incoming}")
        );
    }
}
