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
use super::authz::{
    CREATE_ACTION, DELETE_ACTION, MutationResource, UPDATE_ACTION, authorize_mutation,
    request_security_context,
};
use super::bindings::dispatch_bound_action;
use super::common::{
    constraint_violation_response, extract_key, extract_tenant, load_entity_or_404,
    resolve_entity_type, run_write_prechecks, verification_gate_response,
};
use super::constraints::{PreDeleteRelationError, pre_delete_relation_checks};
use super::rate_limit::{enforce_commons_write_rate_limit, owner_id_from_fields};
use super::response::annotate_entity;
use super::storage_guardrails::enforce_commons_storage_cap;
use super::stream_put::handle_stream_put;
use crate::blobs::hydrate_blob_refs_for_tenant;
use crate::identity::ResolvedIdentity;
use crate::request_context::{AgentContext, extract_agent_context, remote_parent_context};
use crate::response::{ODataResponse, odata_error, service_unavailable_response};
use crate::state::trajectory::{TrajectoryEntry, TrajectorySource};
use crate::state::{DeleteTargetLifecycle, ServerState};

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

fn parse_entity_object_body_or_400(
    body: &axum::body::Bytes,
) -> Result<serde_json::Value, ODataWriteError> {
    let value = parse_json_body_or_400(body)?;
    if value.is_object() {
        return Ok(value);
    }
    Err(Box::new(
        odata_error(
            StatusCode::BAD_REQUEST,
            "InvalidBody",
            "Entity fields must be a JSON object",
        )
        .into_response(),
    ))
}

fn collection_create_entity_id(
    body: &serde_json::Value,
    entity_type: &str,
) -> Result<String, ODataWriteError> {
    let fields = body.as_object().ok_or_else(|| {
        Box::new(
            odata_error(
                StatusCode::BAD_REQUEST,
                "InvalidBody",
                "Entity creation body must be a JSON object",
            )
            .into_response(),
        )
    })?;
    let parse_alias = |name: &str| -> Result<Option<&str>, ODataWriteError> {
        let Some(value) = fields.get(name) else {
            return Ok(None);
        };
        value.as_str().map(Some).ok_or_else(|| {
            Box::new(
                odata_error(
                    StatusCode::BAD_REQUEST,
                    "InvalidEntityId",
                    &format!("Entity id alias '{name}' must be a string"),
                )
                .into_response(),
            )
        })
    };
    let lowercase_id = parse_alias("id")?;
    let odata_id = parse_alias("Id")?;
    if let (Some(lowercase_id), Some(odata_id)) = (lowercase_id, odata_id)
        && lowercase_id != odata_id
    {
        return Err(Box::new(
            odata_error(
                StatusCode::BAD_REQUEST,
                "ConflictingEntityIdAliases",
                "Entity id aliases 'id' and 'Id' must match",
            )
            .into_response(),
        ));
    }
    if let Some(entity_id) = lowercase_id.or(odata_id) {
        if entity_id.is_empty() {
            return Err(Box::new(
                odata_error(
                    StatusCode::BAD_REQUEST,
                    "InvalidEntityId",
                    "Entity id must not be empty",
                )
                .into_response(),
            ));
        }
        return Ok(entity_id.to_string());
    }
    let prefix = entity_type_prefix(entity_type);
    Ok(format!("{prefix}{}", temper_runtime::scheduler::sim_uuid()))
}

