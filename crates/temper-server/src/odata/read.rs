//! OData read handlers (`GET` and metadata/service endpoints).

use std::sync::{Arc, RwLock};

use axum::extract::{Extension, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use temper_authz::SecurityContext;
use temper_odata::path::{ODataPath, parse_path};
use temper_odata::query::parse_query_options;
use temper_odata::query::types::{ExpandItem, ExpandOptions, QueryOptions};
use temper_runtime::tenant::TenantId;
use temper_wasm::{StreamRegistry, WasmInvocationContext};
use tracing::instrument;

use super::authz::{READ_ACTION, authorize_read, request_security_context};
use super::common::{
    check_has_stream_or_400, extract_key, extract_tenant, has_expand_options,
    log_directed_evolution_odata_request, resolve_entity_type, resolve_value_parent,
    tenant_csdl_xml, tenant_entity_sets,
};
use super::query_plane_read::{
    QueryPlaneReadBudget, QueryPlaneReadRequest, read_entity_set_from_query_plane,
};
use super::read_support::{
    record_entity_set_not_found, resolve_entity_set_name, try_load_entity_body_from_catalog,
};
use super::response::annotate_entity;
use super::stream_fast_path::try_file_stream_fast_path;
use crate::blobs::hydrate_blob_refs_for_tenant;
use crate::identity::ResolvedIdentity;
use crate::query_eval::{expand_entity, select_fields};
use crate::request_context::extract_agent_context;
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
    security_ctx: &SecurityContext,
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
                Box::pin(resolve_parent_entity(parent, state, tenant, security_ctx)).await?;

            // Use expand to resolve the nav property
            let parent_set = resolve_entity_set_name(state, tenant, &parent_type);
            let mut parent_body = load_authorized_entity_body(
                state,
                tenant,
                &parent_type,
                &parent_set,
                &parent_key,
                security_ctx,
            )
            .await
            .map_err(|_| {
                (
                    StatusCode::FORBIDDEN,
                    format!("Read access denied for parent entity '{parent_type}'"),
                )
            })?;
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
                security_ctx,
            )
            .await
            .map_err(|_| {
                (
                    StatusCode::FORBIDDEN,
                    format!("Read access denied for navigation property '{property}'"),
                )
            })?;

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
                Box::pin(resolve_parent_entity(parent, state, tenant, security_ctx)).await?;

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
    let registry = state
        .registry
        .read()
        .expect("registry lock should not be poisoned"); // ci-ok: infallible lock
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

/// Load a single entity body for OData read handlers.
///
/// Tries the durable `entity_catalog` projection first when a query-plane
/// store is configured or the `TEMPER_ODATA_CATALOG_FAST_READ` flag is on. On
/// a hit, returns a body whose shape matches the actor's serialized
/// `EntityState` — blob refs already hydrated. On miss (no catalog row,
/// catalog disabled, or backend error), falls back to the actor path: validate
/// the entity exists in the in-memory index, then load via
/// `get_tenant_entity_state`.
///
/// The catalog-first path bypasses the in-memory index, so it survives
/// cold starts and bulk-imported entities (data on disk but not yet
/// indexed in the running process). See ADR-0077.
async fn load_existing_entity_body(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    set_name: &str,
    key: &str,
) -> Result<serde_json::Value, Response> {
    let prefer_catalog = state.query_plane_store().is_some();
    if let Some(body) =
        try_load_entity_body_from_catalog(state, tenant, entity_type, set_name, key, prefer_catalog)
            .await
    {
        return Ok(body);
    }

    if !state.entity_exists(tenant, entity_type, key) {
        return Err(resource_not_found_response(set_name, key));
    }

    let response = state
        .get_tenant_entity_state(tenant, entity_type, key)
        .await
        .map_err(|_| resource_not_found_response(set_name, key))?;
    let mut body = serde_json::to_value(&response.state).unwrap_or_default();
    hydrate_blob_refs_for_tenant(state, tenant, &mut body).await;
    if let Some(obj) = body.as_object_mut() {
        obj.insert(
            "@odata.id".into(),
            serde_json::json!(format!("{set_name}('{key}')")),
        );
    }
    Ok(body)
}

