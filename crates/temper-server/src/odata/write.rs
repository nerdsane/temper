//! OData write handlers (`POST`, `PATCH`, `PUT`, `DELETE`).

use axum::extract::Query;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use temper_odata::path::{ODataPath, parse_path};
use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;

use super::bindings::dispatch_bound_action;
use super::common::{
    constraint_violation_response, extract_key, extract_tenant, resolve_entity_type,
    verification_gate_response,
};
use super::response::annotate_entity;
use crate::constraint_engine::{
    post_write_invariant_checks, pre_delete_relation_checks, pre_upsert_relation_checks,
};
use crate::dispatch::{AgentContext, extract_agent_context};
use crate::response::{ODataResponse, odata_error};
use crate::state::ServerState;
use crate::state::trajectory::{TrajectoryEntry, TrajectorySource};

type ODataWriteError = Box<axum::response::Response>;

fn parse_odata_path_or_400(path: &str) -> Result<ODataPath, ODataWriteError> {
    parse_path(&format!("/{path}")).map_err(|e| {
        Box::new(
            odata_error(StatusCode::BAD_REQUEST, "InvalidPath", &e.to_string()).into_response(),
        )
    })
}

fn parse_json_body_or_400(body: &axum::body::Bytes) -> Result<serde_json::Value, ODataWriteError> {
    serde_json::from_slice(body).map_err(|e| {
        Box::new(
            odata_error(
                StatusCode::BAD_REQUEST,
                "InvalidBody",
                &format!("Invalid JSON body: {e}"),
            )
            .into_response(),
        )
    })
}

fn resolve_entity_type_or_404(
    state: &ServerState,
    tenant: &TenantId,
    set_name: &str,
) -> Result<String, ODataWriteError> {
    resolve_entity_type(state, tenant, set_name).ok_or_else(|| {
        Box::new(
            odata_error(
                StatusCode::NOT_FOUND,
                "EntitySetNotFound",
                &format!("Entity set '{set_name}' not found"),
            )
            .into_response(),
        )
    })
}

/// Like [`resolve_entity_type_or_404`], but also records a trajectory entry
/// for the unmet intent so the Evolution Engine can track entity-set-not-found gaps.
fn resolve_entity_type_or_record_404(
    state: &ServerState,
    tenant: &TenantId,
    set_name: &str,
    agent_ctx: &AgentContext,
) -> Result<String, ODataWriteError> {
    resolve_entity_type(state, tenant, set_name).ok_or_else(|| {
        let entry = TrajectoryEntry {
            timestamp: sim_now().to_rfc3339(),
            tenant: tenant.to_string(),
            entity_type: set_name.to_string(),
            entity_id: String::new(),
            action: "EntitySetNotFound".to_string(),
            success: false,
            from_status: None,
            to_status: None,
            error: Some(format!("Entity set '{}' not found", set_name)),
            agent_id: agent_ctx.agent_id.clone(),
            session_id: agent_ctx.session_id.clone(),
            authz_denied: None,
            denied_resource: None,
            denied_module: None,
            source: Some(TrajectorySource::Entity),
        };
        if let Ok(mut log) = state.trajectory_log.write() {
            // ci-ok: infallible lock
            log.push(entry);
        }
        Box::new(
            odata_error(
                StatusCode::NOT_FOUND,
                "EntitySetNotFound",
                &format!("Entity set '{}' not found", set_name),
            )
            .into_response(),
        )
    })
}

fn check_verification_gate_or_423(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
) -> Result<(), ODataWriteError> {
    state
        .check_verification_gate(tenant, entity_type)
        .map_err(|e| Box::new(verification_gate_response(e)))
}

fn ensure_entity_exists_or_404(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    set_name: &str,
    key: &str,
) -> Result<(), ODataWriteError> {
    if state.entity_exists(tenant, entity_type, key) {
        Ok(())
    } else {
        Err(Box::new(
            odata_error(
                StatusCode::NOT_FOUND,
                "ResourceNotFound",
                &format!("Entity '{set_name}' with key '{key}' not found"),
            )
            .into_response(),
        ))
    }
}

