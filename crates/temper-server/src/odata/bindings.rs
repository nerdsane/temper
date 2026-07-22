//! Bound action helpers for OData write handlers.

use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use opentelemetry::KeyValue as OtelKeyValue;
use opentelemetry::trace::{Span, Status, Tracer};
use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;
use tracing_opentelemetry::OpenTelemetrySpanExt;

use temper_authz::{PrincipalKind, SecurityContext};

use super::account_verification::enforce_commons_account_verified_for_action;
use super::common::run_write_prechecks;
use super::rate_limit::{enforce_commons_write_rate_limit, owner_id_from_action};
use super::response::annotate_entity;
use crate::authz::{DenialInput, record_authz_denial, security_context_from_headers};
use crate::blobs::hydrate_blob_refs_for_tenant;
use crate::entity_actor::{
    EntityRecoveryContext, EntityResponse, recover_entity_state_from_stable_sources,
};
use crate::idempotency::{BoundActionClaim, BoundActionReplayLookup};
use crate::identity::ResolvedIdentity;
use crate::request_context::AgentContext;
use crate::response::{ODataResponse, odata_error};
use crate::state::{
    BoundActionHookContext, DispatchError, DispatchExtOptions, ServerState, TenantGenerationLease,
};

mod execute;
mod hook_receipt;

use execute::{
    BoundActionExecution, execute_bound_action, merge_bound_action_hook_output,
    post_action_error_status, run_or_recover_bound_action_hook,
};

fn idempotency_actor_key(tenant: &TenantId, entity_type: &str, entity_id: &str) -> String {
    format!("{tenant}:{entity_type}:{entity_id}")
}

fn canonical_json(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(fields) => {
            let sorted = fields
                .iter()
                .map(|(key, value)| (key.clone(), canonical_json(value)))
                .collect::<std::collections::BTreeMap<_, _>>();
            serde_json::Value::Object(sorted.into_iter().collect())
        }
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.iter().map(canonical_json).collect())
        }
        scalar => scalar.clone(),
    }
}

fn bound_action_request_fingerprint(
    action: &str,
    params: &serde_json::Value,
    security_ctx: &SecurityContext,
) -> String {
    let principal = canonical_json(
        &serde_json::to_value(&security_ctx.principal).unwrap_or(serde_json::Value::Null),
    );
    let params = canonical_json(params);
    let principal = serde_json::to_vec(&principal).unwrap_or_default();
    let params = serde_json::to_vec(&params).unwrap_or_default();
    ServerState::spec_publication_intent(
        "bound-action-replay",
        [
            ("action", action.as_bytes()),
            ("params", params.as_slice()),
            ("principal", principal.as_slice()),
        ],
    )
}

fn bound_action_operation_fingerprint(action: &str, params: &serde_json::Value) -> String {
    let params = canonical_json(params);
    let params = serde_json::to_vec(&params).unwrap_or_default();
    ServerState::spec_publication_intent(
        "bound-action-operation",
        [("action", action.as_bytes()), ("params", params.as_slice())],
    )
}

fn bound_action_durable_idempotency_key(
    raw_key: &str,
    operation_fingerprint: &str,
    request_fingerprint: &str,
) -> (String, String, String) {
    let raw_key_digest = ServerState::spec_publication_intent(
        "bound-action-idempotency-key",
        [("key", raw_key.as_bytes())],
    );
    let raw_prefix = format!("temper.bound-action.v2:{raw_key_digest}:");
    let operation_prefix = format!("{raw_prefix}{operation_fingerprint}:");
    let durable_key = format!("{operation_prefix}{request_fingerprint}");
    (raw_prefix, operation_prefix, durable_key)
}

