//! OData read handlers (`GET` and metadata/service endpoints).

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use temper_odata::path::{ODataPath, parse_path};
use temper_odata::query::parse_query_options;
use temper_runtime::tenant::TenantId;

use super::common::{
    extract_key, extract_tenant, has_expand_options, resolve_entity_type, tenant_csdl_xml,
    tenant_entity_sets,
};
use super::response::annotate_entity;
use crate::query_eval::{apply_query_options, expand_entity, select_fields};
use crate::response::{ODataResponse, ODataXmlResponse, odata_error};
use crate::state::ServerState;

pub(super) async fn handle_odata_get_for_tenant(
    state: ServerState,
    tenant: TenantId,
    path: String,
    query_params: std::collections::BTreeMap<String, String>,
) -> axum::response::Response {
    let odata_path = match parse_path(&format!("/{path}")) {
        Ok(p) => p,
        Err(e) => {
            return odata_error(StatusCode::BAD_REQUEST, "InvalidPath", &e.to_string())
                .into_response();
        }
    };

    let query_string: String = query_params
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");
    let query_options = match parse_query_options(&query_string) {
        Ok(q) => q,
        Err(e) => {
            return odata_error(StatusCode::BAD_REQUEST, "InvalidQuery", &e.to_string())
                .into_response();
        }
    };

    match odata_path {
        ODataPath::Metadata => ODataXmlResponse {
            body: tenant_csdl_xml(&state, &tenant),
        }
        .into_response(),

        ODataPath::ServiceDocument => {
            let entity_sets: Vec<serde_json::Value> = tenant_entity_sets(&state, &tenant)
                .iter()
                .map(|name| serde_json::json!({"name": name, "kind": "EntitySet", "url": name}))
                .collect();
            ODataResponse {
                status: StatusCode::OK,
                body: serde_json::json!({"@odata.context": "$metadata", "value": entity_sets}),
            }
            .into_response()
        }

        ODataPath::EntitySet(name) => {
            let entity_type = match resolve_entity_type(&state, &tenant, &name) {
                Some(t) => t,
                None => {
                    return odata_error(
                        StatusCode::NOT_FOUND,
                        "EntitySetNotFound",
                        &format!("Entity set '{name}' not found"),
                    )
                    .into_response();
                }
            };

            let entity_ids = state.list_entity_ids(&tenant, &entity_type);
            let mut entities = Vec::new();
            for id in &entity_ids {
                if let Ok(response) = state
                    .get_tenant_entity_state(&tenant, &entity_type, id)
                    .await
                {
                    let mut entity = serde_json::to_value(&response.state).unwrap_or_default();
                    if let Some(obj) = entity.as_object_mut() {
                        obj.insert(
                            "@odata.id".into(),
                            serde_json::json!(format!("{name}('{id}')")),
                        );
                    }
                    entities.push(entity);
                }
            }

            let (mut result, count) = apply_query_options(entities, &query_options);

            if let Some(ref expand_items) = query_options.expand {
                for entity in &mut result {
                    expand_entity(entity, expand_items, &entity_type, &state, &tenant).await;
                }
            }

            let mut body = serde_json::json!({
                "@odata.context": format!("$metadata#{name}"),
                "value": result,
            });
            if let Some(c) = count {
                body["@odata.count"] = serde_json::json!(c);
            }
            ODataResponse {
                status: StatusCode::OK,
                body,
            }
            .into_response()
        }

        ODataPath::Entity(set_name, key) => {
            let entity_type = match resolve_entity_type(&state, &tenant, &set_name) {
                Some(t) => t,
                None => {
                    return odata_error(
                        StatusCode::NOT_FOUND,
                        "EntitySetNotFound",
                        &format!("Entity set '{set_name}' not found"),
                    )
                    .into_response();
                }
            };
            let key_str = extract_key(&key);

            if !state.entity_exists(&tenant, &entity_type, &key_str) {
                return odata_error(
                    StatusCode::NOT_FOUND,
                    "ResourceNotFound",
                    &format!("Entity '{set_name}' with key '{key_str}' not found"),
                )
                .into_response();
            }

            match state
                .get_tenant_entity_state(&tenant, &entity_type, &key_str)
                .await
            {
                Ok(response) => {
                    let mut body = annotate_entity(
                        serde_json::to_value(&response.state).unwrap_or_default(),
                        format!("$metadata#{set_name}/$entity"),
                        Some(format!("{set_name}('{key_str}')")),
                    );

                    if let Some(ref expand_items) = query_options.expand {
                        expand_entity(&mut body, expand_items, &entity_type, &state, &tenant).await;
                    }

                    if let Some(ref select) = query_options.select {
                        body = select_fields(vec![body], select).pop().unwrap_or_default();
                    }

                    ODataResponse {
                        status: StatusCode::OK,
                        body,
                    }
                    .into_response()
                }
                Err(_) => odata_error(
                    StatusCode::NOT_FOUND,
                    "ResourceNotFound",
                    &format!("Entity '{set_name}' with key '{key_str}' not found"),
                )
                .into_response(),
            }
        }

        ODataPath::NavigationProperty { parent, property } => {
            let (parent_set, parent_key) = match *parent {
                ODataPath::Entity(ref set_name, ref key) => (set_name.clone(), extract_key(key)),
                _ => {
                    return odata_error(
                        StatusCode::BAD_REQUEST,
                        "InvalidPath",
                        "Navigation property requires an entity key parent path",
                    )
                    .into_response();
                }
            };

            let parent_entity_type = match resolve_entity_type(&state, &tenant, &parent_set) {
                Some(t) => t,
                None => {
                    return odata_error(
                        StatusCode::NOT_FOUND,
                        "EntitySetNotFound",
                        &format!("Entity set '{parent_set}' not found"),
                    )
                    .into_response();
                }
            };

            if !state.entity_exists(&tenant, &parent_entity_type, &parent_key) {
                return odata_error(
                    StatusCode::NOT_FOUND,
                    "ResourceNotFound",
                    &format!("Entity '{parent_set}' with key '{parent_key}' not found"),
                )
                .into_response();
            }

            let response = match state
                .get_tenant_entity_state(&tenant, &parent_entity_type, &parent_key)
                .await
            {
                Ok(r) => r,
                Err(_) => {
                    return odata_error(
                        StatusCode::NOT_FOUND,
                        "ResourceNotFound",
                        &format!("Entity '{parent_set}' with key '{parent_key}' not found"),
                    )
                    .into_response();
                }
            };

            let mut parent_body = serde_json::to_value(&response.state).unwrap_or_default();
            let nav_opts = temper_odata::query::types::ExpandOptions {
                select: query_options.select.clone(),
                filter: query_options.filter.clone(),
                orderby: query_options.orderby.clone(),
                top: query_options.top,
                skip: query_options.skip,
                expand: query_options.expand.clone(),
            };
            let expand_item = temper_odata::query::types::ExpandItem {
                property: property.clone(),
                options: if has_expand_options(&nav_opts) {
                    Some(nav_opts)
                } else {
                    None
                },
            };

            expand_entity(
                &mut parent_body,
                &[expand_item],
                &parent_entity_type,
                &state,
                &tenant,
            )
            .await;

            let Some(nav_value) = parent_body.get(&property).cloned() else {
                return odata_error(
                    StatusCode::NOT_FOUND,
                    "NavigationPropertyNotFound",
                    &format!(
                        "Navigation property '{property}' not found on entity type '{parent_entity_type}'"
                    ),
                )
                .into_response();
            };

            match nav_value {
                serde_json::Value::Array(values) => {
                    let count = values.len();
                    let mut body = serde_json::json!({
                        "@odata.context": format!("$metadata#{parent_set}('{parent_key}')/{property}"),
                        "value": values,
                    });
                    if query_options.count == Some(true) {
                        body["@odata.count"] = serde_json::json!(count);
                    }
                    ODataResponse {
                        status: StatusCode::OK,
                        body,
                    }
                    .into_response()
                }
                mut other => {
                    if let Some(obj) = other.as_object_mut() {
                        obj.insert(
                            "@odata.context".into(),
                            serde_json::json!(format!(
                                "$metadata#{parent_set}('{parent_key}')/{property}/$entity"
                            )),
                        );
                    }
                    ODataResponse {
                        status: StatusCode::OK,
                        body: other,
                    }
                    .into_response()
                }
            }
        }

        ODataPath::BoundFunction { parent, function } => {
            let (parent_set, parent_key) = match *parent {
                ODataPath::Entity(ref set_name, ref key) => (set_name.clone(), extract_key(key)),
                _ => {
                    return odata_error(
                        StatusCode::BAD_REQUEST,
                        "InvalidPath",
                        "Bound function requires an entity key parent path",
                    )
                    .into_response();
                }
            };

            let entity_type = match resolve_entity_type(&state, &tenant, &parent_set) {
                Some(et) => et,
                None => {
                    return odata_error(
                        StatusCode::NOT_FOUND,
                        "ResourceNotFound",
                        &format!("Entity set '{}' not found", parent_set),
                    )
                    .into_response();
                }
            };

            if !state.entity_exists(&tenant, &entity_type, &parent_key) {
                return odata_error(
                    StatusCode::NOT_FOUND,
                    "ResourceNotFound",
                    &format!("Entity '{parent_set}' with key '{parent_key}' not found"),
                )
                .into_response();
            }

            match state
                .get_tenant_entity_state(&tenant, &entity_type, &parent_key)
                .await
            {
                Ok(response) => {
                    let mut body = annotate_entity(
                        serde_json::to_value(&response.state).unwrap_or_default(),
                        format!("$metadata#{entity_type}"),
                        None,
                    );
                    if let Some(obj) = body.as_object_mut() {
                        obj.insert("@odata.function".to_string(), serde_json::json!(function));
                    }

                    if let Some(ref select) = query_options.select {
                        let selected = crate::query_eval::select_fields(vec![body.clone()], select);
                        if let Some(first) = selected.into_iter().next() {
                            body = first;
                        }
                    }
                    if let Some(ref expand_items) = query_options.expand {
                        crate::query_eval::expand_entity(
                            &mut body,
                            expand_items,
                            &entity_type,
                            &state,
                            &tenant,
                        )
                        .await;
                    }

                    ODataResponse {
                        status: StatusCode::OK,
                        body,
                    }
                    .into_response()
                }
                Err(_) => odata_error(
                    StatusCode::NOT_FOUND,
                    "ResourceNotFound",
                    &format!("Entity '{parent_set}' with key '{parent_key}' not found"),
                )
                .into_response(),
            }
        }

        _ => odata_error(
            StatusCode::NOT_IMPLEMENTED,
            "NotImplemented",
            "This path pattern is not yet supported",
        )
        .into_response(),
    }
}

/// Handle GET requests.
pub async fn handle_odata_get(
    State(state): State<ServerState>,
    headers: HeaderMap,
    axum::extract::Path(path): axum::extract::Path<String>,
    Query(query_params): Query<std::collections::BTreeMap<String, String>>,
) -> impl IntoResponse {
    let tenant = extract_tenant(&headers, &state);
    handle_odata_get_for_tenant(state, tenant, path, query_params).await
}

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

pub async fn handle_metadata(
    State(state): State<ServerState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let tenant = extract_tenant(&headers, &state);
    ODataXmlResponse {
        body: tenant_csdl_xml(&state, &tenant),
    }
}

pub async fn handle_hints(State(state): State<ServerState>) -> impl IntoResponse {
    let hints = state.agent_hints.read().unwrap().clone();
    ODataResponse {
        status: StatusCode::OK,
        body: serde_json::to_value(&hints).unwrap_or_default(),
    }
}