fn collection_create_fields(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    body: &serde_json::Value,
) -> Result<serde_json::Value, ODataWriteError> {
    let initial_status = state
        .initial_entity_status(tenant, entity_type)
        .map_err(|error| {
            Box::new(
                odata_error(StatusCode::INTERNAL_SERVER_ERROR, "EntitySpecError", &error)
                    .into_response(),
            )
        })?;
    let Some(fields) = body.as_object() else {
        return Err(Box::new(
            odata_error(
                StatusCode::BAD_REQUEST,
                "InvalidBody",
                "Entity creation body must be a JSON object",
            )
            .into_response(),
        ));
    };
    for alias in ["status", "Status"] {
        let Some(value) = fields.get(alias) else {
            continue;
        };
        let Some(value) = value.as_str() else {
            return Err(Box::new(
                odata_error(
                    StatusCode::BAD_REQUEST,
                    "InvalidInitialStatus",
                    &format!("Lifecycle alias '{alias}' must be a string"),
                )
                .into_response(),
            ));
        };
        if value != initial_status {
            return Err(Box::new(
                odata_error(
                    StatusCode::BAD_REQUEST,
                    "InvalidInitialStatus",
                    &format!(
                        "Entity type '{entity_type}' must be created in status '{initial_status}'"
                    ),
                )
                .into_response(),
            ));
        }
    }
    Ok(crate::entity_actor::effects::sanitize_action_params(body).into_owned())
}

fn entity_update_fields(
    body: &serde_json::Value,
    entity_id: &str,
    current_status: &str,
) -> Result<serde_json::Value, ODataWriteError> {
    let Some(fields) = body.as_object() else {
        return Err(Box::new(
            odata_error(
                StatusCode::BAD_REQUEST,
                "InvalidBody",
                "Entity fields must be a JSON object",
            )
            .into_response(),
        ));
    };
    for alias in ["id", "Id"] {
        let Some(value) = fields.get(alias) else {
            continue;
        };
        if value.as_str() != Some(entity_id) {
            return Err(Box::new(
                odata_error(
                    StatusCode::BAD_REQUEST,
                    "ImmutableEntityId",
                    "Entity id fields must match the request path",
                )
                .into_response(),
            ));
        }
    }
    for alias in ["status", "Status"] {
        let Some(value) = fields.get(alias) else {
            continue;
        };
        if value.as_str() != Some(current_status) {
            return Err(Box::new(
                odata_error(
                    StatusCode::BAD_REQUEST,
                    "ImmutableEntityStatus",
                    "Generic field updates cannot change entity lifecycle status",
                )
                .into_response(),
            ));
        }
    }
    Ok(crate::entity_actor::effects::sanitize_action_params(body).into_owned())
}

fn field_update_error_response(error: &str) -> axum::response::Response {
    if error.contains("field update authorization became stale") {
        return odata_error(
            StatusCode::CONFLICT,
            "ConcurrentModification",
            "Entity state changed after authorization; retry the request",
        )
        .into_response();
    }
    service_unavailable_response(
        "UpdateUnavailable",
        "Entity update could not be completed; retry the request",
        "entity_field_update",
        error,
    )
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

async fn authorize_collection_create(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
    fields: &serde_json::Value,
    security_ctx: &temper_authz::SecurityContext,
    agent_ctx: &AgentContext,
) -> Result<(), ODataWriteError> {
    let resource_attrs = state
        .build_create_authz_resource_attrs(tenant, entity_type, entity_id, fields)
        .await
        .map_err(|error| {
            Box::new(
                odata_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "AuthorizationResourceError",
                    &error,
                )
                .into_response(),
            )
        })?;
    authorize_mutation(
        state,
        tenant,
        security_ctx,
        agent_ctx,
        CREATE_ACTION,
        MutationResource {
            entity_type,
            entity_id,
            attrs: &resource_attrs,
        },
    )
    .await
    .map_err(Box::new)
}

struct CollisionResource<'a> {
    entity_type: &'a str,
    entity_id: &'a str,
    status: &'a str,
    fields: &'a serde_json::Value,
}

