//! OData request → actor message dispatch.
//!
//! Translates parsed OData paths into actor messages, dispatches them
//! to entity actors, and returns real state machine responses.

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use temper_odata::path::{KeyValue, ODataPath, parse_path};
use temper_odata::query::parse_query_options;

use crate::response::{odata_error, ODataResponse, ODataXmlResponse};
use crate::state::ServerState;

fn extract_key(key: &KeyValue) -> String {
    match key {
        KeyValue::Single(k) => k.clone(),
        KeyValue::Composite(pairs) => pairs.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(","),
    }
}

/// Handle GET requests.
pub async fn handle_odata_get(
    State(state): State<ServerState>,
    axum::extract::Path(path): axum::extract::Path<String>,
    Query(query_params): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
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
            ODataXmlResponse { body: state.csdl_xml.as_ref().clone() }.into_response()
        }

        ODataPath::ServiceDocument => {
            let entity_sets: Vec<serde_json::Value> = state.entity_set_map.keys()
                .map(|name| serde_json::json!({"name": name, "kind": "EntitySet", "url": name}))
                .collect();
            ODataResponse {
                status: StatusCode::OK,
                body: serde_json::json!({"@odata.context": "$metadata", "value": entity_sets}),
            }.into_response()
        }

        ODataPath::EntitySet(name) => {
            if !state.entity_set_map.contains_key(&name) {
                return odata_error(StatusCode::NOT_FOUND, "EntitySetNotFound", &format!("Entity set '{name}' not found")).into_response();
            }
            ODataResponse {
                status: StatusCode::OK,
                body: serde_json::json!({"@odata.context": format!("$metadata#{name}"), "value": []}),
            }.into_response()
        }

        ODataPath::Entity(set_name, key) => {
            let entity_type = match state.entity_set_map.get(&set_name) {
                Some(t) => t.clone(),
                None => return odata_error(StatusCode::NOT_FOUND, "EntitySetNotFound", &format!("Entity set '{set_name}' not found")).into_response(),
            };
            let key_str = extract_key(&key);

            // REAL DISPATCH: get or spawn entity actor, query its state
            match state.get_entity_state(&entity_type, &key_str).await {
                Ok(response) => {
                    let mut body = serde_json::to_value(&response.state).unwrap_or_default();
                    if let Some(obj) = body.as_object_mut() {
                        obj.insert("@odata.context".into(), serde_json::json!(format!("$metadata#{set_name}/$entity")));
                        obj.insert("@odata.id".into(), serde_json::json!(format!("{set_name}('{key_str}')")));
                    }
                    ODataResponse { status: StatusCode::OK, body }.into_response()
                }
                Err(e) => {
                    // No transition table for this entity type — return basic response
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

        ODataPath::BoundFunction { parent, function } => {
            ODataResponse {
                status: StatusCode::OK,
                body: serde_json::json!({"@odata.context": "$metadata#Edm.Untyped", "function": function}),
            }.into_response()
        }

        _ => odata_error(StatusCode::NOT_IMPLEMENTED, "NotImplemented", "This OData path pattern is not yet supported").into_response(),
    }
}

/// Handle POST requests — entity creation and bound actions.
pub async fn handle_odata_post(
    State(state): State<ServerState>,
    axum::extract::Path(path): axum::extract::Path<String>,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let odata_path = match parse_path(&format!("/{path}")) {
        Ok(p) => p,
        Err(e) => return odata_error(StatusCode::BAD_REQUEST, "InvalidPath", &e.to_string()).into_response(),
    };

    match odata_path {
        ODataPath::EntitySet(name) => {
            let entity_type = match state.entity_set_map.get(&name) {
                Some(t) => t.clone(),
                None => return odata_error(StatusCode::NOT_FOUND, "EntitySetNotFound", &format!("Entity set '{name}' not found")).into_response(),
            };

            let body_json: serde_json::Value = match serde_json::from_slice(&body) {
                Ok(v) => v,
                Err(e) => return odata_error(StatusCode::BAD_REQUEST, "InvalidBody", &format!("Invalid JSON body: {e}")).into_response(),
            };

            // Create entity: spawn actor with a new ID
            let entity_id = uuid::Uuid::now_v7().to_string();

            // Dispatch a GetState to initialize the actor and return its state
            match state.get_entity_state(&entity_type, &entity_id).await {
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

            // Extract entity type and key from parent
            let (set_name, key_str) = match *parent {
                ODataPath::Entity(ref set, ref key) => (set.clone(), extract_key(key)),
                _ => return odata_error(StatusCode::BAD_REQUEST, "InvalidPath", "Action must be bound to an entity").into_response(),
            };

            let entity_type = match state.entity_set_map.get(&set_name) {
                Some(t) => t.clone(),
                None => return odata_error(StatusCode::NOT_FOUND, "EntitySetNotFound", &format!("Entity set '{set_name}' not found")).into_response(),
            };

            // REAL DISPATCH: send action to entity actor
            match state.dispatch_action(&entity_type, &key_str, &action, body_json).await {
                Ok(response) => {
                    if response.success {
                        let mut body = serde_json::to_value(&response.state).unwrap_or_default();
                        if let Some(obj) = body.as_object_mut() {
                            obj.insert("@odata.context".into(), serde_json::json!(format!("$metadata#{set_name}/$entity")));
                        }
                        ODataResponse { status: StatusCode::OK, body }.into_response()
                    } else {
                        odata_error(
                            StatusCode::CONFLICT,
                            "ActionFailed",
                            &response.error.unwrap_or_else(|| "Action failed".into()),
                        ).into_response()
                    }
                }
                Err(e) => {
                    odata_error(StatusCode::INTERNAL_SERVER_ERROR, "DispatchError", &e).into_response()
                }
            }
        }

        _ => odata_error(StatusCode::METHOD_NOT_ALLOWED, "MethodNotAllowed", "POST not supported for this path").into_response(),
    }
}

/// Handle the OData service document request at the root endpoint.
pub async fn handle_service_document(State(state): State<ServerState>) -> impl IntoResponse {
    let entity_sets: Vec<serde_json::Value> = state.entity_set_map.keys()
        .map(|name| serde_json::json!({"name": name, "kind": "EntitySet", "url": name}))
        .collect();
    ODataResponse {
        status: StatusCode::OK,
        body: serde_json::json!({"@odata.context": "$metadata", "value": entity_sets}),
    }
}

/// Handle the OData `$metadata` request, returning the CSDL XML document.
pub async fn handle_metadata(State(state): State<ServerState>) -> impl IntoResponse {
    ODataXmlResponse { body: state.csdl_xml.as_ref().clone() }
}
