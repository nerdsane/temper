//! OData read handlers (`GET` and metadata/service endpoints).

use std::sync::{Arc, RwLock};

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use temper_odata::path::{ODataPath, parse_path};
use temper_odata::query::parse_query_options;
use temper_odata::query::types::{ExpandItem, ExpandOptions, QueryOptions};
use temper_runtime::tenant::TenantId;
use temper_wasm::{StreamRegistry, WasmInvocationContext};
use tracing::instrument;

use super::common::{
    check_has_stream_or_400, extract_key, extract_tenant, has_expand_options, resolve_entity_type,
    resolve_value_parent, tenant_csdl_xml, tenant_entity_sets,
};
use super::read_support::{
    odata_default_page_size, odata_max_entities, record_entity_set_not_found,
    resolve_entity_set_name, select_entity_ids_for_materialization,
};
use super::response::annotate_entity;
use crate::query_eval::{apply_query_options, expand_entity, select_fields};
use crate::response::{ODataResponse, ODataStreamResponse, ODataXmlResponse, odata_error};
use crate::state::ServerState;

/// Recursively resolve an OData path to its parent entity's
/// (entity_type, entity_id, entity_set_name).
///
/// Walks the path chain from Entity through NavigationProperty
/// and NavigationEntity nodes, resolving each hop via the RelationGraph.
async fn resolve_parent_entity(
    path: &ODataPath,
    state: &ServerState,
    tenant: &TenantId,
) -> Result<(String, String, String), (StatusCode, String)> {
    match path {
        ODataPath::Entity(set_name, key) => {
            let entity_type = resolve_entity_type(state, tenant, set_name).ok_or_else(|| {
                (
                    StatusCode::NOT_FOUND,
                    format!("Entity set '{set_name}' not found"),
                )
            })?;
            let key_str = extract_key(key);
            Ok((entity_type, key_str, set_name.clone()))
        }
        ODataPath::NavigationProperty { parent, property } => {
            let (parent_type, parent_key, _parent_set) =
                Box::pin(resolve_parent_entity(parent, state, tenant)).await?;

            // Use expand to resolve the nav property
            let response = state
                .get_tenant_entity_state(tenant, &parent_type, &parent_key)
                .await
                .map_err(|_| {
                    (
                        StatusCode::NOT_FOUND,
                        format!("Parent entity '{parent_type}' with key '{parent_key}' not found"),
                    )
                })?;

            let mut parent_body = serde_json::to_value(&response.state).unwrap_or_default();
            let expand_item = ExpandItem {
                property: property.clone(),
                options: None,
            };
            expand_entity(
                &mut parent_body,
                &[expand_item],
                &parent_type,
                state,
                tenant,
            )
            .await;

            let nav_value = parent_body.get(property).ok_or_else(|| {
                (
                    StatusCode::NOT_FOUND,
                    format!("Navigation property '{property}' not found"),
                )
            })?;

            // For single-valued nav, extract the target entity type and id
            let target_type =
                resolve_navigation_target_type(state, tenant, &parent_type, property)?;

            let entity_id = nav_value
                .get("entity_id")
                .and_then(|v| v.as_str())
                .or_else(|| {
                    nav_value
                        .get("fields")
                        .and_then(|f| f.get("Id"))
                        .and_then(|v| v.as_str())
                })
                .ok_or_else(|| {
                    (
                        StatusCode::NOT_FOUND,
                        format!("Could not resolve entity id from nav property '{property}'"),
                    )
                })?
                .to_string();

            let set_name = resolve_entity_set_name(state, tenant, &target_type);
            Ok((target_type, entity_id, set_name))
        }
        ODataPath::NavigationEntity {
            parent,
            property,
            key,
        } => {
            // Resolve the parent, then the keyed entity in the nav collection
            let (parent_type, _parent_key, _parent_set) =
                Box::pin(resolve_parent_entity(parent, state, tenant)).await?;

            let target_type =
                resolve_navigation_target_type(state, tenant, &parent_type, property)?;

            let key_str = extract_key(key);
            let set_name = resolve_entity_set_name(state, tenant, &target_type);
            Ok((target_type, key_str, set_name))
        }
        _ => Err((
            StatusCode::BAD_REQUEST,
            "Cannot resolve entity from this path type".to_string(),
        )),
    }
}

