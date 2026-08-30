//! Spec management endpoints: list, load, and inspect IOA specifications.

use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::response::Json;
use temper_authz::AuthenticatedRequestContext;

use crate::authz::{observe_tenant_scope, require_authenticated_context, require_observe_auth};
use crate::registry::VerificationStatus;
use crate::state::ServerState;

use super::{
    ActionDetail, ActionParamDetail, InvariantDetail, SpecDetail, SpecSummary, StateVarDetail,
};

mod load_dir;
mod load_inline;
mod types;
mod validate_ioa;
mod verification_stream;

pub(crate) use load_dir::handle_load_dir;
pub(crate) use load_inline::handle_load_inline;
pub(crate) use validate_ioa::handle_validate_ioa;

/// GET /observe/specs -- list loaded specs in the credential-bound tenant.
pub(crate) async fn handle_list_specs(
    State(state): State<ServerState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let authenticated = require_authenticated_context(authenticated.as_deref())?;
    require_observe_auth(&state, authenticated, "read_specs", "Spec")?;
    let tenant_scope = observe_tenant_scope(authenticated);
    let registry = state.registry.read().unwrap(); // ci-ok: infallible lock
    let mut specs = Vec::new();

    for tenant_id in registry.tenant_ids() {
        if tenant_id != tenant_scope {
            continue;
        }
        for entity_type in registry.entity_types(tenant_id) {
            if let Some(entity_spec) = registry.get_spec(tenant_id, entity_type) {
                let automaton = &entity_spec.automaton;

                // Read verification status
                let (verification_status, levels_passed, levels_total) = match registry
                    .get_verification_status(tenant_id, entity_type)
                {
                    Some(VerificationStatus::Pending) | None => ("pending".to_string(), None, None),
                    Some(VerificationStatus::Running) => ("running".to_string(), None, None),
                    Some(
                        VerificationStatus::Completed(result)
                        | VerificationStatus::Restored(result),
                    ) => {
                        let passed = result.levels.iter().filter(|l| l.passed).count();
                        let total = result.levels.len();
                        let status = if result.all_passed {
                            "passed"
                        } else if passed == 0 {
                            "failed"
                        } else {
                            "partial"
                        };
                        (status.to_string(), Some(passed), Some(total))
                    }
                };

                specs.push(SpecSummary {
                    tenant: tenant_id.as_str().to_string(),
                    entity_type: entity_type.to_string(),
                    states: automaton.automaton.states.clone(),
                    actions: automaton.actions.iter().map(|a| a.name.clone()).collect(),
                    initial_state: automaton.automaton.initial.clone(),
                    verification_status,
                    levels_passed,
                    levels_total,
                });
            }
        }
    }

    let total = specs.len();
    Ok(Json(serde_json::json!({ "specs": specs, "total": total })))
}

/// GET /observe/specs/{entity} -- full spec detail for a named entity type.
///
/// Searches only the credential-bound tenant.
///
/// Carries `spec_version`, the content hash of the returned spec's IOA source.
/// That is the digest a conformance check compares a run's recorded
/// `metadata.spec_version` against, so a harness that reads it here records a
/// version the kernel will recognise, rather than one it computed from a file
/// some deploy path may have rewritten.
pub(crate) async fn handle_get_spec_detail(
    State(state): State<ServerState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    Path(entity): Path<String>,
) -> Result<Json<SpecDetail>, StatusCode> {
    let authenticated = require_authenticated_context(authenticated.as_deref())?;
    require_observe_auth(&state, authenticated, "read_specs", "Spec")?;
    let tenant_scope = observe_tenant_scope(authenticated);
    let registry = state.registry.read().unwrap(); // ci-ok: infallible lock

    for tenant_id in registry.tenant_ids() {
        if tenant_id != tenant_scope {
            continue;
        }
        if let Some(entity_spec) = registry.get_spec(tenant_id, &entity) {
            let automaton = &entity_spec.automaton;
            let detail = SpecDetail {
                entity_type: entity.clone(),
                spec_version: temper_store_turso::spec_content_hash(&entity_spec.ioa_source),
                states: automaton.automaton.states.clone(),
                initial_state: automaton.automaton.initial.clone(),
                actions: automaton
                    .actions
                    .iter()
                    .map(|a| ActionDetail {
                        name: a.name.clone(),
                        kind: a.kind.clone(),
                        from: a.from.clone(),
                        to: a.to.clone(),
                        guards: a.guard.iter().map(|g| format!("{g:?}")).collect(),
                        effects: a.effect.iter().map(|e| format!("{e:?}")).collect(),
                        params: a
                            .params
                            .iter()
                            .map(|p| ActionParamDetail {
                                name: p.name().to_string(),
                                param_type: p.param_type().to_string(),
                                nullable: p.nullable(),
                            })
                            .collect(),
                        hint: a.hint.clone().unwrap_or_default(),
                    })
                    .collect(),
                invariants: automaton
                    .invariants
                    .iter()
                    .map(|i| InvariantDetail {
                        name: i.name.clone(),
                        when: i.when.clone(),
                        assertion: i.assert.clone(),
                    })
                    .collect(),
                state_variables: automaton
                    .state
                    .iter()
                    .map(|sv| StateVarDetail {
                        name: sv.name.clone(),
                        var_type: sv.var_type.clone(),
                        initial: sv.initial.clone(),
                        query_indexed: sv.query_indexed,
                    })
                    .collect(),
            };
            return Ok(Json(detail));
        }
    }

    Err(StatusCode::NOT_FOUND)
}
