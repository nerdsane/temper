//! Temper Data API request dispatch.
//!
//! Translates parsed OData paths into entity actor messages via the
//! multi-tenant [`SpecRegistry`]. Tenant is extracted from the
//! `X-Tenant-Id` header (default: first registered tenant, or "default").

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use opentelemetry::trace::{Span, Status, Tracer};
use opentelemetry::KeyValue as OtelKeyValue;
use temper_odata::path::{KeyValue, ODataPath, parse_path};
use temper_odata::query::parse_query_options;
use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;

use crate::response::{odata_error, ODataResponse, ODataXmlResponse};
use crate::state::ServerState;

/// Extract the tenant ID from request headers.
///
/// Checks `X-Tenant-Id` header first. Falls back to the first registered
/// tenant in the SpecRegistry, or `TenantId::default()` if empty.
fn extract_tenant(headers: &HeaderMap, state: &ServerState) -> TenantId {
    if let Some(val) = headers.get("x-tenant-id") {
        if let Ok(s) = val.to_str() {
            if !s.is_empty() {
                return TenantId::new(s);
            }
        }
    }

    // Fall back to the first registered tenant
    let tenant_ids = state.registry.read().unwrap().tenant_ids().into_iter().cloned().collect::<Vec<_>>();
    if let Some(first) = tenant_ids.first() {
        return first.clone();
    }

    TenantId::default()
}

fn extract_key(key: &KeyValue) -> String {
    match key {
        KeyValue::Single(k) => k.clone(),
        KeyValue::Composite(pairs) => pairs.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(","),
    }
}

/// Resolve an entity set name to an entity type for a tenant.
///
/// Tries SpecRegistry first, then legacy entity_set_map.
fn resolve_entity_type(state: &ServerState, tenant: &TenantId, entity_set: &str) -> Option<String> {
    state.registry.read().unwrap().resolve_entity_type(tenant, entity_set)
        .or_else(|| state.entity_set_map.get(entity_set).cloned())
}

/// Get the CSDL XML for a tenant.
///
/// Tries SpecRegistry first, then legacy csdl_xml.
fn tenant_csdl_xml(state: &ServerState, tenant: &TenantId) -> String {
    state.registry.read().unwrap().get_tenant(tenant)
        .map(|tc| tc.csdl_xml.as_ref().clone())
        .unwrap_or_else(|| state.csdl_xml.as_ref().clone())
}

/// List entity sets for a tenant.
///
/// Tries SpecRegistry first, then legacy entity_set_map.
fn tenant_entity_sets(state: &ServerState, tenant: &TenantId) -> Vec<String> {
    let registry = state.registry.read().unwrap();
    if let Some(tc) = registry.get_tenant(tenant) {
        tc.entity_set_map.keys().cloned().collect()
    } else {
        state.entity_set_map.keys().cloned().collect()
    }
}

