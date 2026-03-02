//! Bound action helpers for OData write handlers.

use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use opentelemetry::KeyValue as OtelKeyValue;
use opentelemetry::trace::{Span, Status, Tracer};
use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;

use super::common::constraint_violation_response;
use super::response::annotate_entity;
use crate::authz_helpers::{record_authz_denial, security_context_from_headers};
use crate::constraint_engine::{post_write_invariant_checks, pre_upsert_relation_checks};
use crate::dispatch::AgentContext;
use crate::response::{ODataResponse, odata_error};
use crate::state::{DispatchExtOptions, ServerState};

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
        .start(&tracer);

    if let Some(ref aid) = agent_ctx.agent_id {
        http_span.set_attribute(OtelKeyValue::new("agent.id", aid.clone()));
    }
    if let Some(ref sid) = agent_ctx.session_id {
        http_span.set_attribute(OtelKeyValue::new("session.id", sid.clone()));
    }

    // Build SecurityContext from X-Temper-* headers, enriched with agent identity.
    let security_ctx = security_context_from_headers(
        headers,
        agent_ctx.agent_id.as_deref(),
        agent_ctx.session_id.as_deref(),
    );

    // Fetch entity state BEFORE authz check so resource attributes are available.
    let current_state = match state
        .get_tenant_entity_state(tenant, entity_type, key_str)
        .await
    {
        Ok(v) => v,
        Err(e) => {
            http_span.set_status(Status::error(e.clone()));
            http_span.set_attribute(OtelKeyValue::new("http.status_code", 500i64));
            let end_time: std::time::SystemTime = sim_now().into();
            http_span.end_with_timestamp(end_time);
            return odata_error(StatusCode::INTERNAL_SERVER_ERROR, "ReadError", &e).into_response();
        }
    };

    // Build resource attributes from current entity state for Cedar evaluation.
    let mut resource_attrs = std::collections::BTreeMap::new();
    resource_attrs.insert(
        "id".to_string(),
        serde_json::Value::String(key_str.to_string()),
    );
    resource_attrs.insert(
        "status".to_string(),
        serde_json::Value::String(current_state.state.status.clone()),
    );
    // Include entity fields as resource attributes.
    if let serde_json::Value::Object(fields) = &current_state.state.fields {
        for (k, v) in fields {
            resource_attrs.insert(k.clone(), v.clone());
        }
    }

    // Resolve context entities for Cedar authorization (Gap 3: Agent OS).
    // Read [[context_entity]] declarations from the spec, resolve target entity
    // statuses, and inject as ctx_{name}_status into resource_attrs.
    {
        let context_entities: Vec<temper_spec::automaton::ContextEntityDecl> = {
            let registry = state.registry.read().unwrap(); // ci-ok: infallible lock
            registry
                .get_spec(tenant, entity_type)
                .map(|s| s.automaton.context_entities.clone())
                .unwrap_or_default()
        };

        for ce in &context_entities {
            let target_id = current_state
                .state
                .fields
                .get(&ce.id_field)
                .and_then(|v| v.as_str())
                .unwrap_or("");

            if !target_id.is_empty()
                && let Some(status) = state
                    .resolve_entity_status(tenant, &ce.entity_type, target_id)
                    .await
            {
                resource_attrs.insert(
                    format!("ctx_{}_status", ce.name),
                    serde_json::Value::String(status),
                );
            }
        }
    }

    let has_spec = {
        let registry = state.registry.read().expect("registry lock poisoned");
        registry.get_spec(tenant, entity_type).is_some()
    };
    resource_attrs.insert("has_spec".to_string(), serde_json::Value::Bool(has_spec));

    let authz_result =
        state.authorize_with_context(&security_ctx, action, entity_type, &resource_attrs);
    if let Err(reason) = authz_result {
        let pd = record_authz_denial(
            state,
            tenant.as_str(),
            &security_ctx,
            agent_ctx.agent_id.as_deref(),
            action,
            entity_type,
            key_str,
            serde_json::to_value(&resource_attrs).unwrap_or_default(),
            &reason,
            None,
            Some(current_state.state.status.clone()),
        );

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
    if let Err(v) = pre_upsert_relation_checks(
        state,
        tenant,
        entity_type,
        key_str,
        "bound_action",
        &current_fields,
    )
    .await
    {
        http_span.set_status(Status::error(v.message.clone()));
        http_span.set_attribute(OtelKeyValue::new("http.status_code", 409i64));
        let end_time: std::time::SystemTime = sim_now().into();
        http_span.end_with_timestamp(end_time);
        return constraint_violation_response(v);
    }
    if let Err(v) = post_write_invariant_checks(
        state,
        tenant,
        entity_type,
        key_str,
        action,
        &current_fields,
        "bound_action",
    )
    .await
    {
        http_span.set_status(Status::error(v.message.clone()));
        http_span.set_attribute(OtelKeyValue::new("http.status_code", 409i64));
        let end_time: std::time::SystemTime = sim_now().into();
        http_span.end_with_timestamp(end_time);
        return constraint_violation_response(v);
    }

    // Idempotency cache check
    let actor_key = format!("{entity_type}:{key_str}");
    if let Some(ref idem_key) = idempotency_key
        && let Some(cached) = state.idempotency_cache.get(&actor_key, idem_key)
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
        .dispatch_tenant_action_ext(
            tenant,
            entity_type,
            key_str,
            action,
            body_json,
            DispatchExtOptions {
                agent_ctx,
                await_integration,
            },
        )
        .await;

    let http_end: std::time::SystemTime = sim_now().into();
    let response = match result {
        Ok(response) => {
            if response.success {
                // Cache for idempotency
                if let Some(ref idem_key) = idempotency_key {
                    state
                        .idempotency_cache
                        .put(&actor_key, idem_key, response.clone());
                }

                http_span.set_status(Status::Ok);
                http_span.set_attribute(OtelKeyValue::new("http.status_code", 200i64));

                if !response.spec_governed {
                    let body = serde_json::json!({
                        "action": action,
                        "entityId": key_str,
                        "recorded": true,
                        "specGoverned": false,
                    });
                    ODataResponse {
                        status: StatusCode::OK,
                        body,
                    }
                    .into_response()
                } else {
                    let body = annotate_entity(
                        serde_json::to_value(&response.state).unwrap_or_default(),
                        format!("$metadata#{set_name}/$entity"),
                        None,
                    );
                    ODataResponse {
                        status: StatusCode::OK,
                        body,
                    }
                    .into_response()
                }
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
        Err(e) => {
            http_span.set_status(Status::error(e.clone()));
            http_span.set_attribute(OtelKeyValue::new("http.status_code", 500i64));
            odata_error(StatusCode::INTERNAL_SERVER_ERROR, "DispatchError", &e).into_response()
        }
    };

    http_span.end_with_timestamp(http_end);
    response
}