fn resolve_navigation_target_type(
    state: &ServerState,
    tenant: &TenantId,
    parent_type: &str,
    property: &str,
) -> Result<String, (StatusCode, String)> {
    let registry = state.registry.read().unwrap(); // ci-ok: infallible lock
    let tenant_config = registry.get_tenant(tenant);
    tenant_config
        .and_then(|tc| crate::query_eval::find_nav_target(&tc.csdl, parent_type, property))
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                format!("Nav target for '{property}' not found"),
            )
        })
}

fn service_document_body(state: &ServerState, tenant: &TenantId) -> serde_json::Value {
    let entity_sets: Vec<serde_json::Value> = tenant_entity_sets(state, tenant)
        .iter()
        .map(|name| serde_json::json!({"name": name, "kind": "EntitySet", "url": name}))
        .collect();
    serde_json::json!({"@odata.context": "$metadata", "value": entity_sets})
}

async fn entity_set_not_found_response(
    state: &ServerState,
    tenant: &TenantId,
    set_name: &str,
) -> Response {
    record_entity_set_not_found(state, tenant.as_str(), set_name).await;
    odata_error(
        StatusCode::NOT_FOUND,
        "EntitySetNotFound",
        &format!("Entity set '{set_name}' not found"),
    )
    .into_response()
}

fn resource_not_found_response(set_name: &str, key: &str) -> Response {
    odata_error(
        StatusCode::NOT_FOUND,
        "ResourceNotFound",
        &format!("Entity '{set_name}' with key '{key}' not found"),
    )
    .into_response()
}

async fn load_existing_entity_response(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    set_name: &str,
    key: &str,
) -> Result<crate::EntityResponse, Response> {
    if !state.entity_exists(tenant, entity_type, key) {
        return Err(resource_not_found_response(set_name, key));
    }

    state
        .get_tenant_entity_state(tenant, entity_type, key)
        .await
        .map_err(|_| resource_not_found_response(set_name, key))
}

async fn apply_entity_query_options(
    mut body: serde_json::Value,
    entity_type: &str,
    state: &ServerState,
    tenant: &TenantId,
    query_options: &QueryOptions,
    select_before_expand: bool,
) -> serde_json::Value {
    if select_before_expand && let Some(ref select) = query_options.select {
        body = select_fields(vec![body], select).pop().unwrap_or_default();
    }

    if let Some(ref expand_items) = query_options.expand {
        expand_entity(&mut body, expand_items, entity_type, state, tenant).await;
    }

    if !select_before_expand && let Some(ref select) = query_options.select {
        body = select_fields(vec![body], select).pop().unwrap_or_default();
    }

    body
}

struct EntityBodyOptions<'a> {
    context: String,
    odata_id: Option<String>,
    query_options: &'a QueryOptions,
    enrich: bool,
    function: Option<&'a str>,
    select_before_expand: bool,
}

async fn build_entity_body(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    set_name: &str,
    key: &str,
    options: EntityBodyOptions<'_>,
) -> Result<serde_json::Value, Response> {
    let response = load_existing_entity_response(state, tenant, entity_type, set_name, key).await?;
    let mut body = annotate_entity(
        serde_json::to_value(&response.state).unwrap_or_default(),
        options.context,
        options.odata_id,
    );

    if options.enrich {
        enrich_entity_response(&mut body, entity_type, set_name, key, state, tenant);
    }

    if let Some(name) = options.function
        && let Some(obj) = body.as_object_mut()
    {
        obj.insert("@odata.function".to_string(), serde_json::json!(name));
    }

    Ok(apply_entity_query_options(
        body,
        entity_type,
        state,
        tenant,
        options.query_options,
        options.select_before_expand,
    )
    .await)
}

