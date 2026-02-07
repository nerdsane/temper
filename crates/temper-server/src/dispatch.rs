//! OData request → actor message dispatch.
//!
//! This module translates parsed OData paths and query options into
//! actor messages and dispatches them to the appropriate entity actors.

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;

use temper_odata::path::{ODataPath, parse_path};
use temper_odata::query::parse_query_options;

use crate::response::{odata_error, ODataResponse, ODataXmlResponse};
use crate::state::ServerState;

/// Handle GET requests to OData endpoints.
/// Dispatches to: $metadata, entity set listing, entity by key, navigation, functions.
pub async fn handle_odata_get(
    State(state): State<ServerState>,
    axum::extract::Path(path): axum::extract::Path<String>,
    Query(query_params): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let odata_path = match parse_path(&format!("/{path}")) {
        Ok(p) => p,
        Err(e) => return odata_error(StatusCode::BAD_REQUEST, "InvalidPath", &e.to_string()).into_response(),
    };

    // Build query string from params for OData parsing
    let query_string: String = query_params
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");

    let _query_options = match parse_query_options(&query_string) {
        Ok(q) => q,
        Err(e) => return odata_error(StatusCode::BAD_REQUEST, "InvalidQuery", &e.to_string()).into_response(),
    };

    match odata_path {
        ODataPath::Metadata => {
            ODataXmlResponse {
                body: state.csdl_xml.as_ref().clone(),
            }
            .into_response()
        }

        ODataPath::ServiceDocument => {
            let entity_sets: Vec<serde_json::Value> = state
                .entity_set_map
                .keys()
                .map(|name| {
                    serde_json::json!({
                        "name": name,
                        "kind": "EntitySet",
                        "url": name,
                    })
                })
                .collect();

            ODataResponse {
                status: StatusCode::OK,
                body: serde_json::json!({
                    "@odata.context": "$metadata",
                    "value": entity_sets,
                }),
            }
            .into_response()
        }

        ODataPath::EntitySet(name) => {
            if !state.entity_set_map.contains_key(&name) {
                return odata_error(
                    StatusCode::NOT_FOUND,
                    "EntitySetNotFound",
                    &format!("Entity set '{name}' not found"),
                )
                .into_response();
            }

            // TODO: dispatch to shard manager actor for listing
            ODataResponse {
                status: StatusCode::OK,
                body: serde_json::json!({
                    "@odata.context": format!("$metadata#{name}"),
                    "value": [],
                }),
            }
            .into_response()
        }

        ODataPath::Entity(set_name, key) => {
            if !state.entity_set_map.contains_key(&set_name) {
                return odata_error(
                    StatusCode::NOT_FOUND,
                    "EntitySetNotFound",
                    &format!("Entity set '{set_name}' not found"),
                )
                .into_response();
            }

            let key_str = match &key {
                temper_odata::path::KeyValue::Single(k) => k.clone(),
                temper_odata::path::KeyValue::Composite(pairs) => {
                    pairs
                        .iter()
                        .map(|(k, v)| format!("{k}={v}"))
                        .collect::<Vec<_>>()
                        .join(",")
                }
            };

            // TODO: dispatch to entity actor via ask
            ODataResponse {
                status: StatusCode::OK,
                body: serde_json::json!({
                    "@odata.context": format!("$metadata#{set_name}/$entity"),
                    "@odata.id": format!("{set_name}('{key_str}')"),
                }),
            }
            .into_response()
        }

        ODataPath::BoundFunction { parent, function } => {
            ODataResponse {
                status: StatusCode::OK,
                body: serde_json::json!({
                    "@odata.context": "$metadata#Edm.Untyped",
                    "function": function,
                    "parent": format!("{parent:?}"),
                }),
            }
            .into_response()
        }

        _ => {
            odata_error(
                StatusCode::NOT_IMPLEMENTED,
                "NotImplemented",
                "This OData path pattern is not yet supported",
            )
            .into_response()
        }
    }
}

/// Handle POST requests to OData endpoints.
/// Dispatches to: entity creation, bound actions.
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
            if !state.entity_set_map.contains_key(&name) {
                return odata_error(
                    StatusCode::NOT_FOUND,
                    "EntitySetNotFound",
                    &format!("Entity set '{name}' not found"),
                )
                .into_response();
            }

            // Parse request body as JSON
            let _body_json: serde_json::Value = match serde_json::from_slice(&body) {
                Ok(v) => v,
                Err(e) => {
                    return odata_error(
                        StatusCode::BAD_REQUEST,
                        "InvalidBody",
                        &format!("Invalid JSON body: {e}"),
                    )
                    .into_response();
                }
            };

            // TODO: dispatch create command to actor
            ODataResponse {
                status: StatusCode::CREATED,
                body: serde_json::json!({
                    "@odata.context": format!("$metadata#{name}/$entity"),
                    "id": uuid::Uuid::now_v7().to_string(),
                }),
            }
            .into_response()
        }

        ODataPath::BoundAction { parent, action } => {
            let _body_json: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();

            // TODO: dispatch action command to entity actor
            ODataResponse {
                status: StatusCode::OK,
                body: serde_json::json!({
                    "@odata.context": "$metadata#Edm.Untyped",
                    "action": action,
                    "parent": format!("{parent:?}"),
                    "status": "dispatched",
                }),
            }
            .into_response()
        }

        _ => {
            odata_error(
                StatusCode::METHOD_NOT_ALLOWED,
                "MethodNotAllowed",
                "POST is not supported for this OData path",
            )
            .into_response()
        }
    }
}

/// Handle the root service document.
pub async fn handle_service_document(
    State(state): State<ServerState>,
) -> impl IntoResponse {
    let entity_sets: Vec<serde_json::Value> = state
        .entity_set_map
        .keys()
        .map(|name| {
            serde_json::json!({
                "name": name,
                "kind": "EntitySet",
                "url": name,
            })
        })
        .collect();

    ODataResponse {
        status: StatusCode::OK,
        body: serde_json::json!({
            "@odata.context": "$metadata",
            "value": entity_sets,
        }),
    }
}

/// Handle $metadata requests.
pub async fn handle_metadata(
    State(state): State<ServerState>,
) -> impl IntoResponse {
    ODataXmlResponse {
        body: state.csdl_xml.as_ref().clone(),
    }
}