/// Handle GET requests.
pub async fn handle_odata_get(
    State(state): State<ServerState>,
    headers: HeaderMap,
    axum::extract::Path(path): axum::extract::Path<String>,
    Query(query_params): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let tenant = extract_tenant(&headers, &state);

    let odata_path = match parse_path(&format!("/{path}")) {
        Ok(p) => p,
        Err(e) => return odata_error(StatusCode::BAD_REQUEST, "InvalidPath", &e.to_string()).into_response(),
    };

    let query_string: String = query_params.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&");
    let _query_options = match parse_query_options(&query_string) {
        Ok(q) => q,
        Err(e) => return odata_error(StatusCode::BAD_REQUEST, "InvalidQuery", &e.to_string()).into_response(),
    };

    match odata_path {
        ODataPath::Metadata => {
            ODataXmlResponse { body: tenant_csdl_xml(&state, &tenant) }.into_response()
        }

        ODataPath::ServiceDocument => {
            let entity_sets: Vec<serde_json::Value> = tenant_entity_sets(&state, &tenant)
                .iter()
                .map(|name| serde_json::json!({"name": name, "kind": "EntitySet", "url": name}))
                .collect();
            ODataResponse {
                status: StatusCode::OK,
                body: serde_json::json!({"@odata.context": "$metadata", "value": entity_sets}),
            }.into_response()
        }

        ODataPath::EntitySet(name) => {
            if resolve_entity_type(&state, &tenant, &name).is_none() {
                return odata_error(StatusCode::NOT_FOUND, "EntitySetNotFound", &format!("Entity set '{name}' not found")).into_response();
            }
            ODataResponse {
                status: StatusCode::OK,
                body: serde_json::json!({"@odata.context": format!("$metadata#{name}"), "value": []}),
            }.into_response()
        }

        ODataPath::Entity(set_name, key) => {
            let entity_type = match resolve_entity_type(&state, &tenant, &set_name) {
                Some(t) => t,
                None => return odata_error(StatusCode::NOT_FOUND, "EntitySetNotFound", &format!("Entity set '{set_name}' not found")).into_response(),
            };
            let key_str = extract_key(&key);

            match state.get_tenant_entity_state(&tenant, &entity_type, &key_str).await {
                Ok(response) => {
                    let mut body = serde_json::to_value(&response.state).unwrap_or_default();
                    if let Some(obj) = body.as_object_mut() {
                        obj.insert("@odata.context".into(), serde_json::json!(format!("$metadata#{set_name}/$entity")));
                        obj.insert("@odata.id".into(), serde_json::json!(format!("{set_name}('{key_str}')")));
                    }
                    ODataResponse { status: StatusCode::OK, body }.into_response()
                }
                Err(_) => {
                    ODataResponse {
                        status: StatusCode::OK,
                        body: serde_json::json!({
                            "@odata.context": format!("$metadata#{set_name}/$entity"),
                            "@odata.id": format!("{set_name}('{key_str}')"),
                        }),
                    }.into_response()
                }
            }
        }

        ODataPath::BoundFunction { parent: _, function } => {
            ODataResponse {
                status: StatusCode::OK,
                body: serde_json::json!({"@odata.context": "$metadata#Edm.Untyped", "function": function}),
            }.into_response()
        }

        _ => odata_error(StatusCode::NOT_IMPLEMENTED, "NotImplemented", "This path pattern is not yet supported").into_response(),
    }
}

