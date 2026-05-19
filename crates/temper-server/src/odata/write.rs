//! OData write handlers (`POST`, `PATCH`, `PUT`, `DELETE`).

use axum::extract::Query;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use temper_odata::path::{ODataPath, parse_path};
use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;
use tracing::instrument;
use tracing_opentelemetry::OpenTelemetrySpanExt;

use axum::Extension;

use super::account_verification::enforce_commons_account_verified_for_write;
use super::app_uniqueness::enforce_commons_app_name_unique_for_write;
use super::bindings::dispatch_bound_action;
use super::common::{
    constraint_violation_response, extract_key, extract_tenant, load_entity_or_404,
    resolve_entity_type, run_write_prechecks, verification_gate_response,
};
use super::constraints::pre_delete_relation_checks;
use super::rate_limit::{enforce_commons_write_rate_limit, owner_id_from_fields};
use super::response::annotate_entity;
use super::storage_guardrails::enforce_commons_storage_cap;
use super::stream_put::handle_stream_put;
use crate::authz::{DenialInput, record_authz_denial, security_context_from_headers};
use crate::blobs::hydrate_blob_refs_for_tenant;
use crate::identity::ResolvedIdentity;
use crate::request_context::{AgentContext, extract_agent_context, remote_parent_context};
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
            matched_policy_ids: None,
        };
        if !state.enqueue_trajectory_entry(entry) {
            tracing::warn!(
                tenant = %tenant,
                entity_set = %set_name,
                "failed to enqueue entity-set-not-found trajectory"
            );
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

#[allow(clippy::too_many_arguments)]
async fn authorize_commons_collection_create(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
    fields: &serde_json::Value,
    headers: &HeaderMap,
    agent_ctx: &AgentContext,
    resolved_identity: Option<&ResolvedIdentity>,
) -> Result<(), ODataWriteError> {
    if !state.commons_guardrails_enabled(tenant) {
        return Ok(());
    }

    let security_ctx = if let Some(identity) = resolved_identity {
        temper_authz::SecurityContext::from_resolved_identity(
            &identity.agent_instance_id,
            &identity.agent_type_name,
            agent_ctx.session_id.as_deref(),
        )
    } else {
        security_context_from_headers(headers, None, agent_ctx.session_id.as_deref(), None)
    };

    let mut resource_attrs = std::collections::BTreeMap::new();
    resource_attrs.insert(
        "id".to_string(),
        serde_json::Value::String(entity_id.to_string()),
    );
    resource_attrs.insert(
        "status".to_string(),
        fields
            .get("Status")
            .or_else(|| fields.get("status"))
            .cloned()
            .unwrap_or_else(|| serde_json::Value::String(String::new())),
    );
    if let serde_json::Value::Object(obj) = fields {
        for (key, value) in obj {
            resource_attrs.insert(key.clone(), value.clone());
        }
    }
    let has_spec = state
        .has_registered_spec(tenant, entity_type)
        .unwrap_or(false);
    resource_attrs.insert("has_spec".to_string(), serde_json::Value::Bool(has_spec));

    if let Err(denial) = state.authorize_with_context(
        &security_ctx,
        "Create",
        entity_type,
        &resource_attrs,
        tenant.as_str(),
    ) {
        let reason = denial.to_string();
        let _ = record_authz_denial(
            state,
            DenialInput {
                tenant: tenant.as_str(),
                security_ctx: &security_ctx,
                agent_id_override: agent_ctx.agent_id.as_deref(),
                action: "Create",
                resource_type: entity_type,
                resource_id: entity_id,
                resource_attrs: serde_json::to_value(&resource_attrs).unwrap_or_default(),
                reason: &reason,
                module_name: None,
                from_status: None,
            },
        )
        .await;

        return Err(Box::new(
            odata_error(StatusCode::FORBIDDEN, "AuthorizationDenied", &reason).into_response(),
        ));
    }

    Ok(())
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
    let mut agent_ctx = extract_agent_context(&headers);
    if let Some(remote_parent) = remote_parent_context(&agent_ctx) {
        tracing::Span::current().set_parent(remote_parent);
    }
    let resolved_identity = resolved_id.map(|Extension(id)| id);
    // Enrich agent context with credential-resolved identity (ADR-0033).
    if let Some(ref identity) = resolved_identity {
        agent_ctx.agent_id = Some(identity.agent_instance_id.clone());
        agent_ctx.agent_type = Some(identity.agent_type_name.clone());
    }
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
                .or_else(|| body_json.get("Id"))
                .and_then(|v| v.as_str())
                .map(String::from)
                .unwrap_or_else(|| {
                    let prefix = entity_type_prefix(&entity_type);
                    format!("{prefix}{}", temper_runtime::scheduler::sim_uuid())
                });

            let initial_fields = body_json.clone();
            let _commons_guardrail_lock = state.acquire_commons_write_guardrail_lock(&tenant).await;

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

            if let Err(resp) = enforce_commons_account_verified_for_write(
                &state,
                &tenant,
                &entity_type,
                &initial_fields,
            )
            .await
            {
                return resp;
            }

            if let Err(resp) = enforce_commons_app_name_unique_for_write(
                &state,
                &tenant,
                &entity_type,
                &entity_id,
                &initial_fields,
            )
            .await
            {
                return resp;
            }

            if let Err(resp) = enforce_commons_storage_cap(
                &state,
                &tenant,
                &entity_type,
                &entity_id,
                "Create",
                &initial_fields,
            )
            .await
            {
                return resp;
            }

            if let Err(resp) = enforce_commons_write_rate_limit(
                &state,
                &tenant,
                &entity_type,
                owner_id_from_fields(&initial_fields),
                &headers,
                &agent_ctx,
                resolved_identity.as_ref(),
            )
            .await
            {
                return resp;
            }

            if let Err(resp) = authorize_commons_collection_create(
                &state,
                &tenant,
                &entity_type,
                &entity_id,
                &initial_fields,
                &headers,
                &agent_ctx,
                resolved_identity.as_ref(),
            )
            .await
            {
                return *resp;
            }

            // ToolDefinition: forward tool metadata to the session's ToolRegistry.
            if entity_type == "ToolDefinition"
                && let Some(actor_sys) = &state.pg_actor_system
            {
                let session_id = initial_fields
                    .get("session_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&entity_id)
                    .to_string();
                let namespace = format!("{tenant}/{session_id}");
                let registry =
                    temper_actor_runtime::ActorHandle::new(namespace, "ToolRegistry".to_string());
                let mut tool_info = initial_fields.clone();
                if tool_info.get("name").is_none()
                    && let Some(obj) = tool_info.as_object_mut()
                {
                    obj.insert("name".to_string(), serde_json::json!(entity_id));
                }
                let source = tool_info
                    .get("source_type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("builtin");
                let action = if source == "client" {
                    "RegisterTool"
                } else {
                    "RegisterServerTool"
                };
                let msg_params = if source == "client" {
                    serde_json::json!({
                        "client_id": tool_info.get("client_id").and_then(|v| v.as_str()).unwrap_or(""),
                        "tool_names": [entity_id],
                    })
                } else {
                    let mut p = tool_info.clone();
                    p["source"] = serde_json::json!(source);
                    p["name"] = serde_json::json!(entity_id);
                    p
                };
                match actor_sys
                    .tell(
                        None,
                        &registry,
                        temper_actor_runtime::spec_actor::SpecMessage::with_params(
                            action, msg_params,
                        ),
                    )
                    .await
                {
                    Ok(_) => {
                        let _ = actor_sys.activate_now(&registry).await;
                        return ODataResponse {
                            status: StatusCode::CREATED,
                            body: serde_json::json!({
                                "@odata.type": "#ToolDefinition",
                                "Id": entity_id,
                                "session_id": session_id,
                                "source_type": source,
                            }),
                        }
                        .into_response();
                    }
                    Err(e) => {
                        return odata_error(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "ToolRegistrationError",
                            &e.to_string(),
                        )
                        .into_response();
                    }
                }
            }

            // PG-backed entity creation.
            if state.is_pg_actor_backed(&tenant, &entity_type)
                && let Some(actor_sys) = &state.pg_actor_system
            {
                let namespace = format!("{tenant}/{entity_id}");
                let spawn_result = if entity_type == "Process" {
                    actor_sys.spawn_all_registered(&namespace).await
                } else {
                    actor_sys
                        .spawn_with_fields(&namespace, &entity_type, initial_fields.clone())
                        .await
                        .map(|_| ())
                };
                match spawn_result {
                    Ok(_) => {
                        if entity_type == "Process" {
                            let handle = temper_actor_runtime::ActorHandle::new(
                                namespace.clone(),
                                entity_type.clone(),
                            );
                            let _ = actor_sys
                                .update_actor_fields(&handle, initial_fields.clone(), false)
                                .await;
                        }
                        return ODataResponse {
                            status: StatusCode::CREATED,
                            body: serde_json::json!({
                                "@odata.type": format!("#{entity_type}"),
                                "Id": entity_id,
                                "namespace": namespace,
                            }),
                        }
                        .into_response();
                    }
                    Err(e) => {
                        return odata_error(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "ActorSpawnError",
                            &e.to_string(),
                        )
                        .into_response();
                    }
                }
            }

            match state
                .get_or_create_tenant_entity(&tenant, &entity_type, &entity_id, initial_fields)
                .await
            {
                Ok(response) => {
                    if entity_type == "RateLimit" {
                        state.clear_commons_rate_limit_cache();
                    }
                    state.clear_commons_storage_projection_cache_for_entity(&entity_type);
                    let mut state_json = serde_json::to_value(&response.state).unwrap_or_default();
                    hydrate_blob_refs_for_tenant(&state, &tenant, &mut state_json).await;
                    let body = annotate_entity(
                        state_json,
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

            if state.is_pg_actor_backed(&tenant, &entity_type)
                && let Some(actor_sys) = &state.pg_actor_system
            {
                let namespace = format!("{tenant}/{key_str}");
                let handle = temper_actor_runtime::ActorHandle::new(namespace, entity_type.clone());
                let action_name = action.rsplit('.').next().unwrap_or(&action);
                match actor_sys
                    .tell(
                        None,
                        &handle,
                        temper_actor_runtime::spec_actor::SpecMessage::with_params(
                            action_name,
                            body_json.clone(),
                        ),
                    )
                    .await
                {
                    Ok(_) => {
                        let _ = actor_sys.activate_now(&handle).await;
                        let body = if let Some(actor_state) =
                            actor_sys.get_spec_actor_state(&handle).await
                        {
                            serde_json::json!({
                                "entity_type": entity_type,
                                "entity_id": key_str,
                                "status": actor_state.status,
                                "counters": actor_state.counters,
                                "booleans": actor_state.booleans,
                                "lists": actor_state.lists,
                                "fields": actor_state.fields,
                            })
                        } else {
                            serde_json::json!({ "Id": key_str, "action": action_name })
                        };
                        return ODataResponse {
                            status: StatusCode::OK,
                            body,
                        }
                        .into_response();
                    }
                    Err(e) => {
                        return odata_error(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "ActorDispatchError",
                            &e.to_string(),
                        )
                        .into_response();
                    }
                }
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
    let agent_ctx = extract_agent_context(&headers);

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

            let _commons_guardrail_lock = state.acquire_commons_write_guardrail_lock(&tenant).await;

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

            if let Err(resp) = enforce_commons_account_verified_for_write(
                &state,
                &tenant,
                &entity_type,
                &prospective_fields,
            )
            .await
            {
                return resp;
            }

            if let Err(resp) = enforce_commons_app_name_unique_for_write(
                &state,
                &tenant,
                &entity_type,
                &key_str,
                &prospective_fields,
            )
            .await
            {
                return resp;
            }

            if let Err(resp) = enforce_commons_write_rate_limit(
                &state,
                &tenant,
                &entity_type,
                owner_id_from_fields(&prospective_fields),
                &headers,
                &agent_ctx,
                None,
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
                    if entity_type == "RateLimit" {
                        state.clear_commons_rate_limit_cache();
                    }
                    state.clear_commons_storage_projection_cache_for_entity(&entity_type);
                    let mut state_json = serde_json::to_value(&response.state).unwrap_or_default();
                    hydrate_blob_refs_for_tenant(&state, &tenant, &mut state_json).await;
                    let body = annotate_entity(
                        state_json,
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
    let agent_ctx = extract_agent_context(&headers);

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

            let _commons_guardrail_lock = state.acquire_commons_write_guardrail_lock(&tenant).await;

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

            if let Err(resp) = enforce_commons_account_verified_for_write(
                &state,
                &tenant,
                &entity_type,
                &body_json,
            )
            .await
            {
                return resp;
            }

            if let Err(resp) = enforce_commons_app_name_unique_for_write(
                &state,
                &tenant,
                &entity_type,
                &key_str,
                &body_json,
            )
            .await
            {
                return resp;
            }

            if let Err(resp) = enforce_commons_write_rate_limit(
                &state,
                &tenant,
                &entity_type,
                owner_id_from_fields(&body_json),
                &headers,
                &agent_ctx,
                None,
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
                    if entity_type == "RateLimit" {
                        state.clear_commons_rate_limit_cache();
                    }
                    state.clear_commons_storage_projection_cache_for_entity(&entity_type);
                    let mut state_json = serde_json::to_value(&response.state).unwrap_or_default();
                    hydrate_blob_refs_for_tenant(&state, &tenant, &mut state_json).await;
                    let body = annotate_entity(
                        state_json,
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
    let agent_ctx = extract_agent_context(&headers);

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

            if let Err(resp) = enforce_commons_account_verified_for_write(
                &state,
                &tenant,
                &entity_type,
                &current_state.state.fields,
            )
            .await
            {
                return resp;
            }

            if let Err(resp) = enforce_commons_write_rate_limit(
                &state,
                &tenant,
                &entity_type,
                owner_id_from_fields(&current_state.state.fields),
                &headers,
                &agent_ctx,
                None,
            )
            .await
            {
                return resp;
            }

            match state
                .delete_tenant_entity(&tenant, &entity_type, &key_str)
                .await
            {
                Ok(_) => {
                    if entity_type == "RateLimit" {
                        state.clear_commons_rate_limit_cache();
                    }
                    state.clear_commons_storage_projection_cache_for_entity(&entity_type);
                    (StatusCode::NO_CONTENT, "").into_response()
                }
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

/// Map an entity type name to a short lowercase prefix for auto-generated IDs.
///
/// Prefixed UUIDs make IDs self-describing: `aj-01916f3b-...` is immediately
/// identifiable as an Agent without querying. The prefix is prepended only when
/// the caller omits the `id` field from the POST body.
fn entity_type_prefix(entity_type: &str) -> &'static str {
    match entity_type {
        "App" => "ap-",
        "Agent" => "aj-",
        "Soul" => "sl-",
        "Session" => "ss-",
        "File" => "fl-",
        "Directory" => "dr-",
        "Workspace" => "ws-",
        "WorkCycle" => "wc-",
        "Issue" => "is-",
        "Project" => "pj-",
        "Team" => "tm-",
        "Memory" => "mm-",
        "Plan" => "pl-",
        "ToolHook" => "th-",
        "CronJob" => "cj-",
        "CronScheduler" => "cs-",
        "HeartbeatMonitor" => "hm-",
        "CapabilityRequest" => "cr-",
        "CatalogEntry" => "ce-",
        "Monitor" => "mn-",
        "AlertCycle" => "ac-",
        _ => "en-",
    }
}