async fn load_authorized_entity_body(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    set_name: &str,
    key: &str,
    security_ctx: &SecurityContext,
) -> Result<serde_json::Value, Response> {
    let body = load_existing_entity_body(state, tenant, entity_type, set_name, key).await?;
    authorize_read(
        state,
        tenant,
        security_ctx,
        READ_ACTION,
        entity_type,
        key,
        &body,
    )
    .map_err(|response| *response)?;
    Ok(body)
}

async fn apply_entity_query_options(
    mut body: serde_json::Value,
    entity_type: &str,
    state: &ServerState,
    tenant: &TenantId,
    security_ctx: &SecurityContext,
    query_options: &QueryOptions,
    select_before_expand: bool,
) -> Result<serde_json::Value, Response> {
    if select_before_expand && let Some(ref select) = query_options.select {
        body = select_fields(vec![body], select).pop().unwrap_or_default();
    }

    if let Some(ref expand_items) = query_options.expand {
        expand_entity(
            &mut body,
            expand_items,
            entity_type,
            state,
            tenant,
            security_ctx,
        )
        .await?;
    }

    if !select_before_expand && let Some(ref select) = query_options.select {
        body = select_fields(vec![body], select).pop().unwrap_or_default();
    }

    Ok(body)
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
    security_ctx: &SecurityContext,
    options: EntityBodyOptions<'_>,
) -> Result<serde_json::Value, Response> {
    let mut state_json =
        load_authorized_entity_body(state, tenant, entity_type, set_name, key, security_ctx)
            .await?;
    if let Some(obj) = state_json.as_object_mut() {
        obj.remove("@odata.id");
    }
    let mut body = annotate_entity(state_json, options.context, options.odata_id);

    if options.enrich {
        enrich_entity_response(&mut body, entity_type, set_name, key, state, tenant);
    }

    if let Some(name) = options.function
        && let Some(obj) = body.as_object_mut()
    {
        obj.insert("@odata.function".to_string(), serde_json::json!(name));
    }

    apply_entity_query_options(
        body,
        entity_type,
        state,
        tenant,
        security_ctx,
        options.query_options,
        options.select_before_expand,
    )
    .await
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
    let registry = state
        .registry
        .read()
        .expect("registry lock should not be poisoned"); // ci-ok: infallible lock
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
    security_ctx: SecurityContext,
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
            handle_entity_set(&state, &tenant, &security_ctx, &name, &query_options).await
        }

        ODataPath::Entity(set_name, key) => {
            handle_entity(
                &state,
                &tenant,
                &security_ctx,
                &set_name,
                &key,
                &query_options,
            )
            .await
        }

        ODataPath::NavigationProperty {
            ref parent,
            ref property,
        } => {
            handle_navigation_property(
                &state,
                &tenant,
                &security_ctx,
                parent,
                property,
                &query_options,
            )
            .await
        }

        ODataPath::NavigationEntity {
            ref parent,
            ref property,
            ref key,
        } => {
            handle_navigation_entity(
                &state,
                &tenant,
                &security_ctx,
                parent,
                property,
                key,
                &query_options,
            )
            .await
        }

        ODataPath::BoundFunction { parent, function } => {
            handle_bound_function(
                &state,
                &tenant,
                &security_ctx,
                &parent,
                &function,
                &query_options,
            )
            .await
        }

        ODataPath::Value { ref parent } => {
            handle_stream_get(&state, &tenant, &security_ctx, parent).await
        }

        _ => odata_error(
            StatusCode::NOT_IMPLEMENTED,
            "NotImplemented",
            "This path pattern is not yet supported",
        )
        .into_response(),
    }
}