/// Enrich an entity response with `@odata.actions` and `@odata.children`.
///
/// - `@odata.actions`: Actions available from the entity's current state,
///   computed from the [`TransitionTable`].
/// - `@odata.children`: Navigation properties from the CSDL, with types and
///   target OData paths.
fn enrich_entity_response(
    body: &mut serde_json::Value,
    entity_type: &str,
    entity_set: &str,
    entity_key: &str,
    state: &ServerState,
    tenant: &TenantId,
) {
    let registry = state.registry.read().unwrap(); // ci-ok: infallible lock
    let tenant_config = registry.get_tenant(tenant);

    // --- @odata.actions: actions available from current state ---
    let current_status = body
        .get("status")
        .and_then(|v| v.as_str())
        .or_else(|| {
            body.get("fields")
                .and_then(|f| f.get("Status"))
                .and_then(|v| v.as_str())
        })
        .unwrap_or("");

    let mut actions = Vec::new();
    if let Some(tc) = tenant_config
        && let Some(spec) = tc.entities.get(entity_type)
    {
        let table = spec.table();
        for rule in &table.rules {
            if rule.from_states.iter().any(|s| s == current_status) {
                // Look up hint from automaton actions
                let hint = spec
                    .automaton
                    .actions
                    .iter()
                    .find(|a| a.name == rule.name)
                    .and_then(|a| a.hint.clone());
                let action_entry = serde_json::json!({
                    "name": rule.name,
                    "target": format!("{entity_set}('{entity_key}')/Temper.{}", rule.name),
                    "hint": hint,
                });
                // Avoid duplicate action names (multiple rules for same action)
                if !actions.iter().any(|a: &serde_json::Value| {
                    a.get("name").and_then(|n| n.as_str()) == Some(&rule.name)
                }) {
                    actions.push(action_entry);
                }
            }
        }
    }

    // --- @odata.children: navigation properties from CSDL ---
    let mut children = serde_json::Map::new();
    if let Some(tc) = tenant_config {
        for schema in &tc.csdl.schemas {
            if let Some(et) = schema.entity_type(entity_type) {
                for nav in &et.navigation_properties {
                    children.insert(
                        nav.name.clone(),
                        serde_json::json!({
                            "type": nav.type_name,
                            "target": format!("{entity_set}('{entity_key}')/{}", nav.name),
                        }),
                    );
                }
            }
        }
    }

    if let Some(obj) = body.as_object_mut() {
        obj.insert(
            "@odata.actions".to_string(),
            serde_json::Value::Array(actions),
        );
        obj.insert(
            "@odata.children".to_string(),
            serde_json::Value::Object(children),
        );
    }
}

#[instrument(skip_all, fields(tenant = %tenant, otel.name = "GET /odata/{path}"))]
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

        ODataPath::ServiceDocument => ODataResponse {
            status: StatusCode::OK,
            body: service_document_body(&state, &tenant),
        }
        .into_response(),

        ODataPath::EntitySet(name) => {
            handle_entity_set(&state, &tenant, &name, &query_options).await
        }

        ODataPath::Entity(set_name, key) => {
            handle_entity(&state, &tenant, &set_name, &key, &query_options).await
        }

        ODataPath::NavigationProperty {
            ref parent,
            ref property,
        } => handle_navigation_property(&state, &tenant, parent, property, &query_options).await,

        ODataPath::NavigationEntity {
            ref parent,
            ref property,
            ref key,
        } => handle_navigation_entity(&state, &tenant, parent, property, key, &query_options).await,

        ODataPath::BoundFunction { parent, function } => {
            handle_bound_function(&state, &tenant, &parent, &function, &query_options).await
        }

        ODataPath::Value { ref parent } => handle_stream_get(&state, &tenant, parent).await,

        _ => odata_error(
            StatusCode::NOT_IMPLEMENTED,
            "NotImplemented",
            "This path pattern is not yet supported",
        )
        .into_response(),
    }
}

