use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Json;
use sha2::{Digest, Sha256};
use temper_spec::automaton::LintSeverity;
use temper_spec::cross_invariant::CrossInvariantLintSeverity;
use tracing::instrument;

use super::super::specs_helpers::{
    build_ndjson_response, cross_lint_ndjson_line, lint_ndjson_line,
};
use super::types::LoadDirRequest;
use super::verification_stream::build_verification_stream_response;
use crate::platform_store::{
    PolicyEntryPublication, PolicyGenerationPublication, SpecPublication, SpecPublicationMode,
    TenantConstraintsPublication, TenantPolicyPublication,
};
use crate::state::ServerState;

mod source_loading;

use source_loading::{LoadedSpecSources, load_spec_sources};

/// POST /api/specs/load-dir -- hot-load specs from a directory into the running server.///
/// Reads CSDL and IOA files from `specs_dir`, registers them under `tenant`,
/// emits design-time SSE events for each entity, and spawns background
/// verification tasks that stream progress via SSE.
#[instrument(skip_all, fields(otel.name = "POST /api/specs/load-dir"))]
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

    let LoadedSpecSources {
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
    } = load_spec_sources(specs_path)?;
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

    let ioa_pairs: Vec<(&str, &str)> = ioa_sources
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let tenant_id = temper_runtime::tenant::TenantId::new(&body.tenant);
    let mut publication_guard = state
        .begin_spec_publication(&tenant_id)
        .await
        .map_err(|error| (StatusCode::SERVICE_UNAVAILABLE, error))?;
    let incoming_cedar_policy = body
        .cedar_policies
        .as_deref()
        .map(str::trim)
        .filter(|source| !source.is_empty())
        .map(str::to_string);
    let mut owned_policy_entries = Vec::<(String, String, String)>::new();
    let mut policy_owner = None;
    let complete_cedar_policy = match incoming_cedar_policy.as_deref() {
        Some(incoming) => {
            incoming
                .parse::<cedar_policy::PolicySet>()
                .map_err(|error| {
                    (
                        StatusCode::BAD_REQUEST,
                        format!("Bundled Cedar policies failed to parse: {error}"),
                    )
                })?;
            let cached = state
                .tenant_policies
                .read()
                .map_err(|error| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("tenant policy lock poisoned: {error}"),
                    )
                })?
                .get(&body.tenant)
                .cloned()
                .unwrap_or_default();
            let source_digest = format!("{:x}", Sha256::digest(body.specs_dir.as_bytes()));
            let owner = format!("observe:load-dir:{source_digest}");
            let policy_id = format!("observe-load-dir-{source_digest}");
            let complete = if let Some(store) = state.policy_store() {
                let rows = store
                    .load_policies_for_tenant(&body.tenant)
                    .await
                    .map_err(|error| {
                        (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            format!("Failed to load durable Cedar policy generation: {error}"),
                        )
                    })?;
                let had_canonical_rows = !rows.is_empty();
                let mut complete_rows = rows
                    .into_iter()
                    .filter(|row| row.created_by != owner)
                    .map(|row| (row.policy_id, row.cedar_text, row.created_by, row.enabled))
                    .collect::<Vec<_>>();
                if !had_canonical_rows {
                    let legacy = store
                        .load_policy_compatibility_text(&body.tenant)
                        .await
                        .map_err(|error| {
                            (
                                StatusCode::INTERNAL_SERVER_ERROR,
                                format!("Failed to load legacy Cedar policy generation: {error}"),
                            )
                        })?
                        .unwrap_or(cached);
                    if !legacy.trim().is_empty() {
                        let primary = (
                            "primary".to_string(),
                            legacy.trim().to_string(),
                            "legacy-migration".to_string(),
                        );
                        complete_rows.push((
                            primary.0.clone(),
                            primary.1.clone(),
                            primary.2.clone(),
                            true,
                        ));
                        owned_policy_entries.push(primary);
                    }
                }
                complete_rows.push((policy_id.clone(), incoming.to_string(), owner.clone(), true));
                complete_rows.sort_by(|left, right| left.0.cmp(&right.0));
                complete_rows
                    .into_iter()
                    .filter(|(_, _, _, enabled)| *enabled)
                    .map(|(_, cedar_text, _, _)| cedar_text)
                    .collect::<Vec<_>>()
                    .join("\n")
            } else {
                super::load_inline::merge_inline_cedar_policy_text(&cached, incoming)
            };
            owned_policy_entries.push((policy_id, incoming.to_string(), owner.clone()));
            policy_owner = Some(owner);
            complete
                .parse::<cedar_policy::PolicySet>()
                .map_err(|error| {
                    (
                        StatusCode::BAD_REQUEST,
                        format!("Complete tenant Cedar policies failed to parse: {error}"),
                    )
                })?;
            Some(complete)
        }
        None => None,
    };
    let incoming_types = ioa_sources
        .keys()
        .map(String::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    let mut removed_entity_types = if body.merge {
        std::collections::BTreeSet::new()
    } else {
        state
            .registry
            .read()
            .expect("registry lock poisoned")
            .entity_types(&tenant_id)
            .into_iter()
            .filter(|entity_type| !incoming_types.contains(entity_type))
            .map(str::to_string)
            .collect::<std::collections::BTreeSet<_>>()
    };

    // One durable transaction publishes every source, replace-mode deletion,
    // and the matching cross-invariant generation before any runtime fence moves.
    let publications = ioa_sources
        .iter()
        .map(|(entity_type, ioa_source)| {
            (
                entity_type,
                ioa_source,
                temper_store_turso::spec_content_hash(ioa_source),
            )
        })
        .collect::<Vec<_>>();
    let publication_refs = publications
        .iter()
        .map(|(entity_type, ioa_source, content_hash)| SpecPublication {
            entity_type,
            ioa_source,
            csdl_xml: &csdl_xml,
            content_hash,
        })
        .collect::<Vec<_>>();
    let mut intent_components = vec![
        ("csdl".to_string(), csdl_xml.as_bytes().to_vec()),
        (
            "mode".to_string(),
            if body.merge { "merge" } else { "replace" }
                .as_bytes()
                .to_vec(),
        ),
    ];
    let constraints_intent = match (body.merge, cross_invariants_toml.as_deref()) {
        (true, None) => b"preserve".to_vec(),
        (_, Some(source)) => source.as_bytes().to_vec(),
        (_, None) => b"delete".to_vec(),
    };
    intent_components.push(("constraints".to_string(), constraints_intent));
    intent_components.push((
        "policy".to_string(),
        complete_cedar_policy
            .as_deref()
            .map(str::as_bytes)
            .unwrap_or(b"preserve")
            .to_vec(),
    ));
    intent_components.extend(ioa_sources.iter().map(|(entity_type, ioa_source)| {
        (
            format!("spec:{entity_type}"),
            ioa_source.as_bytes().to_vec(),
        )
    }));
    let intent_refs = intent_components
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_slice()))
        .collect::<Vec<_>>();
    let publication_intent = ServerState::spec_publication_intent("load-specs-dir", intent_refs);
    state
        .arm_spec_publication(&mut publication_guard, &tenant_id, &publication_intent)
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error))?;
    if let Some(store) = state
        .storage_stack
        .as_ref()
        .and_then(|stack| stack.platform.clone())
    {
        let policy_entry_refs = owned_policy_entries
            .iter()
            .map(
                |(policy_id, cedar_text, created_by)| PolicyEntryPublication {
                    policy_id,
                    cedar_text,
                    created_by,
                },
            )
            .collect::<Vec<_>>();
        let policy_generation =
            policy_owner
                .as_deref()
                .map(|policy_owner| PolicyGenerationPublication {
                    policy_owner,
                    policy_entries: &policy_entry_refs,
                });
        let constraints = if body.merge && cross_invariants_toml.is_none() {
            TenantConstraintsPublication::Preserve
        } else {
            TenantConstraintsPublication::Replace(cross_invariants_toml.as_deref())
        };
        let durable_removed = store
            .publish_specs(
                &body.tenant,
                &publication_refs,
                if body.merge {
                    SpecPublicationMode::Merge
                } else {
                    SpecPublicationMode::Replace
                },
                constraints,
                complete_cedar_policy
                    .as_deref()
                    .map(TenantPolicyPublication::Replace)
                    .unwrap_or(TenantPolicyPublication::Preserve),
                None,
                policy_generation,
                &[],
            )
            .await
            .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error))?;
        removed_entity_types.extend(durable_removed);
    }
    let removed_entity_types = removed_entity_types.into_iter().collect::<Vec<_>>();

    let mut cutover = state
        .prepare_key_index_contracts_for_spec_activation_with_removals(
            &publication_guard,
            &tenant_id,
            &ioa_pairs,
            &removed_entity_types,
        )
        .await
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error))?;

    {
        let mut registry = state.registry.write().unwrap(); // ci-ok: infallible lock
        registry
            .try_register_tenant_with_reactions_constraints_and_key_epochs(
                body.tenant.as_str(),
                csdl,
                csdl_xml,
                &ioa_pairs,
                Vec::new(),
                cross_invariants_toml.clone(),
                body.merge,
                &cutover.activation_epochs,
            )
            .map_err(|e| {
                (
                    StatusCode::BAD_REQUEST,
                    format!("Failed to register specs: {e}"),
                )
            })?;
    }
    state
        .finish_key_index_contract_activation(&mut publication_guard, &tenant_id, &mut cutover)
        .await
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error))?;
    state.rebuild_reaction_dispatcher();
    if let Some(policy) = complete_cedar_policy {
        state
            .authz
            .reload_tenant_policies(&body.tenant, &policy)
            .map_err(|error| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Failed to activate bundled Cedar policies: {error}"),
                )
            })?;
        state
            .tenant_policies
            .write()
            .map_err(|error| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("tenant policy lock poisoned: {error}"),
                )
            })?
            .insert(body.tenant.clone(), policy);
    }
    state
        .complete_spec_publication_retry(&mut publication_guard, &tenant_id)
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error))?;

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