/// Handle `EntitySet` path: list all entities in a set with query options.
#[instrument(skip_all, fields(
    otel.name = "odata.entity_set_read",
    tenant = %tenant,
    entity_set = %name,
    entity_type = tracing::field::Empty,
    filter_pushdown = tracing::field::Empty,
    id_source = tracing::field::Empty,
    catalog_materialization = tracing::field::Empty,
    candidate_count = tracing::field::Empty,
    materialized_count = tracing::field::Empty,
    returned_count = tracing::field::Empty,
    catalog_shadow_check_budget = tracing::field::Empty,
    catalog_shadow_check_scheduled = tracing::field::Empty,
    catalog_coverage_missing = tracing::field::Empty,
    catalog_coverage_matched = tracing::field::Empty,
    select_requested = tracing::field::Empty,
    catalog_select_projection = tracing::field::Empty,
    select_count = tracing::field::Empty,
    pushdown_sparse_page = tracing::field::Empty,
    pushdown_sparse_probe_count = tracing::field::Empty,
    pushdown_page_count = tracing::field::Empty,
    pushdown_sparse_skip_reason = tracing::field::Empty,
))]
async fn handle_entity_set(
    state: &ServerState,
    tenant: &TenantId,
    security_ctx: &SecurityContext,
    name: &str,
    query_options: &QueryOptions,
) -> axum::response::Response {
    tracing::debug!(name = %name, tenant = %tenant, "handle_entity_set");
    let entity_type = match resolve_entity_type(state, tenant, name) {
        Some(t) => t,
        None => return entity_set_not_found_response(state, tenant, name).await,
    };
    let span = tracing::Span::current();
    span.record("entity_type", entity_type.as_str());
    let read_result = match read_entity_set_from_query_plane(QueryPlaneReadRequest {
        state,
        tenant,
        security_ctx,
        entity_type: &entity_type,
        entity_set_name: name,
        query_options,
        budget: QueryPlaneReadBudget::from_config(),
    })
    .await
    {
        Ok(result) => result,
        Err(error) => {
            error.record_telemetry(&span);
            return error.into_response();
        }
    };
    read_result.telemetry.record(&span);

    let mut result = read_result.entities;
    if let Some(ref expand_items) = query_options.expand {
        for entity in &mut result {
            if let Err(response) = expand_entity(
                entity,
                expand_items,
                &entity_type,
                state,
                tenant,
                security_ctx,
            )
            .await
            {
                return response;
            }
        }
    }

    let count = read_result.count;
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
    security_ctx: &SecurityContext,
    set_name: &str,
    key: &temper_odata::path::KeyValue,
    query_options: &QueryOptions,
) -> axum::response::Response {
    let entity_type = match resolve_entity_type(state, tenant, set_name) {
        Some(t) => t,
        None => return entity_set_not_found_response(state, tenant, set_name).await,
    };
    let key_str = extract_key(key);

    if state.is_pg_actor_backed(tenant, &entity_type)
        && let Some(actor_sys) = &state.pg_actor_system
    {
        let namespace = format!("{tenant}/{key_str}");
        return match actor_sys.load_state(&namespace, &entity_type).await {
            Ok(Some(state_bytes)) => {
                let actor_state: temper_actor_runtime::spec_actor::SpecActorState =
                    serde_json::from_slice(&state_bytes).unwrap_or_default();
                let body = serde_json::json!({
                    "entity_type": entity_type,
                    "entity_id": key_str,
                    "status": actor_state.status,
                    "counters": actor_state.counters,
                    "booleans": actor_state.booleans,
                    "lists": actor_state.lists,
                    "fields": actor_state.fields,
                    "@odata.context": format!("$metadata#{set_name}/$entity"),
                    "@odata.id": format!("{set_name}('{key_str}')"),
                });
                if let Err(response) = authorize_read(
                    state,
                    tenant,
                    security_ctx,
                    READ_ACTION,
                    &entity_type,
                    &key_str,
                    &body,
                ) {
                    return *response;
                }
                ODataResponse {
                    status: StatusCode::OK,
                    body,
                }
                .into_response()
            }
            Ok(None) => odata_error(
                StatusCode::NOT_FOUND,
                "ResourceNotFound",
                &format!("Entity '{set_name}' with key '{key_str}' not found"),
            )
            .into_response(),
            Err(e) => odata_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "ActorReadError",
                &e.to_string(),
            )
            .into_response(),
        };
    }

    match build_entity_body(
        state,
        tenant,
        &entity_type,
        set_name,
        &key_str,
        security_ctx,
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
    security_ctx: &SecurityContext,
    parent: &ODataPath,
    property: &str,
    query_options: &QueryOptions,
) -> axum::response::Response {
    let (parent_type, parent_key, parent_set) =
        match resolve_parent_entity(parent, state, tenant, security_ctx).await {
            Ok(r) => r,
            Err((status, msg)) => {
                return odata_error(status, "InvalidPath", &msg).into_response();
            }
        };

    let parent_body = match load_authorized_entity_body(
        state,
        tenant,
        &parent_type,
        &parent_set,
        &parent_key,
        security_ctx,
    )
    .await
    {
        Ok(b) => b,
        Err(resp) => return resp,
    };

    let mut parent_body = parent_body;
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

    if let Err(response) = expand_entity(
        &mut parent_body,
        &[expand_item],
        &parent_type,
        state,
        tenant,
        security_ctx,
    )
    .await
    {
        return response;
    }

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
    security_ctx: &SecurityContext,
    parent: &ODataPath,
    property: &str,
    key: &temper_odata::path::KeyValue,
    query_options: &QueryOptions,
) -> axum::response::Response {
    let (parent_type, parent_key, parent_set) =
        match resolve_parent_entity(parent, state, tenant, security_ctx).await {
            Ok(r) => r,
            Err((status, msg)) => {
                return odata_error(status, "InvalidPath", &msg).into_response();
            }
        };
    if let Err(response) = load_authorized_entity_body(
        state,
        tenant,
        &parent_type,
        &parent_set,
        &parent_key,
        security_ctx,
    )
    .await
    {
        return response;
    }

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
        security_ctx,
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
    security_ctx: &SecurityContext,
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
        security_ctx,
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
    resolved_id: Option<Extension<ResolvedIdentity>>,
    headers: HeaderMap,
    axum::extract::Path(path): axum::extract::Path<String>,
    Query(query_params): Query<std::collections::BTreeMap<String, String>>,
) -> impl IntoResponse {
    let tenant = match extract_tenant(&headers, &state) {
        Ok(t) => t,
        Err(e) => return e.into_response(),
    };
    log_directed_evolution_odata_request(&headers, &tenant, "GET", &path);
    let agent_ctx = extract_agent_context(&headers);
    let resolved_identity = resolved_id.map(|Extension(identity)| identity);
    let security_ctx = request_security_context(&headers, &agent_ctx, resolved_identity.as_ref());
    handle_odata_get_for_tenant(state, tenant, security_ctx, path, query_params).await
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
    let hints = state
        .agent_hints
        .read()
        .expect("agent hints lock should not be poisoned")
        .clone();
    ODataResponse {
        status: StatusCode::OK,
        body: serde_json::to_value(&hints).unwrap_or_default(),
    }
}