/// Handle `EntitySet` path: list all entities in a set with query options.
async fn handle_entity_set(
    state: &ServerState,
    tenant: &TenantId,
    name: &str,
    query_options: &QueryOptions,
) -> axum::response::Response {
    let entity_type = match resolve_entity_type(state, tenant, name) {
        Some(t) => t,
        None => return entity_set_not_found_response(state, tenant, name).await,
    };

    let default_page_size = odata_default_page_size();
    let max_entities = odata_max_entities();

    let (entity_ids, apply_options, precomputed_count) = select_entity_ids_for_materialization(
        state.list_entity_ids_lazy(tenant, &entity_type).await,
        query_options,
        default_page_size,
        max_entities,
    );

    let mut entities = Vec::new();
    for id in &entity_ids {
        if let Ok(response) = state
            .get_tenant_entity_state(tenant, &entity_type, id)
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

    let (mut result, mut count) = apply_query_options(entities, &apply_options);
    if count.is_none() {
        count = precomputed_count;
    }

    if let Some(ref expand_items) = query_options.expand {
        for entity in &mut result {
            expand_entity(entity, expand_items, &entity_type, state, tenant).await;
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

/// Handle `Entity` path: fetch a single entity by key.
async fn handle_entity(
    state: &ServerState,
    tenant: &TenantId,
    set_name: &str,
    key: &temper_odata::path::KeyValue,
    query_options: &QueryOptions,
) -> axum::response::Response {
    let entity_type = match resolve_entity_type(state, tenant, set_name) {
        Some(t) => t,
        None => return entity_set_not_found_response(state, tenant, set_name).await,
    };
    let key_str = extract_key(key);

    match build_entity_body(
        state,
        tenant,
        &entity_type,
        set_name,
        &key_str,
        EntityBodyOptions {
            context: format!("$metadata#{set_name}/$entity"),
            odata_id: Some(format!("{set_name}('{key_str}')")),
            query_options,
            enrich: true,
            function: None,
            select_before_expand: false,
        },
    )
    .await
    {
        Ok(body) => ODataResponse {
            status: StatusCode::OK,
            body,
        }
        .into_response(),
        Err(resp) => resp,
    }
}

/// Handle `NavigationProperty` path: resolve parent and expand nav property.
async fn handle_navigation_property(
    state: &ServerState,
    tenant: &TenantId,
    parent: &ODataPath,
    property: &str,
    query_options: &QueryOptions,
) -> axum::response::Response {
    let (parent_type, parent_key, parent_set) =
        match resolve_parent_entity(parent, state, tenant).await {
            Ok(r) => r,
            Err((status, msg)) => {
                return odata_error(status, "InvalidPath", &msg).into_response();
            }
        };

    let response =
        match load_existing_entity_response(state, tenant, &parent_type, &parent_set, &parent_key)
            .await
        {
            Ok(r) => r,
            Err(resp) => return resp,
        };

    let mut parent_body = serde_json::to_value(&response.state).unwrap_or_default();
    let nav_opts = ExpandOptions {
        select: query_options.select.clone(),
        filter: query_options.filter.clone(),
        orderby: query_options.orderby.clone(),
        top: query_options.top,
        skip: query_options.skip,
        expand: query_options.expand.clone(),
    };
    let expand_item = ExpandItem {
        property: property.to_string(),
        options: if has_expand_options(&nav_opts) {
            Some(nav_opts)
        } else {
            None
        },
    };

    expand_entity(
        &mut parent_body,
        &[expand_item],
        &parent_type,
        state,
        tenant,
    )
    .await;

    let Some(nav_value) = parent_body.get(property).cloned() else {
        return odata_error(
            StatusCode::NOT_FOUND,
            "NavigationPropertyNotFound",
            &format!("Navigation property '{property}' not found on entity type '{parent_type}'"),
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

/// Handle `NavigationEntity` path: resolve parent, then fetch keyed child.
async fn handle_navigation_entity(
    state: &ServerState,
    tenant: &TenantId,
    parent: &ODataPath,
    property: &str,
    key: &temper_odata::path::KeyValue,
    query_options: &QueryOptions,
) -> axum::response::Response {
    let (parent_type, _parent_key, _parent_set) =
        match resolve_parent_entity(parent, state, tenant).await {
            Ok(r) => r,
            Err((status, msg)) => {
                return odata_error(status, "InvalidPath", &msg).into_response();
            }
        };

    let Ok(target_type) = resolve_navigation_target_type(state, tenant, &parent_type, property)
    else {
        return odata_error(
            StatusCode::NOT_FOUND,
            "NavigationPropertyNotFound",
            &format!("Navigation property '{property}' not found on '{parent_type}'"),
        )
        .into_response();
    };

    let key_str = extract_key(key);
    let target_set = resolve_entity_set_name(state, tenant, &target_type);

    match build_entity_body(
        state,
        tenant,
        &target_type,
        &target_set,
        &key_str,
        EntityBodyOptions {
            context: format!("$metadata#{target_set}/$entity"),
            odata_id: Some(format!("{target_set}('{key_str}')")),
            query_options,
            enrich: true,
            function: None,
            select_before_expand: false,
        },
    )
    .await
    {
        Ok(body) => ODataResponse {
            status: StatusCode::OK,
            body,
        }
        .into_response(),
        Err(resp) => resp,
    }
}

/// Handle `BoundFunction` path: fetch entity and annotate with function info.
async fn handle_bound_function(
    state: &ServerState,
    tenant: &TenantId,
    parent: &ODataPath,
    function: &str,
    query_options: &QueryOptions,
) -> axum::response::Response {
    let (parent_set, parent_key) = match parent {
        ODataPath::Entity(set_name, key) => (set_name.clone(), extract_key(key)),
        _ => {
            return odata_error(
                StatusCode::BAD_REQUEST,
                "InvalidPath",
                "Bound function requires an entity key parent path",
            )
            .into_response();
        }
    };

    let entity_type = match resolve_entity_type(state, tenant, &parent_set) {
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

    match build_entity_body(
        state,
        tenant,
        &entity_type,
        &parent_set,
        &parent_key,
        EntityBodyOptions {
            context: format!("$metadata#{entity_type}"),
            odata_id: None,
            query_options,
            enrich: false,
            function: Some(function),
            select_before_expand: true,
        },
    )
    .await
    {
        Ok(body) => ODataResponse {
            status: StatusCode::OK,
            body,
        }
        .into_response(),
        Err(resp) => resp,
    }
}

/// Handle GET requests.
#[instrument(skip_all, fields(otel.name = "GET /odata/{path}"))]
pub async fn handle_odata_get(
    State(state): State<ServerState>,
    headers: HeaderMap,
    axum::extract::Path(path): axum::extract::Path<String>,
    Query(query_params): Query<std::collections::BTreeMap<String, String>>,
) -> impl IntoResponse {
    let tenant = match extract_tenant(&headers, &state) {
        Ok(t) => t,
        Err(e) => return e.into_response(),
    };
    handle_odata_get_for_tenant(state, tenant, path, query_params).await
}

#[instrument(skip_all, fields(otel.name = "GET /odata"))]
pub async fn handle_service_document(
    State(state): State<ServerState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let tenant = match extract_tenant(&headers, &state) {
        Ok(t) => t,
        Err(e) => return e.into_response(),
    };
    ODataResponse {
        status: StatusCode::OK,
        body: service_document_body(&state, &tenant),
    }
    .into_response()
}

#[instrument(skip_all, fields(otel.name = "GET /odata/$metadata"))]
pub async fn handle_metadata(
    State(state): State<ServerState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let tenant = match extract_tenant(&headers, &state) {
        Ok(t) => t,
        Err(e) => return e.into_response(),
    };
    ODataXmlResponse {
        body: tenant_csdl_xml(&state, &tenant),
    }
    .into_response()
}

#[instrument(skip_all, fields(otel.name = "GET /odata/hints"))]
pub async fn handle_hints(State(state): State<ServerState>) -> impl IntoResponse {
    let hints = state.agent_hints.read().unwrap().clone();
    ODataResponse {
        status: StatusCode::OK,
        body: serde_json::to_value(&hints).unwrap_or_default(),
    }
}

/// Handle GET on `$value` — download binary content via WASM blob_adapter.
///
/// Flow:
/// 1. Resolve parent entity from ODataPath
/// 2. Verify entity type has `HasStream=true` in CSDL
/// 3. Get entity state (content_hash, mime_type, etc.)
/// 4. Invoke WASM blob_adapter (handles auth, caching, download)
/// 5. Read downloaded bytes from StreamRegistry
/// 6. Return binary response
#[instrument(skip_all, fields(otel.name = "GET $value"))]
async fn handle_stream_get(
    state: &ServerState,
    tenant: &TenantId,
    parent: &ODataPath,
) -> axum::response::Response {
    // 1. Resolve parent to (set_name, entity_id)
    let (set_name, key) = match resolve_value_parent(parent) {
        Ok(pair) => pair,
        Err(resp) => return resp,
    };

    let entity_type = match resolve_entity_type(state, tenant, &set_name) {
        Some(t) => t,
        None => return entity_set_not_found_response(state, tenant, &set_name).await,
    };

    // 2. Check HasStream=true
    if let Err(resp) = check_has_stream_or_400(state, tenant, &entity_type) {
        return resp;
    }

    // 3. Get entity state
    let entity_state =
        match load_existing_entity_response(state, tenant, &entity_type, &set_name, &key).await {
            Ok(resp) => serde_json::to_value(&resp.state).unwrap_or_default(),
            Err(resp) => return resp,
        };

    // 4. Check if entity has content (boolean may be in top-level `booleans` map or `fields`)
    let has_content = entity_state
        .get("booleans")
        .and_then(|b| b.get("has_content"))
        .and_then(|v| v.as_bool())
        .or_else(|| {
            entity_state
                .get("fields")
                .and_then(|f| f.get("has_content"))
                .and_then(|v| v.as_bool())
        })
        .unwrap_or(false);
    if !has_content {
        return odata_error(
            StatusCode::NOT_FOUND,
            "NoContent",
            &format!("{set_name}('{key}') has no content yet"),
        )
        .into_response();
    }

    // 5. Invoke WASM blob_adapter for download
    let response_stream_id = format!("download-{}", temper_runtime::scheduler::sim_uuid());
    let streams = Arc::new(RwLock::new(StreamRegistry::default()));

    let inv_ctx = WasmInvocationContext {
        tenant: tenant.to_string(),
        entity_type: entity_type.clone(),
        entity_id: key.clone(),
        trigger_action: "StreamDownload".to_string(),
        trigger_params: serde_json::json!({
            "stream_id": response_stream_id,
            "operation": "get",
        }),
        entity_state: entity_state.clone(),
        agent_id: None,
        session_id: None,
        integration_config: std::collections::BTreeMap::new(),
        trace_id: String::new(),
    };

    let wasm_result = match state
        .invoke_wasm_direct(tenant, "blob_adapter", inv_ctx, streams.clone())
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "WASM blob_adapter download failed");
            return odata_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "BlobAdapterError",
                &format!("Blob adapter failed: {e}"),
            )
            .into_response();
        }
    };

    if !wasm_result.success {
        let error_msg = wasm_result
            .error
            .unwrap_or_else(|| "unknown error".to_string());
        return odata_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "BlobDownloadFailed",
            &error_msg,
        )
        .into_response();
    }

    // 6. Read bytes from StreamRegistry
    let body_bytes = {
        let mut s = streams.write().unwrap(); // ci-ok: infallible lock
        s.take_stream(&response_stream_id).unwrap_or_default()
    };

    // Extract content_type and etag from entity state fields
    let fields = entity_state.get("fields").cloned().unwrap_or_default();
    let content_type = fields
        .get("mime_type")
        .and_then(|v| v.as_str())
        .unwrap_or("application/octet-stream")
        .to_string();
    let etag = fields
        .get("content_hash")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    ODataStreamResponse {
        status: StatusCode::OK,
        body: body_bytes,
        content_type,
        etag,
    }
    .into_response()
}
