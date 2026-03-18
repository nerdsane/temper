//! OData write handlers (`POST`, `PATCH`, `PUT`, `DELETE`).

use std::sync::{Arc, RwLock};

use axum::extract::Query;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use temper_odata::path::{ODataPath, parse_path};
use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;
use temper_wasm::{StreamRegistry, WasmInvocationContext};
use tracing::instrument;

use axum::Extension;

use super::bindings::dispatch_bound_action;
use super::common::{
    check_has_stream_or_400, constraint_violation_response, extract_key, extract_tenant,
    load_entity_or_404, resolve_entity_type, resolve_value_parent, run_write_prechecks,
    verification_gate_response,
};
use super::constraints::pre_delete_relation_checks;
use super::response::annotate_entity;
use crate::identity::ResolvedIdentity;
use crate::request_context::{AgentContext, extract_agent_context};
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
        tracing::warn!(tenant = %tenant, entity_set = %set_name, "entity set not found");
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
    request_body: Option<serde_json::Value>,
    intent: Option<String>,
) -> Result<String, ODataWriteError> {
    resolve_entity_type(state, tenant, set_name).ok_or_else(|| {
        tracing::warn!(tenant = %tenant, entity_set = %set_name, "entity set not found");
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
            source: Some(TrajectorySource::Platform),
            spec_governed: None,
            agent_type: agent_ctx.agent_type.clone(),
            request_body,
            intent,
        };
        {
            let state_c = state.clone();
            tokio::spawn(async move { // determinism-ok: background persist for sync 404 path
                if let Err(e) = state_c.persist_trajectory_entry(&entry).await {
                    tracing::error!(error = %e, "failed to persist entity-set-not-found trajectory");
                }
            });
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
#[instrument(skip_all, fields(otel.name = "POST /odata/{path}"))]
pub async fn handle_odata_post(
    State(state): State<ServerState>,
    resolved_id: Option<Extension<ResolvedIdentity>>,
    headers: HeaderMap,
    axum::extract::Path(path): axum::extract::Path<String>,
    Query(query_params): Query<std::collections::BTreeMap<String, String>>,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let tenant = match extract_tenant(&headers, &state) {
        Ok(t) => t,
        Err(e) => return e.into_response(),
    };
    let agent_ctx = extract_agent_context(&headers);
    let resolved_identity = resolved_id.map(|Extension(id)| id);
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
            let body_for_trajectory = serde_json::from_slice::<serde_json::Value>(&body).ok();
            let entity_type = match resolve_entity_type_or_record_404(
                &state,
                &tenant,
                &name,
                &agent_ctx,
                body_for_trajectory,
                agent_ctx.intent.clone(),
            ) {
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
            if let Err(resp) = run_write_prechecks(
                &state,
                &tenant,
                &entity_type,
                &entity_id,
                "Create",
                "create",
                &initial_fields,
            )
            .await
            {
                return resp;
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

            let entity_type = match resolve_entity_type_or_record_404(
                &state,
                &tenant,
                &set_name,
                &agent_ctx,
                Some(body_json.clone()),
                agent_ctx.intent.clone(),
            ) {
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
                resolved_identity.as_ref(),
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
#[instrument(skip_all, fields(otel.name = "PATCH /odata/{path}"))]
pub async fn handle_odata_patch(
    State(state): State<ServerState>,
    headers: HeaderMap,
    axum::extract::Path(path): axum::extract::Path<String>,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let tenant = match extract_tenant(&headers, &state) {
        Ok(t) => t,
        Err(e) => return e.into_response(),
    };
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
            let current_state = match load_entity_or_404(
                &state,
                &tenant,
                &entity_type,
                &set_name,
                &key_str,
            )
            .await
            {
                Ok(v) => v,
                Err(resp) => return resp,
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

            if let Err(resp) = run_write_prechecks(
                &state,
                &tenant,
                &entity_type,
                &key_str,
                "Patch",
                "patch",
                &prospective_fields,
            )
            .await
            {
                return resp;
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
#[instrument(skip_all, fields(otel.name = "PUT /odata/{path}"))]
pub async fn handle_odata_put(
    State(state): State<ServerState>,
    headers: HeaderMap,
    axum::extract::Path(path): axum::extract::Path<String>,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let tenant = match extract_tenant(&headers, &state) {
        Ok(t) => t,
        Err(e) => return e.into_response(),
    };
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

            if let Err(resp) = run_write_prechecks(
                &state,
                &tenant,
                &entity_type,
                &key_str,
                "Put",
                "put",
                &body_json,
            )
            .await
            {
                return resp;
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
        ODataPath::Value { parent } => {
            let agent_ctx = extract_agent_context(&headers);
            handle_stream_put(&state, &tenant, &parent, &headers, body, &agent_ctx)
                .await
                .into_response()
        }
        _ => odata_error(
            StatusCode::METHOD_NOT_ALLOWED,
            "MethodNotAllowed",
            "PUT only supported on entity instances or $value",
        )
        .into_response(),
    }
}

/// Handle DELETE requests — entity deletion.
#[instrument(skip_all, fields(otel.name = "DELETE /odata/{path}"))]
pub async fn handle_odata_delete(
    State(state): State<ServerState>,
    headers: HeaderMap,
    axum::extract::Path(path): axum::extract::Path<String>,
) -> impl IntoResponse {
    let tenant = match extract_tenant(&headers, &state) {
        Ok(t) => t,
        Err(e) => return e.into_response(),
    };
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
            let current_state = match load_entity_or_404(
                &state,
                &tenant,
                &entity_type,
                &set_name,
                &key_str,
            )
            .await
            {
                Ok(v) => v,
                Err(resp) => return resp,
            };
            if let Err(resp) = run_write_prechecks(
                &state,
                &tenant,
                &entity_type,
                &key_str,
                "Delete",
                "delete",
                &current_state.state.fields,
            )
            .await
            {
                return resp;
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

/// Handle PUT on `$value` — upload binary content via WASM blob_adapter.
///
/// Flow:
/// 1. Resolve parent entity from ODataPath
/// 2. Verify entity type has `HasStream=true` in CSDL
/// 3. Register upload bytes in StreamRegistry
/// 4. Invoke WASM blob_adapter (handles auth, hashing, caching, upload)
/// 5. Dispatch whatever action WASM returns (e.g. StreamUpdated)
/// 6. Return 204 No Content with ETag
#[instrument(skip_all, fields(otel.name = "PUT $value"))]
async fn handle_stream_put(
    state: &ServerState,
    tenant: &TenantId,
    parent: &ODataPath,
    headers: &HeaderMap,
    body: axum::body::Bytes,
    agent_ctx: &AgentContext,
) -> axum::response::Response {
    // 1. Resolve parent to (set_name, entity_id)
    let (set_name, key) = match resolve_value_parent(parent) {
        Ok(pair) => pair,
        Err(resp) => return resp,
    };

    let entity_type = match resolve_entity_type_or_404(state, tenant, &set_name) {
        Ok(t) => t,
        Err(resp) => return *resp,
    };

    // 2. Check HasStream=true on the entity type via CSDL
    if let Err(resp) = check_has_stream_or_400(state, tenant, &entity_type) {
        return resp;
    }

    if let Err(resp) = check_verification_gate_or_423(state, tenant, &entity_type) {
        return *resp;
    }

    // 3. Get entity state (needed by WASM for content_hash, etc.)
    let entity_state = match state
        .get_tenant_entity_state(tenant, &entity_type, &key)
        .await
    {
        Ok(resp) => serde_json::to_value(&resp.state).unwrap_or_default(),
        Err(e) => {
            return odata_error(StatusCode::INTERNAL_SERVER_ERROR, "StateError", &e)
                .into_response();
        }
    };

    let content_type = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();
    let size_bytes = body.len() as i64;

    // 4. Register bytes in StreamRegistry
    let stream_id = format!("upload-{}", temper_runtime::scheduler::sim_uuid());
    let streams = Arc::new(RwLock::new(StreamRegistry::default()));
    {
        let mut s = streams.write().unwrap(); // ci-ok: infallible lock
        s.register_stream(&stream_id, body.to_vec());
    }

    // 5. Invoke WASM blob_adapter
    let inv_ctx = WasmInvocationContext {
        tenant: tenant.to_string(),
        entity_type: entity_type.clone(),
        entity_id: key.clone(),
        trigger_action: "StreamUpload".to_string(),
        trigger_params: serde_json::json!({
            "stream_id": stream_id,
            "size_bytes": size_bytes,
            "content_type": content_type,
            "operation": "put",
        }),
        entity_state,
        agent_id: agent_ctx.agent_id.clone(),
        session_id: agent_ctx.session_id.clone(),
        integration_config: std::collections::BTreeMap::new(),
    };

    let wasm_result = match state
        .invoke_wasm_direct(tenant, "blob_adapter", inv_ctx, streams)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "WASM blob_adapter invocation failed");
            return odata_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "BlobAdapterError",
                &format!("Blob adapter failed: {e}"),
            )
            .into_response();
        }
    };

    if !wasm_result.success {
        let error_msg = wasm_result
            .error
            .unwrap_or_else(|| "unknown error".to_string());
        return odata_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "BlobUploadFailed",
            &error_msg,
        )
        .into_response();
    }

    // 6. Dispatch whatever action WASM returned (e.g. "StreamUpdated")
    if !wasm_result.callback_action.is_empty() {
        match state
            .dispatch_tenant_action(
                tenant,
                &entity_type,
                &key,
                &wasm_result.callback_action,
                wasm_result.callback_params,
                agent_ctx,
            )
            .await
        {
            Ok(entity_resp) => {
                let mut response = StatusCode::NO_CONTENT.into_response();
                response.headers_mut().insert(
                    "OData-Version",
                    "4.0".parse().unwrap(), // ci-ok: static header value
                );
                // Set ETag from entity's content_hash after action dispatch
                let state_val = serde_json::to_value(&entity_resp.state).unwrap_or_default();
                if let Some(hash) = state_val.get("content_hash").and_then(|v| v.as_str())
                    && let Ok(val) = format!("\"{hash}\"").parse()
                {
                    response.headers_mut().insert("ETag", val);
                }
                response
            }
            Err(e) => {
                // Action dispatch failed (e.g., locked file rejects StreamUpdated)
                odata_error(StatusCode::CONFLICT, "ActionRejected", &e).into_response()
            }
        }
    } else {
        StatusCode::NO_CONTENT.into_response()
    }
}