async fn recover_durable_bound_action_response(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
) -> Result<EntityResponse, String> {
    let (store, backend) = state.event_journal().ok_or_else(|| {
        "publication replay has no durable event journal; its pinned in-memory proof is required"
            .to_string()
    })?;
    let live_table = state
        .registry
        .read()
        .map_err(|error| format!("registry lock poisoned: {error}"))?
        .get_table_live(tenant, entity_type);
    let table = if let Some(live_table) = live_table {
        live_table
            .read()
            .map_err(|error| format!("transition table lock poisoned: {error}"))?
            .clone()
    } else {
        state
            .transition_tables
            .get(entity_type)
            .map(|table| (**table).clone())
            .ok_or_else(|| {
                format!("No transition table for tenant '{tenant}', entity type '{entity_type}'")
            })?
    };
    let initial_fields = serde_json::json!({});
    let blob_store = state.blob_store_for_tenant(tenant).ok();
    let recovered = recover_entity_state_from_stable_sources(EntityRecoveryContext {
        tenant: tenant.as_str(),
        entity_type,
        entity_id,
        table: &table,
        store: &store,
        backend,
        initial_fields: &initial_fields,
        blob_store: blob_store.as_ref(),
    })
    .await
    .map_err(|error| format!("failed to recover publication replay proof: {error}"))?;
    let recovered = recovered.state.ok_or_else(|| {
        format!("no durable state exists for publication actor {tenant}:{entity_type}:{entity_id}")
    })?;
    Ok(EntityResponse {
        success: true,
        state: recovered,
        error: None,
        custom_effects: Vec::new(),
        scheduled_actions: Vec::new(),
        spawn_requests: Vec::new(),
        spec_governed: true,
    })
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn dispatch_bound_action(
    state: &ServerState,
    tenant: &TenantId,
    set_name: &str,
    entity_type: &str,
    key_str: &str,
    action: &str,
    body_json: serde_json::Value,
    agent_ctx: &AgentContext,
    headers: &HeaderMap,
    await_integration: bool,
    idempotency_key: Option<String>,
    resolved_identity: Option<&ResolvedIdentity>,
    generation_lease: Option<&TenantGenerationLease>,
) -> axum::response::Response {
    let http_start = sim_now();
    let tracer = opentelemetry::global::tracer("temper");
    let http_start_time: std::time::SystemTime = http_start.into();
    let span_name = format!("HTTP POST {set_name}.{action}");
    let mut http_span = tracer
        .span_builder(span_name)
        .with_start_time(http_start_time)
        .with_attributes(vec![
            OtelKeyValue::new("http.method", "POST"),
            OtelKeyValue::new("odata.entity_set", set_name.to_string()),
            OtelKeyValue::new("odata.entity_id", key_str.to_string()),
            OtelKeyValue::new("odata.action", action.to_string()),
            OtelKeyValue::new("tenant", tenant.as_str().to_string()),
        ])
        .start_with_context(&tracer, &tracing::Span::current().context());

    if let Some(ref aid) = agent_ctx.agent_id {
        http_span.set_attribute(OtelKeyValue::new("agent.id", aid.clone()));
    }
    if let Some(ref sid) = agent_ctx.session_id {
        http_span.set_attribute(OtelKeyValue::new("session.id", sid.clone()));
    }
    if let Some(ref intent) = agent_ctx.intent {
        http_span.set_attribute(OtelKeyValue::new("intent", intent.clone()));
    }
    for (key, value) in &agent_ctx.observation_metadata {
        http_span.set_attribute(OtelKeyValue::new(
            format!("temper.observation.{key}"),
            value.clone(),
        ));
    }

    // Build SecurityContext — credential-resolved identity (ADR-0033) or
    // operator identity for global API key access.
    let security_ctx = if let Some(identity) = resolved_identity {
        http_span.set_attribute(OtelKeyValue::new(
            "agent.id",
            identity.agent_instance_id.clone(),
        ));
        http_span.set_attribute(OtelKeyValue::new(
            "agent.type",
            identity.agent_type_name.clone(),
        ));
        SecurityContext::from_resolved_identity(
            &identity.agent_instance_id,
            &identity.agent_type_name,
            agent_ctx.session_id.as_deref(),
        )
    } else {
        // No credential resolved — operator/admin access via global API key.
        // Build SecurityContext from X-Temper-Principal-Kind header (admin/system)
        // without trusting self-declared identity fields.
        security_context_from_headers(
            headers,
            None, // No self-declared agent_id
            agent_ctx.session_id.as_deref(),
            None, // No self-declared agent_type
        )
    };
    let mut dispatch_agent_ctx = agent_ctx.clone();
    dispatch_agent_ctx.security_ctx = Some(security_ctx.clone());
    let operation_fingerprint = bound_action_operation_fingerprint(action, &body_json);
    let request_fingerprint = bound_action_request_fingerprint(action, &body_json, &security_ctx);
    let idempotency_key = idempotency_key.filter(|key| !key.trim().is_empty());
    let publication_capable = state
        .bound_action_hook
        .as_ref()
        .is_some_and(|hook| hook.requires_generation_handoff(entity_type, action));
    if publication_capable
        && idempotency_key
            .as_deref()
            .is_none_or(|key| key.trim().is_empty())
    {
        return odata_error(
            StatusCode::BAD_REQUEST,
            "IdempotencyKeyRequired",
            "Publication-capable actions require a non-empty Idempotency-Key",
        )
        .into_response();
    }

    // Default-deny: reject actions on entity types with no registered spec.
    let is_governed = match state.is_entity_type_governed(tenant, entity_type) {
        Ok(value) => value,
        Err(e) => {
            http_span.set_status(Status::error(e.clone()));
            http_span.set_attribute(OtelKeyValue::new("http.status_code", 500i64));
            let end_time: std::time::SystemTime = sim_now().into();
            http_span.end_with_timestamp(end_time);
            return odata_error(StatusCode::INTERNAL_SERVER_ERROR, "RegistryError", &e)
                .into_response();
        }
    };

    if !is_governed {
        http_span.set_status(Status::error("EntityTypeNotGoverned"));
        http_span.set_attribute(OtelKeyValue::new("http.status_code", 404i64));
        let end_time: std::time::SystemTime = sim_now().into();
        http_span.end_with_timestamp(end_time);
        return odata_error(
            StatusCode::NOT_FOUND,
            "EntityTypeNotGoverned",
            &format!(
                "Entity type '{entity_type}' has no registered spec — actions are denied by default"
            ),
        )
        .into_response();
    }

    let actor_key = idempotency_actor_key(tenant, entity_type, key_str);
    if state.spec_publication_gated(tenant) {
        if !publication_capable {
            return odata_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "SpecPublicationInProgress",
                "Tenant runtime generation is being published; retry the request",
            )
            .into_response();
        }
        let replay =
            idempotency_key
                .as_deref()
                .map_or(BoundActionReplayLookup::Miss, |idempotency_key| {
                    state.idempotency_cache.lookup_bound_action_replay(
                        &actor_key,
                        idempotency_key,
                        &request_fingerprint,
                    )
                });
        let (cached, original_params, hook_completed, hook_output) = match replay {
            BoundActionReplayLookup::Match {
                response,
                params,
                hook_completed,
                hook_output,
            } => (*response, params, hook_completed, hook_output),
            BoundActionReplayLookup::Pending => {
                return odata_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "IdempotencyInProgress",
                    "An identical post-action hook is still in progress",
                )
                .into_response();
            }
            BoundActionReplayLookup::Conflict
                if !matches!(
                    security_ctx.principal.kind,
                    PrincipalKind::Admin | PrincipalKind::System
                ) =>
            {
                return odata_error(
                    StatusCode::CONFLICT,
                    "IdempotencyConflict",
                    "Idempotency-Key was already used by a different action request or principal",
                )
                .into_response();
            }
            BoundActionReplayLookup::Conflict | BoundActionReplayLookup::Miss => {
                let Some(idempotency_key) = idempotency_key.as_deref() else {
                    return odata_error(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "SpecPublicationInProgress",
                        "Tenant runtime generation is being published; retry the request",
                    )
                    .into_response();
                };
                let (durable_prefix, operation_prefix, durable_key) =
                    bound_action_durable_idempotency_key(
                        idempotency_key,
                        &operation_fingerprint,
                        &request_fingerprint,
                    );
                let durable = match recover_durable_bound_action_response(
                    state,
                    tenant,
                    entity_type,
                    key_str,
                )
                .await
                {
                    Ok(durable) => durable,
                    Err(_) => {
                        return odata_error(
                            StatusCode::SERVICE_UNAVAILABLE,
                            "SpecPublicationInProgress",
                            "Tenant runtime generation is being published; retry the request",
                        )
                        .into_response();
                    }
                };
                let processed = &durable.state.processed_idempotency_keys;
                let durable_claims = processed
                    .keys()
                    .filter(|stored| stored.starts_with(&durable_prefix))
                    .collect::<Vec<_>>();
                let legacy_conflict = processed.contains_key(idempotency_key);
                let exact_match = processed.contains_key(&durable_key);
                let privileged_operation_match = matches!(
                    security_ctx.principal.kind,
                    PrincipalKind::Admin | PrincipalKind::System
                ) && durable_claims.len() == 1
                    && durable_claims[0].starts_with(&operation_prefix);
                if legacy_conflict
                    || durable_claims.len() > 1
                    || (!durable_claims.is_empty() && !exact_match && !privileged_operation_match)
                {
                    return odata_error(
                        StatusCode::CONFLICT,
                        "IdempotencyConflict",
                        "Idempotency-Key was already durably used by a different or unproved action request",
                    )
                    .into_response();
                }
                if !exact_match && !privileged_operation_match {
                    return odata_error(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "SpecPublicationInProgress",
                        "Tenant runtime generation is being published; retry the exact publication request",
                    )
                    .into_response();
                }
                if !state.idempotency_cache.put_bound_action_effects_applied(
                    &actor_key,
                    idempotency_key,
                    durable.clone(),
                    request_fingerprint.clone(),
                    body_json.clone(),
                ) {
                    return odata_error(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "IdempotencyCapacityExceeded",
                        "The durable replay could not reserve bounded recovery state; retry later",
                    )
                    .into_response();
                }
                (durable, body_json.clone(), false, None)
            }
        };
        let mut state_json = serde_json::to_value(&cached.state).unwrap_or_default();
        if hook_completed {
            merge_bound_action_hook_output(&mut state_json, hook_output.as_ref());
        } else {
            let idempotency_key = idempotency_key
                .as_deref()
                .expect("publication-capable replay requires idempotency key");
            if let Err(error) = run_or_recover_bound_action_hook(
                state,
                tenant,
                entity_type,
                key_str,
                action,
                &original_params,
                &mut state_json,
                generation_lease,
                &actor_key,
                idempotency_key,
                &operation_fingerprint,
                &request_fingerprint,
            )
            .await
            {
                let status = post_action_error_status(&error);
                return odata_error(status, "PostActionHookFailed", &error).into_response();
            }
        }
        if let Some(idempotency_key) = idempotency_key.as_deref() {
            state.idempotency_cache.unpin_bound_action_replay(
                &actor_key,
                idempotency_key,
                &request_fingerprint,
            );
        }
        let body = annotate_entity(state_json, format!("$metadata#{set_name}/$entity"), None);
        http_span.set_attribute(OtelKeyValue::new("idempotency.hit", true));
        http_span.set_status(Status::Ok);
        http_span.set_attribute(OtelKeyValue::new("http.status_code", 200i64));
        let end_time: std::time::SystemTime = sim_now().into();
        http_span.end_with_timestamp(end_time);
        return ODataResponse {
            status: StatusCode::OK,
            body,
        }
        .into_response();
    }

    execute_bound_action(
        BoundActionExecution {
            state,
            tenant,
            set_name,
            entity_type,
            key_str,
            action,
            body_json,
            agent_ctx,
            headers,
            await_integration,
            idempotency_key,
            resolved_identity,
            generation_lease,
            security_ctx,
            dispatch_agent_ctx,
            operation_fingerprint,
            request_fingerprint,
            actor_key,
        },
        http_span,
    )
    .await
}

#[cfg(test)]
mod tests;