/// Handle GET on `$value` — return binary content for stream-backed entities.
///
/// Flow:
/// 1. Resolve parent entity from ODataPath
/// 2. Verify entity type has `HasStream=true` in CSDL
/// 3. For TemperFS File/FileVersion, read the projection-backed blob pointer and
///    fetch bytes directly from blob storage.
/// 4. Fall back to actor materialization + WASM blob_adapter only when the
///    projection is missing, so older in-memory/test paths still work.
#[instrument(skip_all, fields(otel.name = "GET $value"))]
async fn handle_stream_get(
    state: &ServerState,
    tenant: &TenantId,
    security_ctx: &SecurityContext,
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

    let entity_state = match load_authorized_entity_body(
        state,
        tenant,
        &entity_type,
        &set_name,
        &key,
        security_ctx,
    )
    .await
    {
        Ok(body) => body,
        Err(resp) => return resp,
    };

    if let Some(response) =
        try_file_stream_fast_path(state, tenant, &set_name, &entity_type, &key).await
    {
        return response;
    }

    // 3. Check if entity has content (boolean may be in top-level `booleans` map or `fields`)
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

    // 4. Invoke WASM blob_adapter for download
    let response_stream_id = format!("download-{}", temper_runtime::scheduler::sim_uuid());
    let streams = Arc::new(RwLock::new(StreamRegistry::default()));

    let inv_ctx = WasmInvocationContext {
        tenant: tenant.to_string(),
        entity_type: entity_type.clone(),
        entity_id: key.clone(),
        trigger_action: "StreamDownload".to_string(),
        wasm_module: Some("blob_adapter".to_string()),
        trigger_params: serde_json::json!({
            "stream_id": response_stream_id,
            "operation": "get",
        }),
        entity_state: entity_state.clone(),
        agent_id: None,
        session_id: None,
        integration_config: std::collections::BTreeMap::new(),
        trace_id: String::new(),
        workflow_root_entity_type: None,
        workflow_root_entity_id: None,
        workflow_run_id: None,
        http_request: None,
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

    // 5. Read bytes from StreamRegistry
    let body_bytes = {
        let mut s = streams
            .write()
            .expect("stream registry lock should not be poisoned"); // ci-ok: infallible lock
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