/// Handle POST requests — entity creation and bound actions.
pub async fn handle_odata_post(
    State(state): State<ServerState>,
    headers: HeaderMap,
    axum::extract::Path(path): axum::extract::Path<String>,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let tenant = extract_tenant(&headers, &state);

    let odata_path = match parse_path(&format!("/{path}")) {
        Ok(p) => p,
        Err(e) => return odata_error(StatusCode::BAD_REQUEST, "InvalidPath", &e.to_string()).into_response(),
    };

    match odata_path {
        ODataPath::EntitySet(name) => {
            let entity_type = match resolve_entity_type(&state, &tenant, &name) {
                Some(t) => t,
                None => return odata_error(StatusCode::NOT_FOUND, "EntitySetNotFound", &format!("Entity set '{name}' not found")).into_response(),
            };

            let _body_json: serde_json::Value = match serde_json::from_slice(&body) {
                Ok(v) => v,
                Err(e) => return odata_error(StatusCode::BAD_REQUEST, "InvalidBody", &format!("Invalid JSON body: {e}")).into_response(),
            };

            let entity_id = uuid::Uuid::now_v7().to_string();

            match state.get_tenant_entity_state(&tenant, &entity_type, &entity_id).await {
                Ok(response) => {
                    let mut body = serde_json::to_value(&response.state).unwrap_or_default();
                    if let Some(obj) = body.as_object_mut() {
                        obj.insert("@odata.context".into(), serde_json::json!(format!("$metadata#{name}/$entity")));
                        obj.insert("@odata.id".into(), serde_json::json!(format!("{name}('{entity_id}')")));
                    }
                    ODataResponse { status: StatusCode::CREATED, body }.into_response()
                }
                Err(_) => {
                    ODataResponse {
                        status: StatusCode::CREATED,
                        body: serde_json::json!({
                            "@odata.context": format!("$metadata#{name}/$entity"),
                            "id": entity_id,
                        }),
                    }.into_response()
                }
            }
        }

        ODataPath::BoundAction { parent, action } => {
            let body_json: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();

            let (set_name, key_str) = match *parent {
                ODataPath::Entity(ref set, ref key) => (set.clone(), extract_key(key)),
                _ => return odata_error(StatusCode::BAD_REQUEST, "InvalidPath", "Action must be bound to an entity").into_response(),
            };

            let entity_type = match resolve_entity_type(&state, &tenant, &set_name) {
                Some(t) => t,
                None => return odata_error(StatusCode::NOT_FOUND, "EntitySetNotFound", &format!("Entity set '{set_name}' not found")).into_response(),
            };

            // HTTP-level span: covers authz + actor dispatch + response serialization.
            // DST-safe: sim_now() for timestamps, no Instant::now().
            let http_start = sim_now();
            let tracer = opentelemetry::global::tracer("temper");
            let http_start_time: std::time::SystemTime = http_start.into();
            let span_name = format!("HTTP POST {set_name}.{action}");
            let mut http_span = tracer
                .span_builder(span_name)
                .with_start_time(http_start_time)
                .with_attributes(vec![
                    OtelKeyValue::new("http.method", "POST"),
                    OtelKeyValue::new("odata.entity_set", set_name.clone()),
                    OtelKeyValue::new("odata.entity_id", key_str.clone()),
                    OtelKeyValue::new("odata.action", action.clone()),
                    OtelKeyValue::new("tenant", tenant.as_str().to_string()),
                ])
                .start(&tracer);

            // Cedar AuthZ check
            let authz_result = state.authorize(&[], &action, &entity_type, &std::collections::HashMap::new());
            if let Err(reason) = authz_result {
                http_span.set_status(Status::error(reason.clone()));
                let end_time: std::time::SystemTime = sim_now().into();
                http_span.end_with_timestamp(end_time);
                return odata_error(StatusCode::FORBIDDEN, "AuthorizationDenied", &reason).into_response();
            }

            let result = state.dispatch_tenant_action(&tenant, &entity_type, &key_str, &action, body_json).await;

            let http_end: std::time::SystemTime = sim_now().into();
            let response = match result {
                Ok(response) => {
                    if response.success {
                        http_span.set_status(Status::Ok);
                        http_span.set_attribute(OtelKeyValue::new("http.status_code", 200i64));
                        let mut body = serde_json::to_value(&response.state).unwrap_or_default();
                        if let Some(obj) = body.as_object_mut() {
                            obj.insert("@odata.context".into(), serde_json::json!(format!("$metadata#{set_name}/$entity")));
                        }
                        ODataResponse { status: StatusCode::OK, body }.into_response()
                    } else {
                        http_span.set_status(Status::error(response.error.clone().unwrap_or_default()));
                        http_span.set_attribute(OtelKeyValue::new("http.status_code", 409i64));
                        odata_error(
                            StatusCode::CONFLICT,
                            "ActionFailed",
                            &response.error.unwrap_or_else(|| "Action failed".into()),
                        ).into_response()
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

        _ => odata_error(StatusCode::METHOD_NOT_ALLOWED, "MethodNotAllowed", "POST not supported for this path").into_response(),
    }
}

/// Handle the service document request at the root endpoint.
pub async fn handle_service_document(
    State(state): State<ServerState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let tenant = extract_tenant(&headers, &state);
    let entity_sets: Vec<serde_json::Value> = tenant_entity_sets(&state, &tenant)
        .iter()
        .map(|name| serde_json::json!({"name": name, "kind": "EntitySet", "url": name}))
        .collect();
    ODataResponse {
        status: StatusCode::OK,
        body: serde_json::json!({"@odata.context": "$metadata", "value": entity_sets}),
    }
}

/// Handle the `$metadata` request, returning the CSDL XML document.
pub async fn handle_metadata(
    State(state): State<ServerState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let tenant = extract_tenant(&headers, &state);
    ODataXmlResponse { body: tenant_csdl_xml(&state, &tenant) }
}

/// Handle the $hints endpoint, returning trajectory-learned agent hints as JSON.
pub async fn handle_hints(State(state): State<ServerState>) -> impl IntoResponse {
    let hints = state.agent_hints.read().unwrap().clone();
    ODataResponse {
        status: StatusCode::OK,
        body: serde_json::to_value(&hints).unwrap_or_default(),
    }
}
