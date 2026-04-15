//! Spec management endpoints: list, load, and inspect IOA specifications.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Json;

use crate::authz::{observe_tenant_scope, require_observe_auth};
use crate::registry::VerificationStatus;
use crate::state::ServerState;

use super::{ActionDetail, InvariantDetail, SpecDetail, SpecSummary, StateVarDetail};

mod load_dir;
mod load_inline;
mod types;
mod verification_stream;

pub(crate) use load_dir::handle_load_dir;
pub(crate) use load_inline::handle_load_inline;

/// GET /observe/specs -- list all loaded specs across all tenants.
pub(crate) async fn handle_list_specs(
    State(state): State<ServerState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, StatusCode> {
    require_observe_auth(&state, &headers, "read_specs", "Spec")?;
    let tenant_scope = observe_tenant_scope(&state, &headers)?;
    let registry = state.registry.read().unwrap(); // ci-ok: infallible lock
    let mut specs = Vec::new();

    for tenant_id in registry.tenant_ids() {
        if let Some(ref scope) = tenant_scope
            && tenant_id != scope
        {
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
/// Searches across all tenants and returns the first match.
pub(crate) async fn handle_get_spec_detail(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Path(entity): Path<String>,
) -> Result<Json<SpecDetail>, StatusCode> {
    require_observe_auth(&state, &headers, "read_specs", "Spec")?;
    let tenant_scope = observe_tenant_scope(&state, &headers)?;
    let registry = state.registry.read().unwrap(); // ci-ok: infallible lock

    for tenant_id in registry.tenant_ids() {
        if let Some(ref scope) = tenant_scope
            && tenant_id != scope
        {
            continue;
        }
        if let Some(entity_spec) = registry.get_spec(tenant_id, &entity) {
            let automaton = &entity_spec.automaton;
            let detail = SpecDetail {
                entity_type: entity.clone(),
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
                    })
                    .collect(),
            };
            return Ok(Json(detail));
        }
    }

    Err(StatusCode::NOT_FOUND)
}
