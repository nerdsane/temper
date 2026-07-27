//! Bound action helpers for OData write handlers.

use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use opentelemetry::KeyValue as OtelKeyValue;
use opentelemetry::trace::{Span, Status, Tracer};
use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;
use tracing_opentelemetry::OpenTelemetrySpanExt;

use temper_authz::SecurityContext;

use super::account_verification::enforce_commons_account_verified_for_action;
use super::common::run_write_prechecks;
use super::rate_limit::{enforce_commons_write_rate_limit, owner_id_from_action};
use super::response::annotate_entity;
use crate::authz::{DenialInput, record_authz_denial, security_context_from_headers};
use crate::blobs::hydrate_blob_refs_for_tenant;
use crate::identity::ResolvedIdentity;
use crate::request_context::AgentContext;
use crate::response::{ODataResponse, odata_error, service_unavailable_response};
use crate::state::{BoundActionHookContext, DispatchError, DispatchExtOptions, ServerState};

fn idempotency_actor_key(tenant: &TenantId, entity_type: &str, entity_id: &str) -> String {
    format!("{tenant}:{entity_type}:{entity_id}")
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

    let authz_snapshot = match state
        .load_authz_resource_snapshot(tenant, entity_type, key_str)
        .await
    {
        Ok(v) => v,
        Err(e) => {
            http_span.set_status(Status::error("ResourceStateUnavailable"));
            http_span.set_attribute(OtelKeyValue::new("http.status_code", 503i64));
            let end_time: std::time::SystemTime = sim_now().into();
            http_span.end_with_timestamp(end_time);
            return service_unavailable_response(
                "ResourceStateUnavailable",
                "Entity state is temporarily unavailable",
                "bound_action_authorization_snapshot",
                e,
            );
        }
    };
    let current_state = authz_snapshot.current_state;
    let resource_attrs = authz_snapshot.resource_attrs;

    if let Err(resp) = enforce_commons_account_verified_for_action(
        state,
        tenant,
        entity_type,
        &current_state.state.fields,
        &body_json,
    )
    .await
    {
        http_span.set_status(Status::error("AccountVerificationRequired"));
        http_span.set_attribute(OtelKeyValue::new("http.status_code", 403i64));
        let end_time: std::time::SystemTime = sim_now().into();
        http_span.end_with_timestamp(end_time);
        return *resp;
    }

    if let Err(resp) = enforce_commons_write_rate_limit(
        state,
        tenant,
        entity_type,
        owner_id_from_action(&current_state.state.fields, &body_json),
        headers,
        agent_ctx,
        resolved_identity,
    )
    .await
    {
        http_span.set_status(Status::error("RateLimitExceeded"));
        http_span.set_attribute(OtelKeyValue::new("http.status_code", 429i64));
        let end_time: std::time::SystemTime = sim_now().into();
        http_span.end_with_timestamp(end_time);
        return resp;
    }

    let authz_result = state.authorize_with_context(
        &security_ctx,
        action,
        entity_type,
        &resource_attrs,
        tenant.as_str(),
    );
    if let Err(denial) = authz_result {
        let reason = denial.to_string();
        let pd = record_authz_denial(
            state,
            DenialInput {
                tenant: tenant.as_str(),
                security_ctx: &security_ctx,
                agent_id_override: agent_ctx.agent_id.as_deref(),
                action,
                resource_type: entity_type,
                resource_id: key_str,
                resource_attrs: serde_json::to_value(&resource_attrs).unwrap_or_default(),
                reason: &reason,
                module_name: None,
                from_status: Some(current_state.state.status.clone()),
            },
        )
        .await;

        http_span.set_status(Status::error(reason.clone()));
        let end_time: std::time::SystemTime = sim_now().into();
        http_span.end_with_timestamp(end_time);
        let reason_with_id = format!("{reason} (decision: {})", pd.id);
        return odata_error(
            StatusCode::FORBIDDEN,
            "AuthorizationDenied",
            &reason_with_id,
        )
        .into_response();
    }

    let current_fields = current_state.state.fields.clone();
    if let Err(resp) = run_write_prechecks(
        state,
        tenant,
        entity_type,
        key_str,
        action,
        "bound_action",
        &current_fields,
    )
    .await
    {
        http_span.set_status(Status::error("ConstraintViolation"));
        http_span.set_attribute(OtelKeyValue::new("http.status_code", 409i64));
        let end_time: std::time::SystemTime = sim_now().into();
        http_span.end_with_timestamp(end_time);
        return resp;
    }

    // Idempotency cache check
    let actor_key = idempotency_actor_key(tenant, entity_type, key_str);
    if let Some(ref idem_key) = idempotency_key
        && let Some(cached) = state
            .idempotency_cache
            .get_after_effects_applied(&actor_key, idem_key)
    {
        let body = annotate_entity(
            serde_json::to_value(&cached.state).unwrap_or_default(),
            format!("$metadata#{set_name}/$entity"),
            None,
        );
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

    let result = state
        .dispatch_tenant_action_ext_typed(
            tenant,
            entity_type,
            key_str,
            action,
            body_json.clone(),
            DispatchExtOptions {
                agent_ctx: &dispatch_agent_ctx,
                await_integration,
                await_reactions: true,
            },
        )
        .await;

    let http_end: std::time::SystemTime = sim_now().into();
    let response = match result {
        Ok(response) => {
            if response.success {
                // Cache for idempotency
                if let Some(ref idem_key) = idempotency_key {
                    state.idempotency_cache.put_effects_applied(
                        &actor_key,
                        idem_key,
                        response.clone(),
                    );
                }

                http_span.set_status(Status::Ok);
                http_span.set_attribute(OtelKeyValue::new("http.status_code", 200i64));

                let mut state_json = serde_json::to_value(&response.state).unwrap_or_default();
                if let Some(hook) = state.bound_action_hook.as_ref() {
                    match hook
                        .after_bound_action(BoundActionHookContext {
                            state,
                            tenant,
                            entity_type,
                            entity_id: key_str,
                            action,
                            params: &body_json,
                            state_json: &state_json,
                        })
                        .await
                    {
                        Ok(Some(hook_json)) => {
                            if let (Some(dst), Some(src)) =
                                (state_json.as_object_mut(), hook_json.as_object())
                            {
                                dst.insert(
                                    "postAction".to_string(),
                                    serde_json::Value::Object(src.clone()),
                                );
                            }
                        }
                        Ok(None) => {}
                        Err(error) => {
                            http_span.set_status(Status::error(error.clone()));
                            http_span.set_attribute(OtelKeyValue::new("http.status_code", 500i64));
                            return odata_error(
                                StatusCode::INTERNAL_SERVER_ERROR,
                                "PostActionHookFailed",
                                &error,
                            )
                            .into_response();
                        }
                    }
                }
                hydrate_blob_refs_for_tenant(state, tenant, &mut state_json).await;
                let body =
                    annotate_entity(state_json, format!("$metadata#{set_name}/$entity"), None);
                ODataResponse {
                    status: StatusCode::OK,
                    body,
                }
                .into_response()
            } else {
                http_span.set_status(Status::error(response.error.clone().unwrap_or_default()));
                http_span.set_attribute(OtelKeyValue::new("http.status_code", 409i64));
                odata_error(
                    StatusCode::CONFLICT,
                    "ActionFailed",
                    &response.error.unwrap_or_else(|| "Action failed".into()),
                )
                .into_response()
            }
        }
        Err(DispatchError::Ungoverned(entity)) => {
            let reason = format!(
                "Entity type '{entity}' has no registered spec — actions are denied by default"
            );
            http_span.set_status(Status::error("EntityTypeNotGoverned"));
            http_span.set_attribute(OtelKeyValue::new("http.status_code", 404i64));
            odata_error(StatusCode::NOT_FOUND, "EntityTypeNotGoverned", &reason).into_response()
        }
        Err(DispatchError::AuthzDenied(reason)) => {
            http_span.set_status(Status::error(reason.clone()));
            http_span.set_attribute(OtelKeyValue::new("http.status_code", 403i64));
            odata_error(StatusCode::FORBIDDEN, "AuthorizationDenied", &reason).into_response()
        }
        Err(DispatchError::QuotaExceeded(reason)) => {
            http_span.set_status(Status::error(reason.clone()));
            http_span.set_attribute(OtelKeyValue::new("http.status_code", 413i64));
            odata_error(StatusCode::PAYLOAD_TOO_LARGE, "StorageCapExceeded", &reason)
                .into_response()
        }
        Err(DispatchError::Conflict(reason)) => {
            http_span.set_status(Status::error(reason.clone()));
            http_span.set_attribute(OtelKeyValue::new("http.status_code", 409i64));
            odata_error(StatusCode::CONFLICT, "Conflict", &reason).into_response()
        }
        // ADR-0048: transient exhaustion → 503 Retry-After so clients and
        // proxies back off instead of paging someone.
        Err(e @ DispatchError::Transient { .. }) => {
            let reason = e.to_string();
            http_span.set_status(Status::error(reason.clone()));
            http_span.set_attribute(OtelKeyValue::new("http.status_code", 503i64));
            let mut resp = odata_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "DispatchTransient",
                &reason,
            )
            .into_response();
            // Retry-After is per-RFC seconds; 1s is a conservative default
            // until admission control (ADR-0051) can supply a tuned value.
            resp.headers_mut().insert(
                axum::http::header::RETRY_AFTER,
                axum::http::HeaderValue::from_static("1"),
            );
            resp
        }
        // ADR-0051: admission control declined; caller should back off.
        Err(DispatchError::Deferred { retry_after_ms }) => {
            let seconds = retry_after_ms.div_ceil(1000).max(1);
            let reason = format!("dispatch deferred: retry after {retry_after_ms}ms");
            http_span.set_status(Status::error("DispatchDeferred"));
            http_span.set_attribute(OtelKeyValue::new("http.status_code", 503i64));
            let mut resp =
                odata_error(StatusCode::SERVICE_UNAVAILABLE, "DispatchDeferred", &reason)
                    .into_response();
            let value = axum::http::HeaderValue::from_str(&seconds.to_string())
                .unwrap_or_else(|_| axum::http::HeaderValue::from_static("1"));
            resp.headers_mut()
                .insert(axum::http::header::RETRY_AFTER, value);
            resp
        }
        Err(e) => {
            let reason = e.to_string();
            http_span.set_status(Status::error(reason.clone()));
            http_span.set_attribute(OtelKeyValue::new("http.status_code", 503i64));
            service_unavailable_response(
                "DispatchUnavailable",
                "Action processing is temporarily unavailable",
                "bound_action_dispatch",
                reason,
            )
        }
    };

    http_span.end_with_timestamp(http_end);
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idempotency_actor_key_matches_actor_persistence_id_shape() {
        let tenant = TenantId::new("acme");

        assert_eq!(
            idempotency_actor_key(&tenant, "WorkCycle", "wc-1"),
            "acme:WorkCycle:wc-1"
        );
    }
}