async fn create_collision_response(
    state: &ServerState,
    tenant: &TenantId,
    collision: CollisionResource<'_>,
    security_ctx: &temper_authz::SecurityContext,
    agent_ctx: &AgentContext,
) -> axum::response::Response {
    let attrs = match state
        .build_authz_resource_attrs(
            tenant,
            collision.entity_type,
            collision.entity_id,
            collision.status,
            collision.fields,
        )
        .await
    {
        Ok(attrs) => attrs,
        Err(error) => {
            return service_unavailable_response(
                "ResourceStateUnavailable",
                "Existing entity state is temporarily unavailable",
                "create_collision_authorization_resource",
                error,
            );
        }
    };
    if let Err(response) = authorize_mutation(
        state,
        tenant,
        security_ctx,
        agent_ctx,
        CREATE_ACTION,
        MutationResource {
            entity_type: collision.entity_type,
            entity_id: collision.entity_id,
            attrs: &attrs,
        },
    )
    .await
    {
        return response;
    }
    odata_error(
        StatusCode::CONFLICT,
        "ResourceAlreadyExists",
        "An entity with this id already exists",
    )
    .into_response()
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

async fn load_delete_target_or_404(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    set_name: &str,
    key: &str,
) -> Result<DeleteTargetLifecycle, ODataWriteError> {
    match state
        .delete_target_lifecycle(tenant, entity_type, key)
        .await
    {
        Ok(DeleteTargetLifecycle::Absent) => Err(Box::new(
            odata_error(
                StatusCode::NOT_FOUND,
                "ResourceNotFound",
                &format!("Entity '{set_name}' with key '{key}' not found"),
            )
            .into_response(),
        )),
        Ok(lifecycle) => Ok(lifecycle),
        Err(error) => {
            tracing::error!(
                error = %error,
                tenant = %tenant,
                entity_type,
                entity_id = key,
                "failed to verify durable entity stream before delete"
            );
            Err(Box::new(
                odata_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "StorageUnavailable",
                    "Entity storage is temporarily unavailable",
                )
                .into_response(),
            ))
        }
    }
}

fn delete_unavailable_response(error: impl std::fmt::Display) -> axum::response::Response {
    service_unavailable_response(
        "DeleteUnavailable",
        "Entity deletion could not be completed; retry the request",
        "entity_delete",
        error,
    )
}

