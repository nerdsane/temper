//! OData write handlers (`POST`, `PATCH`, `PUT`, `DELETE`).

use std::collections::BTreeSet;

use axum::extract::Query;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use temper_authz::AuthenticatedRequestContext;
use temper_odata::path::{ODataPath, parse_path};
use temper_runtime::persistence::PersistenceError;
use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;
use tracing::instrument;

use axum::Extension;

use super::account_verification::enforce_commons_account_verified_for_write;
use super::app_uniqueness::enforce_commons_app_name_unique_for_write;
use super::authz::{
    CREATE_ACTION, DELETE_ACTION, MutationResource, UPDATE_ACTION, apply_authenticated_context,
    authorize_mutation, require_authenticated_context, resource_attrs_from_body,
};
use super::bindings::dispatch_bound_action;
use super::common::{
    constraint_violation_response, extract_key, resolve_entity_type, run_write_prechecks,
    verification_gate_response,
};
use super::constraints::{DeletePlan, plan_delete};
use super::file_content::refuse_content_fields;
use super::rate_limit::enforce_commons_write_rate_limit;
use super::response::annotate_entity;
use super::storage_guardrails::enforce_commons_storage_cap;
use super::stream_put::{handle_file_value_put, handle_stream_put, streamed_file_target};
use crate::blobs::hydrate_blob_refs_for_tenant;
use crate::entity_actor::EntityState;
use crate::request_context::{AgentContext, extract_agent_context};
use crate::response::{ODataResponse, odata_error, odata_refusal};
use crate::state::batch_delete::BatchDeleteError;
use crate::state::redecide::Redecide;
use crate::state::trajectory::{TrajectoryEntry, TrajectorySource};
use crate::state::write_record::{RecordWrite, WriteCaller, created_record, field_update_record};
use crate::state::{CreateError, ServerState};

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

fn strict_generic_write_response(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
) -> Option<axum::response::Response> {
    state
        .has_strict_action_params(tenant, entity_type)
        .then(|| {
            odata_error(
                StatusCode::METHOD_NOT_ALLOWED,
                "StrictActionContract",
                "Strict entities require a declared action for updates and deletion",
            )
            .into_response()
        })
}

fn invalid_create_body(message: &str) -> ODataWriteError {
    Box::new(odata_error(StatusCode::BAD_REQUEST, "InvalidBody", message).into_response())
}

fn prepare_collection_create_fields(
    body: &serde_json::Value,
    entity_type: &str,
    initial_status: &str,
    idempotency_key: Option<&str>,
) -> Result<(String, serde_json::Value), ODataWriteError> {
    let mut fields = body
        .as_object()
        .cloned()
        .ok_or_else(|| invalid_create_body("Entity create body must be a JSON object"))?;

    let lower_id = match fields.get("id") {
        Some(value) => Some(
            value
                .as_str()
                .filter(|value| !value.is_empty())
                .ok_or_else(|| invalid_create_body("id must be a non-empty string"))?,
        ),
        None => None,
    };
    let upper_id = match fields.get("Id") {
        Some(value) => Some(
            value
                .as_str()
                .filter(|value| !value.is_empty())
                .ok_or_else(|| invalid_create_body("Id must be a non-empty string"))?,
        ),
        None => None,
    };
    if let (Some(lower), Some(upper)) = (lower_id, upper_id)
        && lower != upper
    {
        return Err(invalid_create_body(
            "id and Id must identify the same entity",
        ));
    }
    let entity_id = lower_id
        .or(upper_id)
        .map(str::to_string)
        .unwrap_or_else(|| {
            let prefix = entity_type_prefix(entity_type);
            format!("{prefix}{}", create_id(idempotency_key))
        });

    fields.retain(|key, _| !temper_spec::automaton::is_server_derived_field_name(key));
    for key in ["id", "Id"] {
        fields.insert(
            key.to_string(),
            serde_json::Value::String(entity_id.clone()),
        );
    }
    for key in ["status", "Status"] {
        fields.insert(
            key.to_string(),
            serde_json::Value::String(initial_status.to_string()),
        );
    }
    Ok((entity_id, serde_json::Value::Object(fields)))
}

