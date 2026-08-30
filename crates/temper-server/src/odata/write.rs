//! OData write handlers (`POST`, `PATCH`, `PUT`, `DELETE`).

use axum::extract::Query;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use temper_authz::AuthenticatedRequestContext;
use temper_odata::path::{ODataPath, parse_path};
use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;
use tracing::instrument;
use tracing_opentelemetry::OpenTelemetrySpanExt;

use axum::Extension;

use super::account_verification::enforce_commons_account_verified_for_write;
use super::action_input::validate_bound_action_input;
use super::app_uniqueness::enforce_commons_app_name_unique_for_write;
use super::authz::{
    CREATE_ACTION, DELETE_ACTION, MutationResource, UPDATE_ACTION, apply_authenticated_context,
    authorize_mutation, require_authenticated_context, resource_attrs_from_body,
};
use super::bindings::dispatch_bound_action;
use super::common::{
    constraint_violation_response, extract_key, extract_schema_pin, resolve_entity_type_for_pin,
    run_write_prechecks, verification_gate_response,
};
use super::constraints::pre_delete_relation_checks;
use super::rate_limit::{enforce_commons_write_rate_limit, owner_id_from_fields};
use super::response::annotate_entity;
use super::schema_pin::{
    resolve_scope_only_entity_pin, schema_pin_extraction_error_response,
    schema_pin_mismatch_response,
};
use super::storage_guardrails::enforce_commons_storage_cap;
use super::stream_put::handle_stream_put;
use crate::blobs::hydrate_blob_refs_for_tenant;
use crate::request_context::{AgentContext, extract_agent_context, remote_parent_context};
use crate::response::{ODataResponse, odata_error};
use crate::state::trajectory::{TrajectoryEntry, TrajectorySource};
use crate::state::{ServerState, validate_global_entity_id};

type ODataWriteError = Box<axum::response::Response>;

pub(super) fn reference_contract_response(error: &str) -> Option<axum::response::Response> {
    if !crate::entity_actor::reference_contract::is_reference_contract_error(error) {
        return None;
    }
    let status = if error.contains("InvalidReferenceValue") {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::CONFLICT
    };
    Some(odata_error(status, "ConstraintViolation", error).into_response())
}

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

fn invalid_create_body(message: &str) -> ODataWriteError {
    Box::new(odata_error(StatusCode::BAD_REQUEST, "InvalidBody", message).into_response())
}

fn prepare_collection_create_fields(
    body: serde_json::Value,
    entity_type: &str,
    initial_status: &str,
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
            format!("{prefix}{}", temper_runtime::scheduler::sim_uuid())
        });

    for key in ["status", "Status"] {
        if let Some(value) = fields.get(key)
            && value.as_str() != Some(initial_status)
        {
            return Err(invalid_create_body(&format!(
                "{key} must equal the spec-defined initial state '{initial_status}'"
            )));
        }
    }

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