/// Handle POST requests — entity creation and bound actions.
pub async fn handle_odata_post(
    State(state): State<ServerState>,
    headers: HeaderMap,
    axum::extract::Path(path): axum::extract::Path<String>,
    Query(query_params): Query<std::collections::BTreeMap<String, String>>,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let tenant = extract_tenant(&headers, &state);
    let agent_ctx = extract_agent_context(&headers);
    let await_integration = query_params
        .get("await_integration")
        .map(|v| v == "true")
        .unwrap_or(false);
    let idempotency_key = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(String::from);
    let odata_path = match parse_odata_path_or_400(&path) {
        Ok(p) => p,
        Err(resp) => return *resp,
    };

    match odata_path {
        ODataPath::EntitySet(name) => {
            let entity_type =
                match resolve_entity_type_or_record_404(&state, &tenant, &name, &agent_ctx) {
                    Ok(t) => t,
                    Err(resp) => return *resp,
                };
            if let Err(resp) = check_verification_gate_or_423(&state, &tenant, &entity_type) {
                return *resp;
            }

            let body_json = match parse_json_body_or_400(&body) {
                Ok(v) => v,
                Err(resp) => return *resp,
            };

            let entity_id = body_json
                .get("id")
                .and_then(|v| v.as_str())
                .map(String::from)
                .unwrap_or_else(|| temper_runtime::scheduler::sim_uuid().to_string());

            let initial_fields = body_json.clone();
            if let Err(v) = pre_upsert_relation_checks(
                &state,
                &tenant,
                &entity_type,
                &entity_id,
                "create",
                &initial_fields,
            )
            .await
            {
                return constraint_violation_response(v);
            }
            if let Err(v) = post_write_invariant_checks(
                &state,
                &tenant,
                &entity_type,
                &entity_id,
                "Create",
                &initial_fields,
                "create",
            )
            .await
            {
                return constraint_violation_response(v);
            }

            match state
                .get_or_create_tenant_entity(&tenant, &entity_type, &entity_id, initial_fields)
                .await
            {
                Ok(response) => {
                    let body = annotate_entity(
                        serde_json::to_value(&response.state).unwrap_or_default(),
                        format!("$metadata#{name}/$entity"),
                        Some(format!("{name}('{entity_id}')")),
                    );
                    ODataResponse {
                        status: StatusCode::CREATED,
                        body,
                    }
                    .into_response()
                }
                Err(e) => odata_error(StatusCode::INTERNAL_SERVER_ERROR, "CreateError", &e)
                    .into_response(),
            }
        }

        ODataPath::BoundAction { parent, action } => {
            let body_json: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();

            let (set_name, key_str) = match *parent {
                ODataPath::Entity(ref set, ref key) => (set.clone(), extract_key(key)),
                _ => {
                    return odata_error(
                        StatusCode::BAD_REQUEST,
                        "InvalidPath",
                        "Action must be bound to an entity",
                    )
                    .into_response();
                }
            };

            let entity_type =
                match resolve_entity_type_or_record_404(&state, &tenant, &set_name, &agent_ctx) {
                    Ok(t) => t,
                    Err(resp) => return *resp,
                };

            if let Err(resp) = check_verification_gate_or_423(&state, &tenant, &entity_type) {
                return *resp;
            }
            dispatch_bound_action(
                &state,
                &tenant,
                &set_name,
                &entity_type,
                &key_str,
                &action,
                body_json,
                &agent_ctx,
                &headers,
                await_integration,
                idempotency_key.clone(),
            )
            .await
        }

        _ => odata_error(
            StatusCode::METHOD_NOT_ALLOWED,
            "MethodNotAllowed",
            "POST not supported for this path",
        )
        .into_response(),
    }
}

/// Handle PATCH requests — partial entity update.
pub async fn handle_odata_patch(
    State(state): State<ServerState>,
    headers: HeaderMap,
    axum::extract::Path(path): axum::extract::Path<String>,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let tenant = extract_tenant(&headers, &state);
    let odata_path = match parse_odata_path_or_400(&path) {
        Ok(p) => p,
        Err(resp) => return *resp,
    };

    match odata_path {
        ODataPath::Entity(set_name, key) => {
            let entity_type = match resolve_entity_type_or_404(&state, &tenant, &set_name) {
                Ok(t) => t,
                Err(resp) => return *resp,
            };
            let key_str = extract_key(&key);

            if let Err(resp) = check_verification_gate_or_423(&state, &tenant, &entity_type) {
                return *resp;
            }
            if let Err(resp) =
                ensure_entity_exists_or_404(&state, &tenant, &entity_type, &set_name, &key_str)
            {
                return *resp;
            }

            let body_json = match parse_json_body_or_400(&body) {
                Ok(v) => v,
                Err(resp) => return *resp,
            };
            let current_state = match state
                .get_tenant_entity_state(&tenant, &entity_type, &key_str)
                .await
            {
                Ok(v) => v,
                Err(e) => {
                    return odata_error(StatusCode::INTERNAL_SERVER_ERROR, "ReadError", &e)
                        .into_response();
                }
            };

            let mut prospective_fields = current_state.state.fields.clone();
            if let (Some(dst), Some(src)) =
                (prospective_fields.as_object_mut(), body_json.as_object())
            {
                for (k, v) in src {
                    dst.insert(k.clone(), v.clone());
                }
            } else {
                prospective_fields = body_json.clone();
            }

            if let Err(v) = pre_upsert_relation_checks(
                &state,
                &tenant,
                &entity_type,
                &key_str,
                "patch",
                &prospective_fields,
            )
            .await
            {
                return constraint_violation_response(v);
            }
            if let Err(v) = post_write_invariant_checks(
                &state,
                &tenant,
                &entity_type,
                &key_str,
                "Patch",
                &prospective_fields,
                "patch",
            )
            .await
            {
                return constraint_violation_response(v);
            }

            match state
                .update_tenant_entity_fields(&tenant, &entity_type, &key_str, body_json, false)
                .await
            {
                Ok(response) => {
                    let body = annotate_entity(
                        serde_json::to_value(&response.state).unwrap_or_default(),
                        format!("$metadata#{set_name}/$entity"),
                        Some(format!("{set_name}('{key_str}')")),
                    );
                    ODataResponse {
                        status: StatusCode::OK,
                        body,
                    }
                    .into_response()
                }
                Err(e) => odata_error(StatusCode::INTERNAL_SERVER_ERROR, "UpdateError", &e)
                    .into_response(),
            }
        }
        _ => odata_error(
            StatusCode::METHOD_NOT_ALLOWED,
            "MethodNotAllowed",
            "PATCH only supported on entity instances",
        )
        .into_response(),
    }
}