fn validate_collection_create_status(
    body: &serde_json::Value,
    initial_status: &str,
) -> Result<(), ODataWriteError> {
    for key in ["status", "Status"] {
        if let Some(value) = body.get(key)
            && value.as_str() != Some(initial_status)
        {
            return Err(invalid_create_body(&format!(
                "{key} must equal the spec-defined initial state '{initial_status}'"
            )));
        }
    }
    Ok(())
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
            capture_seq: None,
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
                odata_error(StatusCode::INTERNAL_SERVER_ERROR, "ReadError", &error).into_response(),
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

async fn authorize_pg_mutation(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
    action: &str,
    security_ctx: &temper_authz::SecurityContext,
    agent_ctx: &AgentContext,
) -> Result<(temper_actor_runtime::spec_actor::SpecActorState, i64), ODataWriteError> {
    let actor_sys = state.pg_actor_system.as_ref().ok_or_else(|| {
        Box::new(
            odata_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "ActorSystemUnavailable",
                "The actor system for this resource is unavailable",
            )
            .into_response(),
        )
    })?;
    let namespace = format!("{tenant}/{entity_id}");
    let (state_bytes, version) = match actor_sys
        .load_state_with_version(&namespace, entity_type)
        .await
    {
        Ok(Some(snapshot)) => snapshot,
        Ok(None) => {
            return Err(Box::new(
                odata_error(
                    StatusCode::NOT_FOUND,
                    "ResourceNotFound",
                    &format!("Entity '{entity_type}' with key '{entity_id}' not found"),
                )
                .into_response(),
            ));
        }
        Err(error) => {
            return Err(Box::new(
                odata_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "ActorReadError",
                    &error.to_string(),
                )
                .into_response(),
            ));
        }
    };
    let actor_state: temper_actor_runtime::spec_actor::SpecActorState =
        match serde_json::from_slice(&state_bytes) {
            Ok(value) => value,
            Err(_) => {
                return Err(Box::new(
                    odata_error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "ActorReadError",
                        "Stored actor state is invalid",
                    )
                    .into_response(),
                ));
            }
        };
    let authz_body = serde_json::json!({
        "entity_id": entity_id,
        "status": actor_state.status,
        "fields": actor_state.fields,
    });
    let attrs = resource_attrs_from_body(state, tenant, entity_type, entity_id, &authz_body);
    if let Err(response) = authorize_mutation(
        state,
        tenant,
        security_ctx,
        agent_ctx,
        action,
        MutationResource {
            entity_type,
            entity_id,
            attrs: &attrs,
        },
    )
    .await
    {
        return Err(Box::new(response));
    }
    Ok((actor_state, version))
}

/// The record a Postgres-runtime action stores: its actor's own handler run
/// on a copy of `current`, with the status beside the fields as field
/// invariants read it. `None` when the actor refuses the action and stores
/// nothing.
async fn pg_action_record(
    actor_sys: &temper_actor_runtime::ActorSystem,
    handle: &temper_actor_runtime::ActorHandle,
    current: &temper_actor_runtime::spec_actor::SpecActorState,
    action: &str,
    params: &serde_json::Value,
) -> Result<Option<serde_json::Value>, ODataWriteError> {
    let internal = |message: String| {
        Box::new(
            odata_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "ActorPreviewError",
                &message,
            )
            .into_response(),
        )
    };
    let state = serde_json::to_vec(current).map_err(|error| internal(error.to_string()))?;
    let message =
        temper_actor_runtime::spec_actor::SpecMessage::with_params(action, params.clone());
    let next = match actor_sys.preview(handle, &state, message).await {
        Ok(next) => next,
        Err(temper_actor_runtime::ActorError::Rejected(_)) => return Ok(None),
        Err(error) => return Err(internal(error.to_string())),
    };
    let next: temper_actor_runtime::spec_actor::SpecActorState =
        serde_json::from_slice(&next).map_err(|error| internal(error.to_string()))?;
    let mut record = match next.fields {
        serde_json::Value::Object(fields) => fields,
        _ => serde_json::Map::new(),
    };
    for key in ["status", "Status"] {
        record.insert(
            key.to_string(),
            serde_json::Value::String(next.status.clone()),
        );
    }
    Ok(Some(serde_json::Value::Object(record)))
}

async fn authorize_existing_mutation(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
    action: &str,
    security_ctx: &temper_authz::SecurityContext,
    agent_ctx: &AgentContext,
) -> Result<ExistingMutationResource, ODataWriteError> {
    let snapshot = state
        .load_authz_resource_snapshot(tenant, entity_type, entity_id)
        .await
        .map_err(|error| {
            Box::new(
                odata_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "ReadError",
                    &error.to_string(),
                )
                .into_response(),
            )
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
    let precondition = crate::entity_actor::effects::entity_authorization_precondition(
        &snapshot.current_state.state,
    );
    Ok(ExistingMutationResource {
        state: snapshot.current_state.state,
        precondition,
    })
}

struct ExistingMutationResource {
    /// The authorized entity state; the write commits only while it is current.
    state: EntityState,
    precondition: String,
}

struct ProspectiveMutationAuthorization<'a> {
    tenant: &'a TenantId,
    entity_type: &'a str,
    entity_id: &'a str,
    status: &'a str,
    fields: &'a serde_json::Value,
    security_ctx: &'a temper_authz::SecurityContext,
    agent_ctx: &'a AgentContext,
}

async fn authorize_prospective_mutation(
    state: &ServerState,
    request: ProspectiveMutationAuthorization<'_>,
) -> Result<(), ODataWriteError> {
    let ProspectiveMutationAuthorization {
        tenant,
        entity_type,
        entity_id,
        status,
        fields,
        security_ctx,
        agent_ctx,
    } = request;
    let attrs = state
        .build_authz_resource_attrs(tenant, entity_type, entity_id, status, fields)
        .await
        .map_err(|error| {
            Box::new(
                odata_error(StatusCode::INTERNAL_SERVER_ERROR, "ReadError", &error).into_response(),
            )
        })?;
    authorize_mutation(
        state,
        tenant,
        security_ctx,
        agent_ctx,
        UPDATE_ACTION,
        MutationResource {
            entity_type,
            entity_id,
            attrs: &attrs,
        },
    )
    .await
    .map_err(Box::new)
}