fn resolve_entity_type_or_404(
    state: &ServerState,
    tenant: &TenantId,
    schema_pin: Option<&temper_runtime::persistence::schema_deployment::SchemaExecutionPin>,
    set_name: &str,
) -> Result<String, ODataWriteError> {
    resolve_entity_type_for_pin(state, tenant, schema_pin, set_name).ok_or_else(|| {
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
    schema_pin: Option<&temper_runtime::persistence::schema_deployment::SchemaExecutionPin>,
    set_name: &str,
    agent_ctx: &AgentContext,
    request_body: Option<serde_json::Value>,
    intent: Option<String>,
) -> Result<String, ODataWriteError> {
    resolve_entity_type_for_pin(state, tenant, schema_pin, set_name).ok_or_else(|| {
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
    schema_pin: Option<&temper_runtime::persistence::schema_deployment::SchemaExecutionPin>,
) -> Result<(), ODataWriteError> {
    if schema_pin.is_some() {
        return Ok(());
    }
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
    let resource_attrs = match agent_ctx.schema_pin.as_ref() {
        Some(_) => {
            let mut attrs = resource_attrs_from_body(state, tenant, entity_type, entity_id, fields);
            attrs.insert("has_spec".into(), serde_json::Value::Bool(true));
            attrs
        }
        None => state
            .build_create_authz_resource_attrs(tenant, entity_type, entity_id, fields)
            .await
            .map_err(|error| {
                Box::new(
                    odata_error(StatusCode::INTERNAL_SERVER_ERROR, "ReadError", &error)
                        .into_response(),
                )
            })?,
    };
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

async fn ensure_entity_exists_or_404(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    set_name: &str,
    key: &str,
    schema_pin: Option<&temper_runtime::persistence::schema_deployment::SchemaExecutionPin>,
) -> Result<(), ODataWriteError> {
    let exists = match schema_pin {
        Some(pin) => {
            let persistence_id = format!(
                "{tenant}:{entity_type}:{}",
                temper_runtime::persistence::schema_deployment::scoped_journal_entity_id(key, pin)
            );
            let loaded = state
                .actor_registry
                .read()
                .map(|registry| registry.contains_key(&persistence_id))
                .unwrap_or(false);
            if loaded {
                true
            } else if let Some((journal, _)) = state.event_journal() {
                journal
                    .read_latest_events(&persistence_id, 1)
                    .await
                    .is_ok_and(|events| !events.is_empty())
            } else {
                false
            }
        }
        None => state.entity_exists(tenant, entity_type, key),
    };
    if exists {
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

async fn authorize_existing_mutation(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
    action: &str,
    security_ctx: &temper_authz::SecurityContext,
    agent_ctx: &AgentContext,
) -> Result<ExistingMutationResource, ODataWriteError> {
    let (current_state, resource_attrs) = match agent_ctx.schema_pin.as_ref() {
        Some(pin) => {
            let response = state
                .get_scoped_entity_state(tenant, entity_type, entity_id, pin.clone())
                .await
                .map_err(|error| {
                    Box::new(schema_pin_mismatch_response(&error).unwrap_or_else(|| {
                        odata_error(StatusCode::INTERNAL_SERVER_ERROR, "ReadError", &error)
                            .into_response()
                    }))
                })?;
            let mut attrs = response
                .state
                .fields
                .as_object()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .collect::<std::collections::BTreeMap<_, _>>();
            attrs.insert("id".into(), serde_json::Value::String(entity_id.into()));
            attrs.insert(
                "status".into(),
                serde_json::Value::String(response.state.status.clone()),
            );
            attrs.insert("has_spec".into(), serde_json::Value::Bool(true));
            (response, attrs)
        }
        None => {
            let snapshot = state
                .load_authz_resource_snapshot(tenant, entity_type, entity_id)
                .await
                .map_err(|error| {
                    Box::new(
                        odata_error(StatusCode::INTERNAL_SERVER_ERROR, "ReadError", &error)
                            .into_response(),
                    )
                })?;
            (snapshot.current_state, snapshot.resource_attrs)
        }
    };
    authorize_mutation(
        state,
        tenant,
        security_ctx,
        agent_ctx,
        action,
        MutationResource {
            entity_type,
            entity_id,
            attrs: &resource_attrs,
        },
    )
    .await
    .map_err(Box::new)?;
    let precondition =
        crate::entity_actor::effects::entity_authorization_precondition(&current_state.state);
    Ok(ExistingMutationResource {
        status: current_state.state.status,
        fields: current_state.state.fields,
        precondition,
    })
}

struct ExistingMutationResource {
    status: String,
    fields: serde_json::Value,
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
    agent_ctx.schema_pin = match extract_schema_pin(&headers, &state, &tenant).await {
        Ok(pin) => pin,
        Err(error) => return schema_pin_extraction_error_response(error),
    };
    if let Some(remote_parent) = remote_parent_context(&agent_ctx) {
        tracing::Span::current().set_parent(remote_parent);
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
                agent_ctx.schema_pin.as_ref(),
                &name,
                &agent_ctx,
                body_for_trajectory,
                agent_ctx.intent.clone(),
            ) {
                Ok(t) => t,
                Err(resp) => return *resp,
            };
            if let Err(resp) = check_verification_gate_or_423(
                &state,
                &tenant,
                &entity_type,
                agent_ctx.schema_pin.as_ref(),
            ) {
                return *resp;
            }

            let body_json = match parse_json_body_or_400(&body) {
                Ok(v) => v,
                Err(resp) => return *resp,
            };

            let supplied_entity_id = body_json
                .get("id")
                .or_else(|| body_json.get("Id"))
                .and_then(|v| v.as_str())
                .map(String::from);
            if agent_ctx.schema_pin.is_none()
                && let Some(entity_id) = supplied_entity_id.as_deref()
                && let Err(error) = validate_global_entity_id(entity_id)
            {
                return odata_error(StatusCode::BAD_REQUEST, "InvalidEntityId", &error)
                    .into_response();
            }
            let initial_status = match agent_ctx.schema_pin.as_ref() {
                Some(pin) => state
                    .registry
                    .read()
                    .map_err(|error| format!("registry lock poisoned: {error}"))
                    .and_then(|registry| {
                        registry
                            .get_scoped_table_at_digest(
                                &tenant,
                                &pin.scope,
                                &pin.bundle_digest,
                                &entity_type,
                            )
                            .map(|table| table.initial_state.clone())
                            .ok_or_else(|| "scoped transition table is unavailable".to_string())
                    }),
                None => state.initial_entity_status(&tenant, &entity_type),
            };
            let initial_status = match initial_status {
                Ok(status) => status,
                Err(error) => {
                    return odata_error(StatusCode::INTERNAL_SERVER_ERROR, "ReadError", &error)
                        .into_response();
                }
            };
            let (_, mut initial_fields) =
                match prepare_collection_create_fields(body_json, &entity_type, &initial_status) {
                    Ok(prepared) => prepared,
                    Err(response) => return *response,
                };
            let prepared_id = match agent_ctx.schema_pin.as_ref() {
                Some(pin) => {
                    state
                        .prepare_scoped_reference_contract_create(
                            &tenant,
                            &entity_type,
                            supplied_entity_id.as_deref(),
                            &initial_fields,
                            pin,
                        )
                        .await
                }
                None => {
                    state
                        .prepare_reference_contract_create(
                            &tenant,
                            &entity_type,
                            supplied_entity_id.as_deref(),
                            &initial_fields,
                        )
                        .await
                }
            };
            let entity_id = match prepared_id {
                Ok(Some(entity_id)) => entity_id,
                Ok(None) => {
                    let prefix = entity_type_prefix(&entity_type);
                    format!("{prefix}{}", temper_runtime::scheduler::sim_uuid())
                }
                Err(error) => {
                    let status = if error.contains("InvalidReferenceValue") {
                        StatusCode::BAD_REQUEST
                    } else {
                        StatusCode::CONFLICT
                    };
                    return odata_error(status, "ConstraintViolation", &error).into_response();
                }
            };
            if let Some(fields) = initial_fields.as_object_mut() {
                fields.insert("id".to_string(), entity_id.clone().into());
                fields.insert("Id".to_string(), entity_id.clone().into());
            }
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
            let _commons_guardrail_lock = state.acquire_commons_write_guardrail_lock(&tenant).await;

            if let Err(resp) = run_write_prechecks(
                &state,
                &tenant,
                &entity_type,
                &entity_id,
                ("Create", "create"),
                &initial_fields,
                agent_ctx.schema_pin.as_ref(),
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
                &security_ctx,
            )
            .await
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

            let application_data =
                crate::application_data::GovernedApplicationDataService::new(&state);
            let create_result = match agent_ctx.schema_pin.clone() {
                Some(schema_pin) => {
                    application_data
                        .create_scoped(
                            &tenant,
                            &entity_type,
                            &entity_id,
                            initial_fields,
                            schema_pin,
                        )
                        .await
                }
                None => {
                    application_data
                        .create(&tenant, &entity_type, &entity_id, initial_fields)
                        .await
                }
            };
            match create_result {
                Ok(response) => {
                    if !response.success {
                        let error = response.error.as_deref().unwrap_or("Update failed");
                        return reference_contract_response(error).unwrap_or_else(|| {
                            odata_error(StatusCode::CONFLICT, "UpdateFailed", error).into_response()
                        });
                    }
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
                Err(e) => schema_pin_mismatch_response(&e).unwrap_or_else(|| {
                    odata_error(StatusCode::INTERNAL_SERVER_ERROR, "CreateError", &e)
                        .into_response()
                }),
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
                agent_ctx.schema_pin.as_ref(),
                &set_name,
                &agent_ctx,
                Some(body_json.clone()),
                agent_ctx.intent.clone(),
            ) {
                Ok(t) => t,
                Err(resp) => return *resp,
            };
            agent_ctx.schema_pin = match resolve_scope_only_entity_pin(
                &headers,
                &state,
                &tenant,
                &entity_type,
                &key_str,
                agent_ctx.schema_pin.take(),
            )
            .await
            {
                Ok(pin) => pin,
                Err(error) => return schema_pin_extraction_error_response(error),
            };
            if agent_ctx.schema_pin.is_none()
                && let Err(error) = validate_global_entity_id(&key_str)
            {
                return odata_error(StatusCode::BAD_REQUEST, "InvalidEntityId", &error)
                    .into_response();
            }

            if let Err(resp) = check_verification_gate_or_423(
                &state,
                &tenant,
                &entity_type,
                agent_ctx.schema_pin.as_ref(),
            ) {
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
                let authz_body = serde_json::json!({
                    "entity_id": key_str,
                    "status": actor_state.status,
                    "fields": actor_state.fields,
                });
                let attrs =
                    resource_attrs_from_body(&state, &tenant, &entity_type, &key_str, &authz_body);
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
                if let Err(error) = validate_bound_action_input(
                    &state,
                    &tenant,
                    agent_ctx.schema_pin.as_ref(),
                    &entity_type,
                    action_name,
                    &body_json,
                ) {
                    return odata_error(StatusCode::BAD_REQUEST, error.code, &error.message)
                        .into_response();
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
    agent_ctx.schema_pin = match extract_schema_pin(&headers, &state, &tenant).await {
        Ok(pin) => pin,
        Err(error) => return schema_pin_extraction_error_response(error),
    };

    match odata_path {
        ODataPath::Entity(set_name, key) => {
            let entity_type = match resolve_entity_type_or_404(
                &state,
                &tenant,
                agent_ctx.schema_pin.as_ref(),
                &set_name,
            ) {
                Ok(t) => t,
                Err(resp) => return *resp,
            };
            let key_str = extract_key(&key);
            agent_ctx.schema_pin = match resolve_scope_only_entity_pin(
                &headers,
                &state,
                &tenant,
                &entity_type,
                &key_str,
                agent_ctx.schema_pin.take(),
            )
            .await
            {
                Ok(pin) => pin,
                Err(error) => return schema_pin_extraction_error_response(error),
            };

            if let Err(resp) = check_verification_gate_or_423(
                &state,
                &tenant,
                &entity_type,
                agent_ctx.schema_pin.as_ref(),
            ) {
                return *resp;
            }
            if let Err(resp) = ensure_entity_exists_or_404(
                &state,
                &tenant,
                &entity_type,
                &set_name,
                &key_str,
                agent_ctx.schema_pin.as_ref(),
            )
            .await
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
            let mut prospective_fields = existing.fields;
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
                    status: &existing.status,
                    fields: &prospective_fields,
                    security_ctx: &security_ctx,
                    agent_ctx: &agent_ctx,
                },
            )
            .await
            {
                return *response;
            }

            let _commons_guardrail_lock = state.acquire_commons_write_guardrail_lock(&tenant).await;

            if let Err(resp) = run_write_prechecks(
                &state,
                &tenant,
                &entity_type,
                &key_str,
                ("Patch", "patch"),
                &prospective_fields,
                agent_ctx.schema_pin.as_ref(),
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
                &security_ctx,
            )
            .await
            {
                return resp;
            }

            let update_result = match agent_ctx.schema_pin.clone() {
                Some(pin) => {
                    state
                        .update_scoped_entity_fields_if_current(
                            &tenant,
                            &entity_type,
                            &key_str,
                            body_json,
                            false,
                            pin,
                            existing.precondition,
                        )
                        .await
                }
                None => {
                    state
                        .update_tenant_entity_fields_if_current(
                            &tenant,
                            &entity_type,
                            &key_str,
                            body_json,
                            false,
                            existing.precondition,
                        )
                        .await
                }
            };
            match update_result {
                Ok(response) if response.success => {
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
                Ok(response) => {
                    let error = response.error.as_deref().unwrap_or(
                        "entity changed after authorization; retry against current state",
                    );
                    reference_contract_response(error).unwrap_or_else(|| {
                        odata_error(StatusCode::CONFLICT, "ConcurrentModification", error)
                            .into_response()
                    })
                }
                Err(e) => reference_contract_response(&e).unwrap_or_else(|| {
                    odata_error(StatusCode::INTERNAL_SERVER_ERROR, "UpdateError", &e)
                        .into_response()
                }),
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
    agent_ctx.schema_pin = match extract_schema_pin(&headers, &state, &tenant).await {
        Ok(pin) => pin,
        Err(error) => return schema_pin_extraction_error_response(error),
    };

    match odata_path {
        ODataPath::Entity(set_name, key) => {
            let entity_type = match resolve_entity_type_or_404(
                &state,
                &tenant,
                agent_ctx.schema_pin.as_ref(),
                &set_name,
            ) {
                Ok(t) => t,
                Err(resp) => return *resp,
            };
            let key_str = extract_key(&key);
            agent_ctx.schema_pin = match resolve_scope_only_entity_pin(
                &headers,
                &state,
                &tenant,
                &entity_type,
                &key_str,
                agent_ctx.schema_pin.take(),
            )
            .await
            {
                Ok(pin) => pin,
                Err(error) => return schema_pin_extraction_error_response(error),
            };

            if let Err(resp) = check_verification_gate_or_423(
                &state,
                &tenant,
                &entity_type,
                agent_ctx.schema_pin.as_ref(),
            ) {
                return *resp;
            }
            if let Err(resp) = ensure_entity_exists_or_404(
                &state,
                &tenant,
                &entity_type,
                &set_name,
                &key_str,
                agent_ctx.schema_pin.as_ref(),
            )
            .await
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

            if let Err(response) = authorize_prospective_mutation(
                &state,
                ProspectiveMutationAuthorization {
                    tenant: &tenant,
                    entity_type: &entity_type,
                    entity_id: &key_str,
                    status: &existing.status,
                    fields: &body_json,
                    security_ctx: &security_ctx,
                    agent_ctx: &agent_ctx,
                },
            )
            .await
            {
                return *response;
            }

            let _commons_guardrail_lock = state.acquire_commons_write_guardrail_lock(&tenant).await;

            if let Err(resp) = run_write_prechecks(
                &state,
                &tenant,
                &entity_type,
                &key_str,
                ("Put", "put"),
                &body_json,
                agent_ctx.schema_pin.as_ref(),
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
                &security_ctx,
            )
            .await
            {
                return resp;
            }

            let update_result = match agent_ctx.schema_pin.clone() {
                Some(pin) => {
                    state
                        .update_scoped_entity_fields_if_current(
                            &tenant,
                            &entity_type,
                            &key_str,
                            body_json,
                            true,
                            pin,
                            existing.precondition,
                        )
                        .await
                }
                None => {
                    state
                        .update_tenant_entity_fields_if_current(
                            &tenant,
                            &entity_type,
                            &key_str,
                            body_json,
                            true,
                            existing.precondition,
                        )
                        .await
                }
            };
            match update_result {
                Ok(response) if response.success => {
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
                Ok(response) => {
                    let error = response.error.as_deref().unwrap_or(
                        "entity changed after authorization; retry against current state",
                    );
                    reference_contract_response(error).unwrap_or_else(|| {
                        odata_error(StatusCode::CONFLICT, "ConcurrentModification", error)
                            .into_response()
                    })
                }
                Err(e) => reference_contract_response(&e).unwrap_or_else(|| {
                    odata_error(StatusCode::INTERNAL_SERVER_ERROR, "UpdateError", &e)
                        .into_response()
                }),
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
    agent_ctx.schema_pin = match extract_schema_pin(&headers, &state, &tenant).await {
        Ok(pin) => pin,
        Err(error) => return schema_pin_extraction_error_response(error),
    };

    match odata_path {
        ODataPath::Entity(set_name, key) => {
            let entity_type = match resolve_entity_type_or_404(
                &state,
                &tenant,
                agent_ctx.schema_pin.as_ref(),
                &set_name,
            ) {
                Ok(t) => t,
                Err(resp) => return *resp,
            };
            let key_str = extract_key(&key);
            agent_ctx.schema_pin = match resolve_scope_only_entity_pin(
                &headers,
                &state,
                &tenant,
                &entity_type,
                &key_str,
                agent_ctx.schema_pin.take(),
            )
            .await
            {
                Ok(pin) => pin,
                Err(error) => return schema_pin_extraction_error_response(error),
            };

            if let Err(resp) = check_verification_gate_or_423(
                &state,
                &tenant,
                &entity_type,
                agent_ctx.schema_pin.as_ref(),
            ) {
                return *resp;
            }
            if let Err(resp) = ensure_entity_exists_or_404(
                &state,
                &tenant,
                &entity_type,
                &set_name,
                &key_str,
                agent_ctx.schema_pin.as_ref(),
            )
            .await
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
            if let Err(v) = pre_delete_relation_checks(
                &state,
                &tenant,
                &entity_type,
                &key_str,
                "delete",
                agent_ctx.schema_pin.as_ref(),
            )
            .await
            {
                return constraint_violation_response(v);
            }
            if let Err(resp) = run_write_prechecks(
                &state,
                &tenant,
                &entity_type,
                &key_str,
                ("Delete", "delete"),
                &existing.fields,
                agent_ctx.schema_pin.as_ref(),
            )
            .await
            {
                return resp;
            }

            if let Err(resp) = enforce_commons_account_verified_for_write(
                &state,
                &tenant,
                &entity_type,
                &existing.fields,
            )
            .await
            {
                return *resp;
            }

            if let Err(resp) = enforce_commons_write_rate_limit(
                &state,
                &tenant,
                &entity_type,
                owner_id_from_fields(&existing.fields),
                &security_ctx,
            )
            .await
            {
                return resp;
            }

            let delete_result = match agent_ctx.schema_pin.clone() {
                Some(pin) => {
                    state
                        .delete_scoped_entity_if_current(
                            &tenant,
                            &entity_type,
                            &key_str,
                            pin,
                            existing.precondition,
                        )
                        .await
                }
                None => {
                    state
                        .delete_tenant_entity_if_current(
                            &tenant,
                            &entity_type,
                            &key_str,
                            existing.precondition,
                        )
                        .await
                }
            };
            match delete_result {
                Ok(response) if response.success => {
                    if entity_type == "RateLimit" {
                        state.clear_commons_rate_limit_cache();
                    }
                    state.clear_commons_storage_projection_cache_for_entity(&entity_type);
                    (StatusCode::NO_CONTENT, "").into_response()
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
