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
use crate::state::trajectory::{TrajectoryEntry, TrajectorySource};
use temper_runtime::scheduler::sim_now;

/// Resolve an entity set name from an entity type name.
///
/// Reverse-lookups the entity_set_map to find the set name for a given type.
fn resolve_entity_set_name(state: &ServerState, tenant: &TenantId, entity_type: &str) -> String {
    let registry = state.registry.read().unwrap(); // ci-ok: infallible lock
    if let Some(tc) = registry.get_tenant(tenant) {
        for (set_name, type_name) in &tc.entity_set_map {
            if type_name == entity_type {
                return set_name.clone();
            }
        }
    }
    // Fallback: pluralize entity type
    format!("{entity_type}s")
}

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
            let expand_item = temper_odata::query::types::ExpandItem {
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
            let target_type = {
                let registry = state.registry.read().unwrap(); // ci-ok: infallible lock
                let tc = registry.get_tenant(tenant);
                tc.and_then(|tc| {
                    crate::query_eval::find_nav_target(&tc.csdl, &parent_type, property)
                })
                .ok_or_else(|| {
                    (
                        StatusCode::NOT_FOUND,
                        format!("Nav target for '{property}' not found"),
                    )
                })?
            };

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

            let target_type = {
                let registry = state.registry.read().unwrap(); // ci-ok: infallible lock
                let tc = registry.get_tenant(tenant);
                tc.and_then(|tc| {
                    crate::query_eval::find_nav_target(&tc.csdl, &parent_type, property)
                })
                .ok_or_else(|| {
                    (
                        StatusCode::NOT_FOUND,
                        format!("Nav target for '{property}' not found"),
                    )
                })?
            };

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
    if let Some(tc) = tenant_config {
        if let Some(spec) = tc.entities.get(entity_type) {
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

/// Record a trajectory entry for an EntitySetNotFound error.
fn record_entity_set_not_found(state: &ServerState, tenant: &str, set_name: &str) {
    let entry = TrajectoryEntry {
        timestamp: sim_now().to_rfc3339(),
        tenant: tenant.to_string(),
        entity_type: set_name.to_string(),
        entity_id: "".to_string(),
        action: "EntitySetLookup".to_string(),
        success: false,
        from_status: None,
        to_status: None,
        error: Some(format!(
            "EntitySetNotFound: entity set '{}' not found",
            set_name
        )),
        agent_id: None,
        session_id: None,
        authz_denied: None,
        denied_resource: None,
        denied_module: None,
        source: Some(TrajectorySource::Platform),
        spec_governed: None,
    };
    if let Ok(mut log) = state.trajectory_log.write() {
        log.push(entry);
    }
}

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
                    record_entity_set_not_found(&state, tenant.as_str(), &name);
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
                    record_entity_set_not_found(&state, tenant.as_str(), &set_name);
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

                    // Enrich with available actions and child nav properties
                    enrich_entity_response(
                        &mut body,
                        &entity_type,
                        &set_name,
                        &key_str,
                        &state,
                        &tenant,
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

        ODataPath::NavigationProperty {
            ref parent,
            ref property,
        } => {
            // Recursively resolve the parent entity
            let (parent_type, parent_key, parent_set) =
                match resolve_parent_entity(parent, &state, &tenant).await {
                    Ok(r) => r,
                    Err((status, msg)) => {
                        return odata_error(status, "InvalidPath", &msg).into_response();
                    }
                };

            if !state.entity_exists(&tenant, &parent_type, &parent_key) {
                return odata_error(
                    StatusCode::NOT_FOUND,
                    "ResourceNotFound",
                    &format!("Entity '{parent_set}' with key '{parent_key}' not found"),
                )
                .into_response();
            }

            let response = match state
                .get_tenant_entity_state(&tenant, &parent_type, &parent_key)
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
                &parent_type,
                &state,
                &tenant,
            )
            .await;

            let Some(nav_value) = parent_body.get(property).cloned() else {
                return odata_error(
                    StatusCode::NOT_FOUND,
                    "NavigationPropertyNotFound",
                    &format!(
                        "Navigation property '{property}' not found on entity type '{parent_type}'"
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

        ODataPath::NavigationEntity {
            ref parent,
            ref property,
            ref key,
        } => {
            // Resolve the parent entity, then fetch the keyed child
            let (parent_type, _parent_key, _parent_set) =
                match resolve_parent_entity(parent, &state, &tenant).await {
                    Ok(r) => r,
                    Err((status, msg)) => {
                        return odata_error(status, "InvalidPath", &msg).into_response();
                    }
                };

            let target_type = {
                let registry = state.registry.read().unwrap(); // ci-ok: infallible lock
                let tc = registry.get_tenant(&tenant);
                tc.and_then(|tc| {
                    crate::query_eval::find_nav_target(&tc.csdl, &parent_type, property)
                })
            };

            let Some(target_type) = target_type else {
                return odata_error(
                    StatusCode::NOT_FOUND,
                    "NavigationPropertyNotFound",
                    &format!("Navigation property '{property}' not found on '{parent_type}'"),
                )
                .into_response();
            };

            let key_str = extract_key(key);
            let target_set = resolve_entity_set_name(&state, &tenant, &target_type);

            if !state.entity_exists(&tenant, &target_type, &key_str) {
                return odata_error(
                    StatusCode::NOT_FOUND,
                    "ResourceNotFound",
                    &format!("Entity '{target_set}' with key '{key_str}' not found"),
                )
                .into_response();
            }

            match state
                .get_tenant_entity_state(&tenant, &target_type, &key_str)
                .await
            {
                Ok(response) => {
                    let mut body = annotate_entity(
                        serde_json::to_value(&response.state).unwrap_or_default(),
                        format!("$metadata#{target_set}/$entity"),
                        Some(format!("{target_set}('{key_str}')")),
                    );

                    enrich_entity_response(
                        &mut body,
                        &target_type,
                        &target_set,
                        &key_str,
                        &state,
                        &tenant,
                    );

                    if let Some(ref expand_items) = query_options.expand {
                        expand_entity(&mut body, expand_items, &target_type, &state, &tenant).await;
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
                    &format!("Entity '{target_set}' with key '{key_str}' not found"),
                )
                .into_response(),
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