async fn authorize_existing_mutation(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
    action: &str,
    security_ctx: &temper_authz::SecurityContext,
    agent_ctx: &AgentContext,
) -> Result<crate::state::AuthzResourceSnapshot, ODataWriteError> {
    let snapshot = state
        .load_authz_resource_snapshot(tenant, entity_type, entity_id)
        .await
        .map_err(|error| {
            Box::new(service_unavailable_response(
                "ResourceStateUnavailable",
                "Entity state is temporarily unavailable",
                "mutation_authorization_snapshot",
                error,
            ))
        })?;
    authorize_mutation(
        state,
        tenant,
        security_ctx,
        agent_ctx,
        action,
        MutationResource {
            entity_type,
            entity_id,
            attrs: &snapshot.resource_attrs,
        },
    )
    .await
    .map_err(Box::new)?;
    Ok(snapshot)
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

            let entity_id = match collection_create_entity_id(&body_json, &entity_type) {
                Ok(entity_id) => entity_id,
                Err(response) => return *response,
            };

            let initial_fields =
                match collection_create_fields(&state, &tenant, &entity_type, &body_json) {
                    Ok(fields) => fields,
                    Err(response) => return *response,
                };
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
                return *resp;
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

            let security_ctx =
                request_security_context(&headers, &agent_ctx, resolved_identity.as_ref());
            if let Err(resp) = authorize_collection_create(
                &state,
                &tenant,
                &entity_type,
                &entity_id,
                &initial_fields,
                &security_ctx,
                &agent_ctx,
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
                match actor_sys
                    .spawn_with_fields_if_absent(&namespace, &entity_type, initial_fields.clone())
                    .await
                {
                    Ok(temper_actor_runtime::ActorSpawnOutcome::Created(_handle)) => {
                        if entity_type == "Process"
                            && let Err(error) = actor_sys.spawn_all_registered(&namespace).await
                        {
                            return service_unavailable_response(
                                "ActorSpawnUnavailable",
                                "Actor creation could not be completed; retry the request",
                                "pg_actor_namespace_spawn",
                                error,
                            );
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
                    Ok(temper_actor_runtime::ActorSpawnOutcome::AlreadyExists(_handle)) => {
                        if entity_type == "Process"
                            && let Err(error) = actor_sys.spawn_all_registered(&namespace).await
                        {
                            return service_unavailable_response(
                                "ActorSpawnUnavailable",
                                "Actor creation could not be completed; retry the request",
                                "pg_actor_namespace_repair",
                                error,
                            );
                        }
                        let state_bytes = match actor_sys.load_state(&namespace, &entity_type).await
                        {
                            Ok(Some(state_bytes)) => state_bytes,
                            Ok(None) => {
                                return service_unavailable_response(
                                    "ResourceStateUnavailable",
                                    "Existing actor state is temporarily unavailable",
                                    "pg_actor_create_collision_load",
                                    "actor insert reported a collision but no state was readable",
                                );
                            }
                            Err(error) => {
                                return service_unavailable_response(
                                    "ResourceStateUnavailable",
                                    "Existing actor state is temporarily unavailable",
                                    "pg_actor_create_collision_load",
                                    error,
                                );
                            }
                        };
                        let existing: temper_actor_runtime::spec_actor::SpecActorState =
                            match serde_json::from_slice(&state_bytes) {
                                Ok(existing) => existing,
                                Err(error) => {
                                    return service_unavailable_response(
                                        "ResourceStateUnavailable",
                                        "Existing actor state is temporarily unavailable",
                                        "pg_actor_create_collision_decode",
                                        error,
                                    );
                                }
                            };
                        return create_collision_response(
                            &state,
                            &tenant,
                            CollisionResource {
                                entity_type: &entity_type,
                                entity_id: &entity_id,
                                status: &existing.status,
                                fields: &existing.fields,
                            },
                            &security_ctx,
                            &agent_ctx,
                        )
                        .await;
                    }
                    Err(error) => {
                        return service_unavailable_response(
                            "ActorSpawnUnavailable",
                            "Actor creation could not be completed; retry the request",
                            "pg_actor_insert",
                            error,
                        );
                    }
                }
            }

            match state
                .try_create_data_only_tenant_entity(
                    &tenant,
                    &entity_type,
                    &entity_id,
                    initial_fields.clone(),
                )
                .await
            {
                Ok(Some(response)) => {
                    let mut state_json = serde_json::to_value(&response.state).unwrap_or_default();
                    hydrate_blob_refs_for_tenant(&state, &tenant, &mut state_json).await;
                    let body = annotate_entity(
                        state_json,
                        format!("$metadata#{name}/$entity"),
                        Some(format!("{name}('{entity_id}')")),
                    );
                    return ODataResponse {
                        status: StatusCode::CREATED,
                        body,
                    }
                    .into_response();
                }
                Ok(None) => {}
                Err(e) => {
                    return service_unavailable_response(
                        "CreateUnavailable",
                        "Entity creation could not be completed; retry the request",
                        "data_only_entity_create",
                        e,
                    );
                }
            }

            match state
                .create_tenant_entity_if_absent(&tenant, &entity_type, &entity_id, initial_fields)
                .await
            {
                Ok(crate::state::CreateEntityOutcome::Created(response)) => {
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
                Ok(crate::state::CreateEntityOutcome::AlreadyExists(response)) => {
                    create_collision_response(
                        &state,
                        &tenant,
                        CollisionResource {
                            entity_type: &entity_type,
                            entity_id: &entity_id,
                            status: &response.state.status,
                            fields: &response.state.fields,
                        },
                        &security_ctx,
                        &agent_ctx,
                    )
                    .await
                }
                Err(error) => service_unavailable_response(
                    "CreateUnavailable",
                    "Entity creation could not be completed; retry the request",
                    "entity_create_compare_and_append",
                    error,
                ),
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
                let handle =
                    temper_actor_runtime::ActorHandle::new(namespace.clone(), entity_type.clone());
                let action_name = action.rsplit('.').next().unwrap_or(&action);
                let state_bytes = match actor_sys.load_state(&namespace, &entity_type).await {
                    Ok(Some(state_bytes)) => state_bytes,
                    Ok(None) => {
                        return odata_error(
                            StatusCode::NOT_FOUND,
                            "ResourceNotFound",
                            &format!("Entity '{set_name}' with key '{key_str}' not found"),
                        )
                        .into_response();
                    }
                    Err(error) => {
                        return odata_error(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "ActorReadError",
                            &error.to_string(),
                        )
                        .into_response();
                    }
                };
                let actor_state: temper_actor_runtime::spec_actor::SpecActorState =
                    serde_json::from_slice(&state_bytes).unwrap_or_default();
                let attrs = match state
                    .build_authz_resource_attrs(
                        &tenant,
                        &entity_type,
                        &key_str,
                        &actor_state.status,
                        &actor_state.fields,
                    )
                    .await
                {
                    Ok(attrs) => attrs,
                    Err(error) => {
                        return service_unavailable_response(
                            "ResourceStateUnavailable",
                            "Authorization resource state is temporarily unavailable",
                            "pg_actor_mutation_authorization_resource",
                            error,
                        );
                    }
                };
                let security_ctx =
                    request_security_context(&headers, &agent_ctx, resolved_identity.as_ref());
                if let Err(response) = authorize_mutation(
                    &state,
                    &tenant,
                    &security_ctx,
                    &agent_ctx,
                    &action,
                    MutationResource {
                        entity_type: &entity_type,
                        entity_id: &key_str,
                        attrs: &attrs,
                    },
                )
                .await
                {
                    return response;
                }
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
    resolved_id: Option<Extension<ResolvedIdentity>>,
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
    let resolved_identity = resolved_id.map(|Extension(identity)| identity);
    let mut agent_ctx = extract_agent_context(&headers);
    if let Some(ref identity) = resolved_identity {
        agent_ctx.agent_id = Some(identity.agent_instance_id.clone());
        agent_ctx.agent_type = Some(identity.agent_type_name.clone());
    }

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
            let security_ctx =
                request_security_context(&headers, &agent_ctx, resolved_identity.as_ref());
            let auth_snapshot = match authorize_existing_mutation(
                &state,
                &tenant,
                &entity_type,
                &key_str,
                UPDATE_ACTION,
                &security_ctx,
                &agent_ctx,
            )
            .await
            {
                Ok(current_state) => current_state,
                Err(resp) => return *resp,
            };
            let current_state = &auth_snapshot.current_state;

            let raw_body = match parse_entity_object_body_or_400(&body) {
                Ok(v) => v,
                Err(resp) => return *resp,
            };
            let body_json =
                match entity_update_fields(&raw_body, &key_str, &current_state.state.status) {
                    Ok(fields) => fields,
                    Err(resp) => return *resp,
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
                return *resp;
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
                resolved_identity.as_ref(),
            )
            .await
            {
                return resp;
            }

            let expected_precondition =
                crate::entity_actor::effects::field_update_precondition(&current_state.state);
            match state
                .update_tenant_entity_fields_authorized(
                    &tenant,
                    &entity_type,
                    &key_str,
                    body_json,
                    false,
                    crate::state::FieldUpdateAuthorization {
                        target_precondition: expected_precondition,
                        context_guards: auth_snapshot.context_guards.clone(),
                        has_unguarded_context: auth_snapshot.has_unguarded_context,
                    },
                )
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
                Err(error) => field_update_error_response(&error),
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
    resolved_id: Option<Extension<ResolvedIdentity>>,
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
    let resolved_identity = resolved_id.map(|Extension(identity)| identity);
    let mut agent_ctx = extract_agent_context(&headers);
    if let Some(ref identity) = resolved_identity {
        agent_ctx.agent_id = Some(identity.agent_instance_id.clone());
        agent_ctx.agent_type = Some(identity.agent_type_name.clone());
    }

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
            let security_ctx =
                request_security_context(&headers, &agent_ctx, resolved_identity.as_ref());
            let auth_snapshot = match authorize_existing_mutation(
                &state,
                &tenant,
                &entity_type,
                &key_str,
                UPDATE_ACTION,
                &security_ctx,
                &agent_ctx,
            )
            .await
            {
                Ok(current_state) => current_state,
                Err(resp) => return *resp,
            };
            let current_state = &auth_snapshot.current_state;

            let raw_body = match parse_entity_object_body_or_400(&body) {
                Ok(v) => v,
                Err(resp) => return *resp,
            };
            let body_json =
                match entity_update_fields(&raw_body, &key_str, &current_state.state.status) {
                    Ok(fields) => fields,
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
                return *resp;
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
                resolved_identity.as_ref(),
            )
            .await
            {
                return resp;
            }

            let expected_precondition =
                crate::entity_actor::effects::field_update_precondition(&current_state.state);
            match state
                .update_tenant_entity_fields_authorized(
                    &tenant,
                    &entity_type,
                    &key_str,
                    body_json,
                    true,
                    crate::state::FieldUpdateAuthorization {
                        target_precondition: expected_precondition,
                        context_guards: auth_snapshot.context_guards.clone(),
                        has_unguarded_context: auth_snapshot.has_unguarded_context,
                    },
                )
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
                Err(error) => field_update_error_response(&error),
            }
        }
        ODataPath::Value { parent } => {
            let security_ctx =
                request_security_context(&headers, &agent_ctx, resolved_identity.as_ref());
            handle_stream_put(
                &state,
                &tenant,
                &parent,
                &headers,
                body,
                &agent_ctx,
                &security_ctx,
            )
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
    resolved_id: Option<Extension<ResolvedIdentity>>,
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
    let resolved_identity = resolved_id.map(|Extension(identity)| identity);
    let mut agent_ctx = extract_agent_context(&headers);
    if let Some(ref identity) = resolved_identity {
        agent_ctx.agent_id = Some(identity.agent_instance_id.clone());
        agent_ctx.agent_type = Some(identity.agent_type_name.clone());
    }

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
            let delete_target =
                match load_delete_target_or_404(&state, &tenant, &entity_type, &set_name, &key_str)
                    .await
                {
                    Ok(target) => target,
                    Err(resp) => return *resp,
                };
            if let DeleteTargetLifecycle::Deleted { sequence_nr } = delete_target {
                return match state
                    .retry_deleted_entity_cleanup(&tenant, &entity_type, &key_str, sequence_nr)
                    .await
                {
                    Ok(()) => (StatusCode::NO_CONTENT, "").into_response(),
                    Err(error) => delete_unavailable_response(error),
                };
            }
            let security_ctx =
                request_security_context(&headers, &agent_ctx, resolved_identity.as_ref());
            if let Err(resp) = authorize_existing_mutation(
                &state,
                &tenant,
                &entity_type,
                &key_str,
                DELETE_ACTION,
                &security_ctx,
                &agent_ctx,
            )
            .await
            {
                return *resp;
            }
            if let Err(error) =
                pre_delete_relation_checks(&state, &tenant, &entity_type, &key_str, "delete").await
            {
                return match error {
                    PreDeleteRelationError::Violation(violation) => {
                        constraint_violation_response(violation)
                    }
                    PreDeleteRelationError::Unavailable(error) => service_unavailable_response(
                        "RelationCheckUnavailable",
                        "Relationship verification is temporarily unavailable",
                        "pre_delete_relation_check",
                        error,
                    ),
                };
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
                return *resp;
            }

            if let Err(resp) = enforce_commons_write_rate_limit(
                &state,
                &tenant,
                &entity_type,
                owner_id_from_fields(&current_state.state.fields),
                &headers,
                &agent_ctx,
                resolved_identity.as_ref(),
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
                Err(error) => delete_unavailable_response(error),
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

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;

    #[test]
    fn stale_field_update_authorization_maps_to_conflict() {
        let response = super::field_update_error_response(
            "field update authorization became stale; retry against current state",
        );
        assert_eq!(response.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn field_update_backend_error_is_not_exposed() {
        let sentinel = "postgres password=secret redis://internal-host";
        let response = super::field_update_error_response(sentinel);
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("read sanitized error body");
        let body = String::from_utf8_lossy(&body);
        assert!(!body.contains(sentinel));
        assert!(!body.contains("internal-host"));
        assert!(body.contains("UpdateUnavailable"));
    }
}