/// Handle POST requests — entity creation and bound actions.
#[instrument(skip_all, fields(otel.name = "POST /odata/{path}"))]
pub async fn handle_odata_post(
    State(state): State<ServerState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    headers: HeaderMap,
    axum::extract::Path(path): axum::extract::Path<String>,
    Query(query_params): Query<std::collections::BTreeMap<String, String>>,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let authenticated = match require_authenticated_context(authenticated) {
        Ok(context) => context,
        Err(error) => return error.into_response(),
    };
    let tenant = authenticated.tenant().clone();
    let security_ctx = authenticated.security_context().clone();
    let mut agent_ctx = extract_agent_context(&headers);
    apply_authenticated_context(&mut agent_ctx, &security_ctx);
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
            let body_json = match parse_json_body_or_400(&body) {
                Ok(v) => v,
                Err(resp) => return *resp,
            };
            if let Some(response) = refuse_content_fields(&entity_type, &body_json) {
                return response;
            }

            let initial_status = match state.initial_entity_status(&tenant, &entity_type) {
                Ok(status) => status,
                Err(error) => {
                    return odata_error(StatusCode::INTERNAL_SERVER_ERROR, "ReadError", &error)
                        .into_response();
                }
            };
            let (entity_id, initial_fields) = match prepare_collection_create_fields(
                &body_json,
                &entity_type,
                &initial_status,
                idempotency_key.as_deref(),
            ) {
                Ok(prepared) => prepared,
                Err(response) => return *response,
            };
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
            if let Err(resp) = check_verification_gate_or_423(&state, &tenant, &entity_type) {
                return *resp;
            }
            if let Err(resp) = validate_collection_create_status(&body_json, &initial_status) {
                return *resp;
            }
            if let Err(error) =
                state.validate_initial_entity_fields(&tenant, &entity_type, &initial_fields)
            {
                return odata_error(StatusCode::BAD_REQUEST, "StrictActionContract", &error)
                    .into_response();
            }
            let created_record = match state.transition_table_for_dispatch(&tenant, &entity_type) {
                Ok(table) => created_record(&entity_type, &entity_id, &table, &initial_fields),
                Err(error) => {
                    return odata_error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "RegistryError",
                        &error.to_string(),
                    )
                    .into_response();
                }
            };
            let _commons_guardrail_lock = state.acquire_commons_write_guardrail_lock(&tenant).await;

            if let Err(resp) = run_write_prechecks(
                &state,
                RecordWrite {
                    caller: WriteCaller::of_request(&security_ctx),
                    tenant: &tenant,
                    entity_type: &entity_type,
                    entity_id: &entity_id,
                    action: "Create",
                    operation: "create",
                },
                &created_record,
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
                &security_ctx,
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

            if let Err(resp) =
                enforce_commons_write_rate_limit(&state, &tenant, &entity_type, &security_ctx).await
            {
                return resp;
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
            if state.is_pg_actor_backed(&tenant, &entity_type) {
                let Some(actor_sys) = &state.pg_actor_system else {
                    return odata_error(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "ActorSystemUnavailable",
                        "The actor system for this resource is unavailable",
                    )
                    .into_response();
                };
                let namespace = format!("{tenant}/{entity_id}");
                let spawn_result = actor_sys
                    .spawn_with_fields(&namespace, &entity_type, initial_fields.clone())
                    .await;
                match spawn_result {
                    Ok(_) => {
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

            // A new entity's first event holds its initial fields: one too large, or
            // one the disk has no room for, is refused here, before any create path
            // starts.
            if let Err(error) =
                state.check_created_event(&tenant, &entity_type, &entity_id, &initial_fields)
            {
                let (status, code) = match error {
                    PersistenceError::EventTooLarge { .. } => {
                        (StatusCode::PAYLOAD_TOO_LARGE, "EventTooLarge")
                    }
                    PersistenceError::StorageFull { .. } => {
                        return crate::write_health::store_unavailable(&error.to_string());
                    }
                    _ => (StatusCode::INTERNAL_SERVER_ERROR, "CreateError"),
                };
                return odata_error(status, code, &error.to_string()).into_response();
            }

            match state
                .try_create_data_only_tenant_entity_by(
                    &tenant,
                    &entity_type,
                    &entity_id,
                    initial_fields.clone(),
                    Some(&agent_ctx),
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
                    return odata_error(StatusCode::INTERNAL_SERVER_ERROR, "CreateError", &e)
                        .into_response();
                }
            }

            // A timed first state's timer keeps whether its creator could
            // read the record.
            let creator = match state
                .created_with_read_when_set(
                    &tenant,
                    &entity_type,
                    &entity_id,
                    &initial_fields,
                    &agent_ctx,
                )
                .await
            {
                Ok(creator) => creator,
                Err(reason) => return crate::write_health::store_unavailable(&reason),
            };
            match state
                .get_or_create_tenant_entity_by(
                    &tenant,
                    &entity_type,
                    &entity_id,
                    initial_fields,
                    &creator,
                )
                .await
            {
                Ok(response) => {
                    // Creation is not a dispatch: a record created into a
                    // timed first state has its timer armed here. Its row
                    // committed with the `Created` event when the store keeps
                    // rows with the journal.
                    state
                        .arm_state_timeouts_on_creation(
                            &tenant,
                            &entity_type,
                            &entity_id,
                            &response,
                            &creator,
                        )
                        .await;
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
                Err(CreateError::StoreUnavailable(reason)) => {
                    crate::write_health::store_unavailable(&reason)
                }
                Err(CreateError::Failed(e)) => {
                    odata_error(StatusCode::INTERNAL_SERVER_ERROR, "CreateError", &e)
                        .into_response()
                }
            }
        }

        ODataPath::BoundAction { parent, action } => {
            let body_json = if body.iter().all(u8::is_ascii_whitespace) {
                serde_json::json!({})
            } else {
                match parse_json_body_or_400(&body) {
                    Ok(value) => value,
                    Err(response) => return *response,
                }
            };

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

            if temper_spec::automaton::is_carry_move(action.rsplit('.').next().unwrap_or(&action)) {
                return odata_error(
                    StatusCode::FORBIDDEN,
                    "PlatformMove",
                    "A [[carry]] move is made only by the platform, when it installs a new \
                     version of the spec",
                )
                .into_response();
            }

            if state.is_pg_actor_backed(&tenant, &entity_type) {
                let Some(actor_sys) = &state.pg_actor_system else {
                    return odata_error(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "ActorSystemUnavailable",
                        "The actor system for this resource is unavailable",
                    )
                    .into_response();
                };
                let body_json = match state.resolve_authenticated_params(
                    &tenant,
                    &entity_type,
                    &action,
                    body_json,
                    &agent_ctx,
                ) {
                    Ok(params) => params,
                    Err(error) => {
                        return odata_error(
                            StatusCode::BAD_REQUEST,
                            "AuthenticatedParameter",
                            &error.to_string(),
                        )
                        .into_response();
                    }
                };
                let namespace = format!("{tenant}/{key_str}");
                let handle =
                    temper_actor_runtime::ActorHandle::new(namespace.clone(), entity_type.clone());
                let action_name = action.rsplit('.').next().unwrap_or(&action);
                let (actor_state, authorization_version) = match authorize_pg_mutation(
                    &state,
                    &tenant,
                    &entity_type,
                    &key_str,
                    &action,
                    &security_ctx,
                    &agent_ctx,
                )
                .await
                {
                    Ok(state) => state,
                    Err(response) => return *response,
                };
                if let Err(resp) = check_verification_gate_or_423(&state, &tenant, &entity_type) {
                    return *resp;
                }
                {
                    let registry = state.registry.read().expect("registry lock poisoned");
                    if let Some(table) = registry.get_table(&tenant, &entity_type)
                        && let Err(error) = table.validate_action_params(
                            action_name,
                            &body_json,
                            &actor_state.fields,
                            &actor_state.counters,
                            &actor_state.booleans,
                        )
                    {
                        return odata_refusal(
                            StatusCode::BAD_REQUEST,
                            "StrictActionContract",
                            &error.message,
                            error.refusal.as_ref(),
                        )
                        .into_response();
                    }
                }
                let caller = WriteCaller::of_request(&security_ctx);
                if state.action_record_checked(caller, &tenant, &entity_type, action_name) {
                    // The record checked is the one this runtime's own actor stores.
                    let record = match pg_action_record(
                        actor_sys,
                        &handle,
                        &actor_state,
                        action_name,
                        &body_json,
                    )
                    .await
                    {
                        Ok(record) => record,
                        Err(response) => return *response,
                    };
                    if let Some(record) = record
                        && let Err(violation) = state
                            .check_record_to_store(
                                RecordWrite {
                                    caller,
                                    tenant: &tenant,
                                    entity_type: &entity_type,
                                    entity_id: &key_str,
                                    action: action_name,
                                    operation: "action",
                                },
                                &record,
                            )
                            .await
                    {
                        return constraint_violation_response(violation);
                    }
                }
                match actor_sys
                    .tell(
                        None,
                        &handle,
                        temper_actor_runtime::spec_actor::SpecMessage::with_params(
                            action_name,
                            body_json.clone(),
                        )
                        .if_state_version(authorization_version),
                    )
                    .await
                {
                    Ok(message_id) => {
                        // Enqueueing does not establish which FIFO message will execute next.
                        return ODataResponse {
                            status: StatusCode::ACCEPTED,
                            body: serde_json::json!({"entity_type":entity_type,"entity_id":key_str,"action":action_name,"message_id":message_id,"accepted":true}),
                        }.into_response();
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
                await_integration,
                idempotency_key.clone(),
                &security_ctx,
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
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    headers: HeaderMap,
    axum::extract::Path(path): axum::extract::Path<String>,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let authenticated = match require_authenticated_context(authenticated) {
        Ok(context) => context,
        Err(error) => return error.into_response(),
    };
    let tenant = authenticated.tenant().clone();
    let security_ctx = authenticated.security_context().clone();
    let odata_path = match parse_odata_path_or_400(&path) {
        Ok(p) => p,
        Err(resp) => return *resp,
    };
    let mut agent_ctx = extract_agent_context(&headers);
    apply_authenticated_context(&mut agent_ctx, &security_ctx);

    match odata_path {
        ODataPath::Entity(set_name, key) => {
            let entity_type = match resolve_entity_type_or_404(&state, &tenant, &set_name) {
                Ok(t) => t,
                Err(resp) => return *resp,
            };
            let key_str = extract_key(&key);

            // PG instances are authorized from their persisted state, before
            // refusing verbs that the strict type does not support.
            if state.is_pg_actor_backed(&tenant, &entity_type)
                && let Some(response) = strict_generic_write_response(&state, &tenant, &entity_type)
            {
                if let Err(error) = authorize_pg_mutation(
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
                    return *error;
                }
                if let Err(resp) = check_verification_gate_or_423(&state, &tenant, &entity_type) {
                    return *resp;
                }
                return response;
            }
            let mut redecide = Redecide::start(&state);
            loop {
                if let Err(resp) =
                    ensure_entity_exists_or_404(&state, &tenant, &entity_type, &set_name, &key_str)
                {
                    return *resp;
                }
                let existing = match authorize_existing_mutation(
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
                    Ok(existing) => existing,
                    Err(resp) => return *resp,
                };
                if let Err(resp) = check_verification_gate_or_423(&state, &tenant, &entity_type) {
                    return *resp;
                }
                if let Some(response) = strict_generic_write_response(&state, &tenant, &entity_type)
                {
                    return response;
                }

                let body_json = match parse_json_body_or_400(&body) {
                    Ok(v) => v,
                    Err(resp) => return *resp,
                };
                if !body_json.is_object() {
                    return odata_error(
                        StatusCode::BAD_REQUEST,
                        "InvalidBody",
                        "PATCH body must be a JSON object",
                    )
                    .into_response();
                }
                if let Some(response) = refuse_content_fields(&entity_type, &body_json) {
                    return response;
                }
                let mut prospective_fields = existing.state.fields.clone();
                if let (Some(dst), Some(src)) =
                    (prospective_fields.as_object_mut(), body_json.as_object())
                {
                    for (k, v) in src {
                        dst.insert(k.clone(), v.clone());
                    }
                } else {
                    prospective_fields = body_json.clone();
                }

                if let Err(response) = authorize_prospective_mutation(
                    &state,
                    ProspectiveMutationAuthorization {
                        tenant: &tenant,
                        entity_type: &entity_type,
                        entity_id: &key_str,
                        status: &existing.state.status,
                        fields: &prospective_fields,
                        security_ctx: &security_ctx,
                        agent_ctx: &agent_ctx,
                    },
                )
                .await
                {
                    return *response;
                }

                let _commons_guardrail_lock =
                    state.acquire_commons_write_guardrail_lock(&tenant).await;

                let record = field_update_record(&existing.state, &body_json, false);
                if let Err(resp) = run_write_prechecks(
                    &state,
                    RecordWrite {
                        caller: WriteCaller::of_request(&security_ctx),
                        tenant: &tenant,
                        entity_type: &entity_type,
                        entity_id: &key_str,
                        action: "Patch",
                        operation: "patch",
                    },
                    &record,
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
                    &security_ctx,
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

                if redecide.is_first()
                    && let Err(resp) = enforce_commons_write_rate_limit(
                        &state,
                        &tenant,
                        &entity_type,
                        &security_ctx,
                    )
                    .await
                {
                    return resp;
                }

                redecide.dispatching().await;
                let answer = state
                    .update_tenant_entity_fields_if_current(
                        &tenant,
                        &entity_type,
                        &key_str,
                        body_json,
                        false,
                        existing.precondition,
                        &agent_ctx,
                    )
                    .await;
                if let Ok(response) = &answer
                    && redecide.again(response.is_record_changed(), "patch")
                {
                    continue;
                }
                return match answer {
                    Ok(response) if response.success => {
                        if entity_type == "RateLimit" {
                            state.clear_commons_rate_limit_cache();
                        }
                        state.clear_commons_storage_projection_cache_for_entity(&entity_type);
                        let mut state_json =
                            serde_json::to_value(&response.state).unwrap_or_default();
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
                    Ok(response) if response.store_unavailable => {
                        crate::write_health::store_unavailable(
                            response
                                .error
                                .as_deref()
                                .unwrap_or("the store did not take the write"),
                        )
                    }
                    Ok(response) => odata_error(
                        StatusCode::CONFLICT,
                        "ConcurrentModification",
                        response.error.as_deref().unwrap_or(
                            "entity changed after authorization; retry against current state",
                        ),
                    )
                    .into_response(),
                    Err(e) => odata_error(StatusCode::INTERNAL_SERVER_ERROR, "UpdateError", &e)
                        .into_response(),
                };
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
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    headers: HeaderMap,
    axum::extract::Path(path): axum::extract::Path<String>,
    request: axum::extract::Request,
) -> impl IntoResponse {
    let authenticated = match require_authenticated_context(authenticated) {
        Ok(context) => context,
        Err(error) => return error.into_response(),
    };
    let tenant = authenticated.tenant().clone();
    let security_ctx = authenticated.security_context().clone();
    let odata_path = match parse_odata_path_or_400(&path) {
        Ok(p) => p,
        Err(resp) => return *resp,
    };
    let mut agent_ctx = extract_agent_context(&headers);
    apply_authenticated_context(&mut agent_ctx, &security_ctx);

    // A File's bytes stream to the blob store; every other body is buffered within the router's body limit.
    if let Some(file_id) = streamed_file_target(&state, &tenant, &odata_path, &headers) {
        return handle_file_value_put(
            &state,
            &tenant,
            &file_id,
            &headers,
            request.into_body(),
            &agent_ctx,
            &security_ctx,
        )
        .await;
    }
    let body = match <axum::body::Bytes as axum::extract::FromRequest<ServerState>>::from_request(
        request, &state,
    )
    .await
    {
        Ok(body) => body,
        Err(rejection) => return rejection.into_response(),
    };

    match odata_path {
        ODataPath::Entity(set_name, key) => {
            let entity_type = match resolve_entity_type_or_404(&state, &tenant, &set_name) {
                Ok(t) => t,
                Err(resp) => return *resp,
            };
            let key_str = extract_key(&key);

            // PG instances are authorized from their persisted state, before
            // refusing verbs that the strict type does not support.
            if state.is_pg_actor_backed(&tenant, &entity_type)
                && let Some(response) = strict_generic_write_response(&state, &tenant, &entity_type)
            {
                if let Err(error) = authorize_pg_mutation(
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
                    return *error;
                }
                if let Err(resp) = check_verification_gate_or_423(&state, &tenant, &entity_type) {
                    return *resp;
                }
                return response;
            }
            let mut redecide = Redecide::start(&state);
            loop {
                if let Err(resp) =
                    ensure_entity_exists_or_404(&state, &tenant, &entity_type, &set_name, &key_str)
                {
                    return *resp;
                }
                let existing = match authorize_existing_mutation(
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
                    Ok(existing) => existing,
                    Err(resp) => return *resp,
                };
                if let Err(resp) = check_verification_gate_or_423(&state, &tenant, &entity_type) {
                    return *resp;
                }
                if let Some(response) = strict_generic_write_response(&state, &tenant, &entity_type)
                {
                    return response;
                }

                let body_json = match parse_json_body_or_400(&body) {
                    Ok(v) => v,
                    Err(resp) => return *resp,
                };
                if !body_json.is_object() {
                    return odata_error(
                        StatusCode::BAD_REQUEST,
                        "InvalidBody",
                        "PUT body must be a JSON object",
                    )
                    .into_response();
                }
                if let Some(response) = refuse_content_fields(&entity_type, &body_json) {
                    return response;
                }

                if let Err(response) = authorize_prospective_mutation(
                    &state,
                    ProspectiveMutationAuthorization {
                        tenant: &tenant,
                        entity_type: &entity_type,
                        entity_id: &key_str,
                        status: &existing.state.status,
                        fields: &body_json,
                        security_ctx: &security_ctx,
                        agent_ctx: &agent_ctx,
                    },
                )
                .await
                {
                    return *response;
                }

                let _commons_guardrail_lock =
                    state.acquire_commons_write_guardrail_lock(&tenant).await;

                let record = field_update_record(&existing.state, &body_json, true);
                if let Err(resp) = run_write_prechecks(
                    &state,
                    RecordWrite {
                        caller: WriteCaller::of_request(&security_ctx),
                        tenant: &tenant,
                        entity_type: &entity_type,
                        entity_id: &key_str,
                        action: "Put",
                        operation: "put",
                    },
                    &record,
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
                    &security_ctx,
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

                if redecide.is_first()
                    && let Err(resp) = enforce_commons_write_rate_limit(
                        &state,
                        &tenant,
                        &entity_type,
                        &security_ctx,
                    )
                    .await
                {
                    return resp;
                }

                redecide.dispatching().await;
                let answer = state
                    .update_tenant_entity_fields_if_current(
                        &tenant,
                        &entity_type,
                        &key_str,
                        body_json,
                        true,
                        existing.precondition,
                        &agent_ctx,
                    )
                    .await;
                if let Ok(response) = &answer
                    && redecide.again(response.is_record_changed(), "put")
                {
                    continue;
                }
                return match answer {
                    Ok(response) if response.success => {
                        if entity_type == "RateLimit" {
                            state.clear_commons_rate_limit_cache();
                        }
                        state.clear_commons_storage_projection_cache_for_entity(&entity_type);
                        let mut state_json =
                            serde_json::to_value(&response.state).unwrap_or_default();
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
                    Ok(response) if response.store_unavailable => {
                        crate::write_health::store_unavailable(
                            response
                                .error
                                .as_deref()
                                .unwrap_or("the store did not take the write"),
                        )
                    }
                    Ok(response) => odata_error(
                        StatusCode::CONFLICT,
                        "ConcurrentModification",
                        response.error.as_deref().unwrap_or(
                            "entity changed after authorization; retry against current state",
                        ),
                    )
                    .into_response(),
                    Err(e) => odata_error(StatusCode::INTERNAL_SERVER_ERROR, "UpdateError", &e)
                        .into_response(),
                };
            }
        }
        ODataPath::Value { parent } => handle_stream_put(
            &state,
            &tenant,
            &parent,
            &headers,
            body,
            &agent_ctx,
            &security_ctx,
        )
        .await
        .into_response(),
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
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    headers: HeaderMap,
    axum::extract::Path(path): axum::extract::Path<String>,
) -> impl IntoResponse {
    let authenticated = match require_authenticated_context(authenticated) {
        Ok(context) => context,
        Err(error) => return error.into_response(),
    };
    let tenant = authenticated.tenant().clone();
    let security_ctx = authenticated.security_context().clone();
    let odata_path = match parse_odata_path_or_400(&path) {
        Ok(p) => p,
        Err(resp) => return *resp,
    };
    let mut agent_ctx = extract_agent_context(&headers);
    apply_authenticated_context(&mut agent_ctx, &security_ctx);

    match odata_path {
        ODataPath::Entity(set_name, key) => {
            let entity_type = match resolve_entity_type_or_404(&state, &tenant, &set_name) {
                Ok(t) => t,
                Err(resp) => return *resp,
            };
            let key_str = extract_key(&key);

            // PG instances are authorized from their persisted state, before
            // refusing verbs that the strict type does not support.
            if state.is_pg_actor_backed(&tenant, &entity_type)
                && let Some(response) = strict_generic_write_response(&state, &tenant, &entity_type)
            {
                if let Err(error) = authorize_pg_mutation(
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
                    return *error;
                }
                if let Err(resp) = check_verification_gate_or_423(&state, &tenant, &entity_type) {
                    return *resp;
                }
                return response;
            }
            let mut redecide = Redecide::start(&state);
            loop {
                if let Err(resp) =
                    ensure_entity_exists_or_404(&state, &tenant, &entity_type, &set_name, &key_str)
                {
                    return *resp;
                }
                let existing = match authorize_existing_mutation(
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
                    Ok(existing) => existing,
                    Err(resp) => return *resp,
                };
                if let Err(resp) = check_verification_gate_or_423(&state, &tenant, &entity_type) {
                    return *resp;
                }
                if let Some(response) = strict_generic_write_response(&state, &tenant, &entity_type)
                {
                    return response;
                }
                let plan =
                    match plan_delete(&state, &tenant, &entity_type, &key_str, "delete").await {
                        Ok(plan) => plan,
                        Err(v) => return constraint_violation_response(v),
                    };
                if let Err(resp) = run_write_prechecks(
                    &state,
                    RecordWrite {
                        caller: WriteCaller::of_request(&security_ctx),
                        tenant: &tenant,
                        entity_type: &entity_type,
                        entity_id: &key_str,
                        action: "Delete",
                        operation: "delete",
                    },
                    &existing.state.fields,
                )
                .await
                {
                    return resp;
                }

                if let Err(resp) = enforce_commons_account_verified_for_write(
                    &state,
                    &tenant,
                    &entity_type,
                    &existing.state.fields,
                    &security_ctx,
                )
                .await
                {
                    return *resp;
                }

                if redecide.is_first()
                    && let Err(resp) = enforce_commons_write_rate_limit(
                        &state,
                        &tenant,
                        &entity_type,
                        &security_ctx,
                    )
                    .await
                {
                    return resp;
                }

                redecide.dispatching().await;
                if !plan.dependents().is_empty() {
                    match delete_with_dependents(
                        &state,
                        &tenant,
                        &entity_type,
                        &key_str,
                        &existing.precondition,
                        plan,
                    )
                    .await
                    {
                        CascadeAnswer::Answered(response) => return response,
                        CascadeAnswer::Changed(_) if redecide.again(true, "delete") => continue,
                        CascadeAnswer::Changed(reason) => {
                            return odata_error(
                                StatusCode::CONFLICT,
                                "ConcurrentModification",
                                &reason,
                            )
                            .into_response();
                        }
                    }
                }
                let answer = state
                    .delete_tenant_entity_if_current(
                        &tenant,
                        &entity_type,
                        &key_str,
                        existing.precondition,
                        &agent_ctx,
                    )
                    .await;
                if let Ok(response) = &answer
                    && redecide.again(response.is_record_changed(), "delete")
                {
                    continue;
                }
                return match answer {
                    Ok(response) if response.success => {
                        forget_deleted_entity(&state, &entity_type);
                        (StatusCode::NO_CONTENT, "").into_response()
                    }
                    Ok(response) if response.store_unavailable => {
                        crate::write_health::store_unavailable(
                            response
                                .error
                                .as_deref()
                                .unwrap_or("the store did not take the write"),
                        )
                    }
                    Ok(response) => odata_error(
                        StatusCode::CONFLICT,
                        "ConcurrentModification",
                        response.error.as_deref().unwrap_or(
                            "entity changed after authorization; retry against current state",
                        ),
                    )
                    .into_response(),
                    Err(e) => odata_error(StatusCode::INTERNAL_SERVER_ERROR, "DeleteError", &e)
                        .into_response(),
                };
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

/// Delete `entity_type('entity_id')` and the dependents `plan` found, all
/// together and only while each is in the state it was checked in: the
/// requested entity's as it was authorized, each dependent's as the plan read
/// it (`ServerState::delete_entities_together`). When any of them has changed
/// since, or the server keeps no journal, nothing is deleted.
async fn delete_with_dependents(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
    authorized_precondition: &str,
    plan: DeletePlan,
) -> CascadeAnswer {
    let root = match state
        .get_tenant_entity_state(tenant, entity_type, entity_id)
        .await
    {
        Ok(response) => response.state,
        Err(error) => {
            return CascadeAnswer::Answered(
                odata_error(StatusCode::INTERNAL_SERVER_ERROR, "DeleteError", &error)
                    .into_response(),
            );
        }
    };
    if crate::entity_actor::effects::entity_authorization_precondition(&root)
        != authorized_precondition
    {
        return CascadeAnswer::Changed(
            "entity changed after authorization; retry against current state".to_string(),
        );
    }

    let dependents = plan.into_dependents();
    let count = dependents.len();
    let mut removed_types: BTreeSet<String> = dependents
        .iter()
        .map(|dependent| dependent.entity_type.clone())
        .collect();
    removed_types.insert(entity_type.to_string());
    let mut members = Vec::with_capacity(count + 1);
    members.push(root);
    members.extend(dependents);

    let answer = match state.delete_entities_together(tenant, members).await {
        Ok(()) => {
            for removed_type in &removed_types {
                forget_deleted_entity(state, removed_type);
            }
            (StatusCode::NO_CONTENT, "").into_response()
        }
        Err(BatchDeleteError::Changed(reason)) => {
            return CascadeAnswer::Changed(format!(
                "nothing was deleted: {reason}; retry against current state"
            ));
        }
        // 501, as a secret store that cannot delete answers: retrying cannot
        // help, only a server with a journal can.
        Err(BatchDeleteError::NoJournal) => odata_error(
            StatusCode::NOT_IMPLEMENTED,
            "NotImplemented",
            &format!(
                "this server keeps no journal, so it cannot delete {entity_type}('{entity_id}') \
                 and the {count} entities that belong to it as one change; nothing was deleted"
            ),
        )
        .into_response(),
        Err(BatchDeleteError::Storage(error)) => odata_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "DeleteError",
            &format!(
                "could not delete {entity_type}('{entity_id}') with the {count} entities that \
                 belong to it: {error}"
            ),
        )
        .into_response(),
    };
    CascadeAnswer::Answered(answer)
}

/// What a delete with dependents did.
enum CascadeAnswer {
    /// Its answer.
    Answered(axum::response::Response),
    /// The requested entity or one it removes changed after the delete was
    /// decided, and nothing was deleted: the delete is decided again on what
    /// they are now (`Redecide`).
    Changed(String),
}

/// Drop the caches that hold rows of `entity_type` after one is deleted.
fn forget_deleted_entity(state: &ServerState, entity_type: &str) {
    if entity_type == "RateLimit" {
        state.clear_commons_rate_limit_cache();
    }
    state.clear_commons_storage_projection_cache_for_entity(entity_type);
}

/// The id of a record created without one: the create's idempotency key, so
/// the same create sent again with its key finds the record the first made
/// (creating a record that exists answers that record). A key that is a UUID,
/// as every key the kernel gives a write is, is the id as it is; any other
/// key is named by a UUID derived from it.
fn create_id(idempotency_key: Option<&str>) -> uuid::Uuid {
    match idempotency_key {
        Some(key) => uuid::Uuid::try_parse(key)
            .unwrap_or_else(|_| uuid::Uuid::new_v5(&CREATE_KEY_NAMESPACE, key.as_bytes())),
        None => temper_runtime::scheduler::sim_uuid(),
    }
}

/// The namespace of ids derived from a create's own key.
const CREATE_KEY_NAMESPACE: uuid::Uuid = uuid::uuid!("6f1d3c52-8a4e-4b7f-9c2d-5e0a1b3c4d5e");

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
