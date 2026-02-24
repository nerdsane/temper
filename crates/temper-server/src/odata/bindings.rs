//! Bound action helpers for OData write handlers.

use axum::http::StatusCode;
use axum::response::IntoResponse;
use opentelemetry::KeyValue as OtelKeyValue;
use opentelemetry::trace::{Span, Status, Tracer};
use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;

use super::common::constraint_violation_response;
use super::response::annotate_entity;
use crate::constraint_engine::{post_write_invariant_checks, pre_upsert_relation_checks};
use crate::response::{ODataResponse, odata_error};
use crate::state::ServerState;

pub(super) async fn dispatch_bound_action(
    state: &ServerState,
    tenant: &TenantId,
    set_name: &str,
    entity_type: &str,
    key_str: &str,
    action: &str,
    body_json: serde_json::Value,
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

    let authz_result =
        state.authorize(&[], action, entity_type, &std::collections::BTreeMap::new());
    if let Err(reason) = authz_result {
        http_span.set_status(Status::error(reason.clone()));
        let end_time: std::time::SystemTime = sim_now().into();
        http_span.end_with_timestamp(end_time);
        return odata_error(StatusCode::FORBIDDEN, "AuthorizationDenied", &reason).into_response();
    }

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

    let result = state
        .dispatch_tenant_action(tenant, entity_type, key_str, action, body_json)
        .await;

    let http_end: std::time::SystemTime = sim_now().into();
    let response = match result {
        Ok(response) => {
            if response.success {
                http_span.set_status(Status::Ok);
                http_span.set_attribute(OtelKeyValue::new("http.status_code", 200i64));
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