/// Handle PUT requests — full entity replacement.
pub async fn handle_odata_put(
    State(state): State<ServerState>,
    headers: HeaderMap,
    axum::extract::Path(path): axum::extract::Path<String>,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let tenant = extract_tenant(&headers, &state);
    let odata_path = match parse_odata_path_or_400(&path) {
        Ok(p) => p,
        Err(resp) => return *resp,
    };

    match odata_path {
        ODataPath::Entity(set_name, key) => {
            let entity_type = match resolve_entity_type_or_404(&state, &tenant, &set_name) {
                Ok(t) => t,
                Err(resp) => return *resp,
            };
            let key_str = extract_key(&key);

            if let Err(resp) = check_verification_gate_or_423(&state, &tenant, &entity_type) {
                return *resp;
            }
            if let Err(resp) =
                ensure_entity_exists_or_404(&state, &tenant, &entity_type, &set_name, &key_str)
            {
                return *resp;
            }

            let body_json = match parse_json_body_or_400(&body) {
                Ok(v) => v,
                Err(resp) => return *resp,
            };

            if let Err(v) = pre_upsert_relation_checks(
                &state,
                &tenant,
                &entity_type,
                &key_str,
                "put",
                &body_json,
            )
            .await
            {
                return constraint_violation_response(v);
            }
            if let Err(v) = post_write_invariant_checks(
                &state,
                &tenant,
                &entity_type,
                &key_str,
                "Put",
                &body_json,
                "put",
            )
            .await
            {
                return constraint_violation_response(v);
            }

            match state
                .update_tenant_entity_fields(&tenant, &entity_type, &key_str, body_json, true)
                .await
            {
                Ok(response) => {
                    let body = annotate_entity(
                        serde_json::to_value(&response.state).unwrap_or_default(),
                        format!("$metadata#{set_name}/$entity"),
                        Some(format!("{set_name}('{key_str}')")),
                    );
                    ODataResponse {
                        status: StatusCode::OK,
                        body,
                    }
                    .into_response()
                }
                Err(e) => odata_error(StatusCode::INTERNAL_SERVER_ERROR, "UpdateError", &e)
                    .into_response(),
            }
        }
        _ => odata_error(
            StatusCode::METHOD_NOT_ALLOWED,
            "MethodNotAllowed",
            "PUT only supported on entity instances",
        )
        .into_response(),
    }
}

/// Handle DELETE requests — entity deletion.
pub async fn handle_odata_delete(
    State(state): State<ServerState>,
    headers: HeaderMap,
    axum::extract::Path(path): axum::extract::Path<String>,
) -> impl IntoResponse {
    let tenant = extract_tenant(&headers, &state);
    let odata_path = match parse_odata_path_or_400(&path) {
        Ok(p) => p,
        Err(resp) => return *resp,
    };

    match odata_path {
        ODataPath::Entity(set_name, key) => {
            let entity_type = match resolve_entity_type_or_404(&state, &tenant, &set_name) {
                Ok(t) => t,
                Err(resp) => return *resp,
            };
            let key_str = extract_key(&key);

            if let Err(resp) = check_verification_gate_or_423(&state, &tenant, &entity_type) {
                return *resp;
            }
            if let Err(resp) =
                ensure_entity_exists_or_404(&state, &tenant, &entity_type, &set_name, &key_str)
            {
                return *resp;
            }
            if let Err(v) =
                pre_delete_relation_checks(&state, &tenant, &entity_type, &key_str, "delete").await
            {
                return constraint_violation_response(v);
            }
            let current_state = match state
                .get_tenant_entity_state(&tenant, &entity_type, &key_str)
                .await
            {
                Ok(v) => v,
                Err(e) => {
                    return odata_error(StatusCode::INTERNAL_SERVER_ERROR, "ReadError", &e)
                        .into_response();
                }
            };
            if let Err(v) = post_write_invariant_checks(
                &state,
                &tenant,
                &entity_type,
                &key_str,
                "Delete",
                &current_state.state.fields,
                "delete",
            )
            .await
            {
                return constraint_violation_response(v);
            }

            match state
                .delete_tenant_entity(&tenant, &entity_type, &key_str)
                .await
            {
                Ok(_) => (StatusCode::NO_CONTENT, "").into_response(),
                Err(e) => odata_error(StatusCode::INTERNAL_SERVER_ERROR, "DeleteError", &e)
                    .into_response(),
            }
        }
        _ => odata_error(
            StatusCode::METHOD_NOT_ALLOWED,
            "MethodNotAllowed",
            "DELETE only supported on entity instances",
        )
        .into_response(),
    }
}
