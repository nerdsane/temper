//! OData read handlers (`GET` and metadata/service endpoints).

use std::sync::{Arc, RwLock};

use axum::extract::{Extension, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use temper_authz::{AuthenticatedRequestContext, SecurityContext};
use temper_odata::path::{KeyValue, ODataPath, parse_path};
use temper_odata::query::parse_query_options;
use temper_odata::query::types::{
    BinaryOperator, ExpandItem, ExpandOptions, FilterExpr, ODataValue, QueryOptions,
};
use temper_runtime::persistence::schema_deployment::SchemaExecutionPin;
use temper_runtime::tenant::TenantId;
use temper_wasm::{StreamRegistry, WasmInvocationContext};
use tracing::instrument;

use super::authz::{READ_ACTION, authorize_read, require_authenticated_context};
use super::blob_media::handle_blob_primitive_stream;
use super::common::{
    check_has_stream_or_400, extract_key, extract_schema_pin, extract_tenant, has_expand_options,
    resolve_entity_type, resolve_entity_type_for_pin, resolve_value_parent, tenant_csdl_xml,
    tenant_csdl_xml_for_pin, tenant_entity_sets_for_pin,
};
use super::filter_sql;
use super::query_plane_read::{
    QueryPlaneReadBudget, QueryPlaneReadRequest, read_entity_set_from_query_plane,
};
use super::read_support::{record_entity_set_not_found, try_load_entity_body_from_catalog};
use super::response::annotate_entity;
use super::schema_pin::{schema_pin_extraction_error_response, schema_pin_mismatch_response};
use super::stream_fast_path::try_file_stream_fast_path;
use crate::blobs::{BlobHydrationBudget, hydrate_blob_refs_for_tenant_with_budget};
use crate::query_eval::{apply_query_options, expand_entity, expand_scoped_entity, select_fields};
use crate::response::{ODataResponse, ODataStreamResponse, ODataXmlResponse, odata_error};
use crate::state::{ServerState, validate_global_entity_id};
use crate::storage::{QueryFieldIndexOrder, QueryFieldIndexOrderDirection};

/// Recursively resolve an OData path to its parent entity's
/// (entity_type, entity_id, entity_set_name).
///
/// Walks the path chain from Entity through NavigationProperty
/// and NavigationEntity nodes, resolving each hop via the RelationGraph.
struct ResolvedParentEntity {
    entity_type: String,
    entity_id: String,
    entity_set: String,
    schema_pin: Option<SchemaExecutionPin>,
}

async fn resolve_parent_entity(
    path: &ODataPath,
    state: &ServerState,
    tenant: &TenantId,
    security_ctx: &SecurityContext,
    schema_pin: Option<&SchemaExecutionPin>,
    resolve_durable_pin: bool,
    hydration_budget: &BlobHydrationBudget,
) -> Result<ResolvedParentEntity, (StatusCode, String)> {
    match path {
        ODataPath::Entity(set_name, key) => {
            let entity_type = resolve_entity_type_for_pin(state, tenant, schema_pin, set_name)
                .ok_or_else(|| {
                    (
                        StatusCode::NOT_FOUND,
                        format!("Entity set '{set_name}' not found"),
                    )
                })?;
            let key_str = extract_key(key);
            let resolved_pin = if resolve_durable_pin {
                match schema_pin {
                    Some(pin) => Some(
                        state
                            .resolve_scope_only_scoped_entity_pin(
                                tenant,
                                &entity_type,
                                &key_str,
                                pin.clone(),
                            )
                            .await
                            .map_err(|error| (StatusCode::CONFLICT, error))?,
                    ),
                    None => None,
                }
            } else {
                schema_pin.cloned()
            };
            Ok(ResolvedParentEntity {
                entity_type,
                entity_id: key_str,
                entity_set: set_name.clone(),
                schema_pin: resolved_pin,
            })
        }
        ODataPath::NavigationProperty { parent, property } => {
            let resolved_parent = Box::pin(resolve_parent_entity(
                parent,
                state,
                tenant,
                security_ctx,
                schema_pin,
                resolve_durable_pin,
                hydration_budget,
            ))
            .await?;
            let parent_type = resolved_parent.entity_type;
            let parent_key = resolved_parent.entity_id;
            let parent_pin = resolved_parent.schema_pin;

            // Use expand to resolve the nav property
            let parent_set =
                resolve_entity_set_name_for_pin(state, tenant, parent_pin.as_ref(), &parent_type);
            let mut parent_body = load_authorized_entity_body_for_pin(
                state,
                tenant,
                &parent_type,
                &parent_set,
                &parent_key,
                security_ctx,
                parent_pin.as_ref(),
                hydration_budget,
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
            match parent_pin.as_ref() {
                Some(pin) => {
                    expand_scoped_entity(
                        &mut parent_body,
                        &[expand_item],
                        &parent_type,
                        state,
                        tenant,
                        security_ctx,
                        hydration_budget,
                        pin,
                    )
                    .await
                }
                None => {
                    expand_entity(
                        &mut parent_body,
                        &[expand_item],
                        &parent_type,
                        state,
                        tenant,
                        security_ctx,
                        hydration_budget,
                    )
                    .await
                }
            }
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
            let target_type = resolve_navigation_target_type(
                state,
                tenant,
                parent_pin.as_ref(),
                &parent_type,
                property,
            )?;

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

            let target_pin = if resolve_durable_pin {
                match schema_pin {
                    Some(pin) => Some(
                        state
                            .resolve_scope_only_scoped_entity_pin(
                                tenant,
                                &target_type,
                                &entity_id,
                                pin.clone(),
                            )
                            .await
                            .map_err(|error| (StatusCode::CONFLICT, error))?,
                    ),
                    None => None,
                }
            } else {
                parent_pin
            };
            let set_name =
                resolve_entity_set_name_for_pin(state, tenant, target_pin.as_ref(), &target_type);
            Ok(ResolvedParentEntity {
                entity_type: target_type,
                entity_id,
                entity_set: set_name,
                schema_pin: target_pin,
            })
        }
        ODataPath::NavigationEntity {
            parent,
            property,
            key,
        } => {
            // Resolve the parent, then the keyed entity in the nav collection
            let resolved_parent = Box::pin(resolve_parent_entity(
                parent,
                state,
                tenant,
                security_ctx,
                schema_pin,
                resolve_durable_pin,
                hydration_budget,
            ))
            .await?;
            let parent_type = resolved_parent.entity_type;
            let parent_pin = resolved_parent.schema_pin;

            let target_type = resolve_navigation_target_type(
                state,
                tenant,
                parent_pin.as_ref(),
                &parent_type,
                property,
            )?;

            let key_str = extract_key(key);
            let target_pin = if resolve_durable_pin {
                match schema_pin {
                    Some(pin) => Some(
                        state
                            .resolve_scope_only_scoped_entity_pin(
                                tenant,
                                &target_type,
                                &key_str,
                                pin.clone(),
                            )
                            .await
                            .map_err(|error| (StatusCode::CONFLICT, error))?,
                    ),
                    None => None,
                }
            } else {
                parent_pin
            };
            let set_name =
                resolve_entity_set_name_for_pin(state, tenant, target_pin.as_ref(), &target_type);
            Ok(ResolvedParentEntity {
                entity_type: target_type,
                entity_id: key_str,
                entity_set: set_name,
                schema_pin: target_pin,
            })
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
    schema_pin: Option<&SchemaExecutionPin>,
    parent_type: &str,
    property: &str,
) -> Result<String, (StatusCode, String)> {
    let registry = state
        .registry
        .read()
        .expect("registry lock should not be poisoned"); // ci-ok: infallible lock
    let tenant_config = match schema_pin {
        Some(pin) => registry.get_scoped_config_at_digest(tenant, &pin.scope, &pin.bundle_digest),
        None => registry.get_tenant(tenant),
    };
    tenant_config
        .and_then(|tc| crate::query_eval::find_nav_target(&tc.csdl, parent_type, property))
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                format!("Nav target for '{property}' not found"),
            )
        })
}

fn resolve_entity_set_name_for_pin(
    state: &ServerState,
    tenant: &TenantId,
    schema_pin: Option<&SchemaExecutionPin>,
    entity_type: &str,
) -> String {
    let registry = state.registry.read().expect("registry lock poisoned");
    let config = match schema_pin {
        Some(pin) => registry.get_scoped_config_at_digest(tenant, &pin.scope, &pin.bundle_digest),
        None => registry.get_tenant(tenant),
    };
    config
        .and_then(|config| {
            config
                .entity_set_map
                .iter()
                .find(|(_, found_type)| found_type.as_str() == entity_type)
                .map(|(set_name, _)| set_name.clone())
        })
        .unwrap_or_else(|| format!("{entity_type}s"))
}

fn service_document_body_for_pin(
    state: &ServerState,
    tenant: &TenantId,
    schema_pin: Option<&temper_runtime::persistence::schema_deployment::SchemaExecutionPin>,
) -> Option<serde_json::Value> {
    let entity_sets: Vec<serde_json::Value> =
        tenant_entity_sets_for_pin(state, tenant, schema_pin)?
            .iter()
            .map(|name| serde_json::json!({"name": name, "kind": "EntitySet", "url": name}))
            .collect();
    Some(serde_json::json!({"@odata.context": "$metadata", "value": entity_sets}))
}

fn service_document_body(state: &ServerState, tenant: &TenantId) -> serde_json::Value {
    service_document_body_for_pin(state, tenant, None)
        .expect("tenant-global service document is always available")
}

pub(super) async fn entity_set_not_found_response(
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

pub(super) async fn load_existing_entity_descriptor_body(
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
    if !state.entity_exists(tenant, entity_type, key)
        && !state.ensure_entity_loaded(tenant, entity_type, key).await
    {
        return Err(resource_not_found_response(set_name, key));
    }
    crate::application_data::GovernedApplicationDataService::new(state)
        .get(tenant, entity_type, key)
        .await
        .map(|response| serde_json::to_value(&response.state).unwrap_or_default())
        .map_err(|_| resource_not_found_response(set_name, key))
}

async fn load_authorized_entity_body(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    set_name: &str,
    key: &str,
    security_ctx: &SecurityContext,
    hydration_budget: &BlobHydrationBudget,
) -> Result<serde_json::Value, Response> {
    // Authorization precedes object-store reads. Overflow descriptors contain
    // the inline ownership/relationship metadata Cedar needs without granting
    // an unauthorized caller a storage-I/O amplification primitive.
    let mut body =
        load_existing_entity_descriptor_body(state, tenant, entity_type, set_name, key).await?;
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
    hydrate_blob_refs_for_tenant_with_budget(state, tenant, &mut body, hydration_budget).await;
    if let Some(object) = body.as_object_mut() {
        object.insert(
            "@odata.id".into(),
            serde_json::json!(format!("{set_name}('{key}')")),
        );
    }
    Ok(body)
}

#[expect(
    clippy::too_many_arguments,
    reason = "the scoped read boundary keeps authority, schema pin, and hydration budget explicit"
)]
async fn load_authorized_entity_body_for_pin(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    set_name: &str,
    key: &str,
    security_ctx: &SecurityContext,
    schema_pin: Option<&SchemaExecutionPin>,
    hydration_budget: &BlobHydrationBudget,
) -> Result<serde_json::Value, Response> {
    let Some(pin) = schema_pin else {
        return load_authorized_entity_body(
            state,
            tenant,
            entity_type,
            set_name,
            key,
            security_ctx,
            hydration_budget,
        )
        .await;
    };
    let response = state
        .get_scoped_entity_state(tenant, entity_type, key, pin.clone())
        .await
        .map_err(|error| {
            schema_pin_mismatch_response(&error)
                .unwrap_or_else(|| resource_not_found_response(set_name, key))
        })?;
    let mut body = serde_json::to_value(&response.state).unwrap_or_default();
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
    hydrate_blob_refs_for_tenant_with_budget(state, tenant, &mut body, hydration_budget).await;
    Ok(body)
}

fn composite_key_filter(key_pairs: &[(String, String)]) -> Option<FilterExpr> {
    let mut pairs = key_pairs.iter().filter(|(name, _)| !name.trim().is_empty());
    let (name, value) = pairs.next()?;
    let mut expr = FilterExpr::BinaryOp {
        left: Box::new(FilterExpr::Property(name.clone())),
        op: BinaryOperator::Eq,
        right: Box::new(FilterExpr::Literal(ODataValue::String(value.clone()))),
    };

    for (name, value) in pairs {
        expr = FilterExpr::BinaryOp {
            left: Box::new(expr),
            op: BinaryOperator::And,
            right: Box::new(FilterExpr::BinaryOp {
                left: Box::new(FilterExpr::Property(name.clone())),
                op: BinaryOperator::Eq,
                right: Box::new(FilterExpr::Literal(ODataValue::String(value.clone()))),
            }),
        };
    }
    Some(expr)
}

async fn try_resolve_composite_entity_key(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    key_pairs: &[(String, String)],
) -> Option<String> {
    // ADR-0153 fast path: if the key is a declared `[[key]]`, probe
    // `entity_key_index` (O(log n), present/absent) instead of the candidate
    // scan. On a miss we fall through to the scan, which still covers
    // pre-backfill entities — a safe additive fast path until #324's scan is
    // retired behind the backfill gate.
    // Composite-key URL addressing delivers string values; carry them typed so
    // `resolve_query_to_key` hashes them the same way the write side does.
    let typed_pairs: Vec<(String, serde_json::Value)> = key_pairs
        .iter()
        .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
        .collect();
    // Resolve declared keys via the registry-aware path (os-app entities live in
    // the per-tenant registry, not `transition_tables`).
    let keys = state.declared_keys_for(tenant, entity_type);
    if let Some((key_name, key_hash)) = crate::key_index::resolve_query_to_key(&keys, &typed_pairs)
        && let Some((store, _)) = state.event_journal()
        && let Ok(Some(entity_id)) = store
            .lookup_by_key(tenant.as_str(), entity_type, &key_name, &key_hash)
            .await
    {
        return Some(entity_id);
    }

    let filter = composite_key_filter(key_pairs)?;
    let translated = filter_sql::try_translate_candidate_filter(&filter)?;
    let order_by = [QueryFieldIndexOrder {
        target: crate::storage::QueryFieldIndexOrderTarget::EntityId,
        direction: QueryFieldIndexOrderDirection::Asc,
    }];
    let page = match crate::application_data::GovernedApplicationDataService::new(state)
        .query_index_page(
            tenant,
            entity_type,
            &translated.where_clause,
            translated.params,
            &order_by,
            0,
            2,
            false,
        )
        .await
    {
        Ok(Some(page)) => page,
        Ok(None) => return None,
        Err(error) => {
            tracing::warn!(
                error = %error,
                tenant = %tenant,
                entity_type,
                "composite OData key lookup failed; falling back to direct key"
            );
            return None;
        }
    };

    match page.entity_ids.as_slice() {
        [entity_id] => Some(entity_id.clone()),
        [] => None,
        _ => {
            tracing::warn!(
                tenant = %tenant,
                entity_type,
                match_count = page.entity_ids.len(),
                "composite OData key lookup was ambiguous; falling back to direct key"
            );
            None
        }
    }
}

async fn resolve_entity_request_key(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    key: &KeyValue,
) -> String {
    match key {
        KeyValue::Single(_) => extract_key(key),
        KeyValue::Composite(pairs) => {
            try_resolve_composite_entity_key(state, tenant, entity_type, pairs)
                .await
                .unwrap_or_else(|| extract_key(key))
        }
    }
}

#[derive(Clone, Copy)]
struct ReadContext<'a> {
    state: &'a ServerState,
    tenant: &'a TenantId,
    security: &'a SecurityContext,
    hydration: &'a BlobHydrationBudget,
}

impl<'a> ReadContext<'a> {
    fn new(
        state: &'a ServerState,
        tenant: &'a TenantId,
        security: &'a SecurityContext,
        hydration: &'a BlobHydrationBudget,
    ) -> Self {
        Self {
            state,
            tenant,
            security,
            hydration,
        }
    }
}

async fn apply_entity_query_options(
    mut body: serde_json::Value,
    entity_type: &str,
    context: ReadContext<'_>,
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
            context.state,
            context.tenant,
            context.security,
            context.hydration,
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
    context: ReadContext<'_>,
    entity_type: &str,
    set_name: &str,
    key: &str,
    options: EntityBodyOptions<'_>,
) -> Result<serde_json::Value, Response> {
    let mut state_json = load_authorized_entity_body(
        context.state,
        context.tenant,
        entity_type,
        set_name,
        key,
        context.security,
        context.hydration,
    )
    .await?;
    if let Some(obj) = state_json.as_object_mut() {
        obj.remove("@odata.id");
    }
    let mut body = annotate_entity(state_json, options.context, options.odata_id);

    if options.enrich {
        enrich_entity_response(
            &mut body,
            entity_type,
            set_name,
            key,
            context.state,
            context.tenant,
        );
    }

    if let Some(name) = options.function
        && let Some(obj) = body.as_object_mut()
    {
        obj.insert("@odata.function".to_string(), serde_json::json!(name));
    }

    apply_entity_query_options(
        body,
        entity_type,
        context,
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
    schema_pin: Option<SchemaExecutionPin>,
    scope_only_schema_pin: bool,
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
    let hydration_budget = BlobHydrationBudget::generic_response();

    match odata_path {
        ODataPath::Metadata if schema_pin.is_some() => {
            let pin = schema_pin.as_ref().expect("guarded above");
            match tenant_csdl_xml_for_pin(&state, &tenant, Some(pin)) {
                Some(body) => ODataXmlResponse { body }.into_response(),
                None => odata_error(
                    StatusCode::NOT_FOUND,
                    "ScopedSchemaNotFound",
                    "The pinned scoped schema bundle is unavailable",
                )
                .into_response(),
            }
        }

        ODataPath::ServiceDocument if schema_pin.is_some() => {
            let pin = schema_pin.as_ref().expect("guarded above");
            match service_document_body_for_pin(&state, &tenant, Some(pin)) {
                Some(body) => ODataResponse {
                    status: StatusCode::OK,
                    body,
                }
                .into_response(),
                None => odata_error(
                    StatusCode::NOT_FOUND,
                    "ScopedSchemaNotFound",
                    "The pinned scoped schema bundle is unavailable",
                )
                .into_response(),
            }
        }

        ODataPath::EntitySet(name) if schema_pin.is_some() => {
            handle_scoped_entity_set(
                &state,
                &tenant,
                &security_ctx,
                schema_pin.as_ref().expect("guarded above"),
                &name,
                &query_options,
                &hydration_budget,
            )
            .await
        }

        ODataPath::Entity(set_name, key) if schema_pin.is_some() => {
            handle_scoped_entity(
                &state,
                &tenant,
                &security_ctx,
                ScopedReadPin {
                    pin: schema_pin.as_ref().expect("guarded above"),
                    resolve_durable: scope_only_schema_pin,
                },
                &set_name,
                &key,
                &query_options,
                &hydration_budget,
            )
            .await
        }

        ODataPath::NavigationProperty {
            ref parent,
            ref property,
        } if schema_pin.is_some() => {
            handle_navigation_property(
                &state,
                &tenant,
                &security_ctx,
                NavigationReadPin {
                    pin: schema_pin.as_ref(),
                    resolve_durable: scope_only_schema_pin,
                },
                parent,
                property,
                &query_options,
                &hydration_budget,
            )
            .await
        }

        ODataPath::NavigationEntity {
            ref parent,
            ref property,
            ref key,
        } if schema_pin.is_some() => {
            handle_navigation_entity(
                &state,
                &tenant,
                &security_ctx,
                NavigationReadPin {
                    pin: schema_pin.as_ref(),
                    resolve_durable: scope_only_schema_pin,
                },
                parent,
                NavigationEntityTarget { property, key },
                &query_options,
                &hydration_budget,
            )
            .await
        }

        _ if schema_pin.is_some() => odata_error(
            StatusCode::NOT_IMPLEMENTED,
            "ScopedPathNotImplemented",
            "This scoped OData path is not implemented",
        )
        .into_response(),

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
            handle_entity_set(
                &state,
                &tenant,
                &security_ctx,
                &name,
                &query_options,
                &query_params,
                &hydration_budget,
            )
            .await
        }

        ODataPath::Entity(set_name, key) => {
            handle_entity(
                &state,
                &tenant,
                &security_ctx,
                &set_name,
                &key,
                &query_options,
                &hydration_budget,
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
                NavigationReadPin {
                    pin: None,
                    resolve_durable: false,
                },
                parent,
                property,
                &query_options,
                &hydration_budget,
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
                NavigationReadPin {
                    pin: None,
                    resolve_durable: false,
                },
                parent,
                NavigationEntityTarget { property, key },
                &query_options,
                &hydration_budget,
            )
            .await
        }

        ODataPath::BoundFunction {
            parent,
            function,
            params,
        } => {
            if function == "Temper.Nearest" {
                // ADR-0155: the collection-bound exact-scan kNN function, dispatched by
                // its fully-qualified name so a same-named function in another
                // namespace never routes here.
                super::nearest::handle_nearest(
                    &state,
                    &tenant,
                    &security_ctx,
                    &parent,
                    &params,
                    &query_options,
                )
                .await
            } else {
                handle_bound_function(
                    &state,
                    &tenant,
                    &security_ctx,
                    &parent,
                    &function,
                    &query_options,
                    &hydration_budget,
                )
                .await
            }
        }

        ODataPath::Value { ref parent } => {
            handle_stream_get(&state, &tenant, &security_ctx, parent, &hydration_budget).await
        }

        _ => odata_error(
            StatusCode::NOT_IMPLEMENTED,
            "NotImplemented",
            "This path pattern is not yet supported",
        )
        .into_response(),
    }
}

struct ScopedReadPin<'a> {
    pin: &'a SchemaExecutionPin,
    resolve_durable: bool,
}

#[expect(
    clippy::too_many_arguments,
    reason = "the scoped read boundary keeps authority, schema pin, and hydration budget explicit"
)]
async fn handle_scoped_entity(
    state: &ServerState,
    tenant: &TenantId,
    security_ctx: &SecurityContext,
    requested_pin: ScopedReadPin<'_>,
    set_name: &str,
    key: &KeyValue,
    query_options: &QueryOptions,
    hydration_budget: &BlobHydrationBudget,
) -> Response {
    let Some(entity_type) =
        resolve_entity_type_for_pin(state, tenant, Some(requested_pin.pin), set_name)
    else {
        return entity_set_not_found_response(state, tenant, set_name).await;
    };
    let key = extract_key(key);
    let schema_pin = if requested_pin.resolve_durable {
        match state
            .resolve_scope_only_scoped_entity_pin(
                tenant,
                &entity_type,
                &key,
                requested_pin.pin.clone(),
            )
            .await
        {
            Ok(pin) => pin,
            Err(error) => {
                return schema_pin_mismatch_response(&error).unwrap_or_else(|| {
                    odata_error(StatusCode::CONFLICT, "SchemaPinMismatch", &error).into_response()
                });
            }
        }
    } else {
        requested_pin.pin.clone()
    };
    let response = match state
        .get_scoped_entity_state(tenant, &entity_type, &key, schema_pin.clone())
        .await
    {
        Ok(response) => response,
        Err(error) => {
            if let Some(response) = schema_pin_mismatch_response(&error) {
                return response;
            }
            return odata_error(StatusCode::NOT_FOUND, "ResourceNotFound", &error).into_response();
        }
    };
    let mut body = serde_json::to_value(&response.state).unwrap_or_default();
    if let Err(response) = authorize_read(
        state,
        tenant,
        security_ctx,
        READ_ACTION,
        &entity_type,
        &key,
        &body,
    ) {
        return *response;
    }
    hydrate_blob_refs_for_tenant_with_budget(state, tenant, &mut body, hydration_budget).await;
    if let Some(expand) = query_options.expand.as_ref()
        && let Err(response) = expand_scoped_entity(
            &mut body,
            expand,
            &entity_type,
            state,
            tenant,
            security_ctx,
            hydration_budget,
            &schema_pin,
        )
        .await
    {
        return response;
    }
    body = annotate_entity(
        body,
        format!("$metadata#{set_name}/$entity"),
        Some(format!("{set_name}('{key}')")),
    );
    if let Some(select) = query_options.select.as_ref() {
        body = select_fields(vec![body], select).pop().unwrap_or_default();
    }
    ODataResponse {
        status: StatusCode::OK,
        body,
    }
    .into_response()
}

async fn handle_scoped_entity_set(
    state: &ServerState,
    tenant: &TenantId,
    security_ctx: &SecurityContext,
    schema_pin: &SchemaExecutionPin,
    set_name: &str,
    query_options: &QueryOptions,
    hydration_budget: &BlobHydrationBudget,
) -> Response {
    let Some(entity_type) = resolve_entity_type_for_pin(state, tenant, Some(schema_pin), set_name)
    else {
        return entity_set_not_found_response(state, tenant, set_name).await;
    };
    const SCOPED_COLLECTION_SCAN_BUDGET: usize = 1_000;
    let ids = match state
        .page_scoped_entity_ids(
            tenant,
            std::slice::from_ref(&entity_type),
            schema_pin,
            None,
            SCOPED_COLLECTION_SCAN_BUDGET + 1,
        )
        .await
    {
        Ok(ids) if ids.len() <= SCOPED_COLLECTION_SCAN_BUDGET => ids,
        Ok(_) => {
            return odata_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "ScopedQueryBudgetExceeded",
                "Scoped collection query exceeded its entity scan budget",
            )
            .into_response();
        }
        Err(error) => {
            return odata_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "ScopedReadFailed",
                &error,
            )
            .into_response();
        }
    };
    let mut entities = Vec::new();
    for (_, id) in ids {
        let response = match state
            .get_scoped_entity_state(tenant, &entity_type, &id, schema_pin.clone())
            .await
        {
            Ok(response) => response,
            Err(error) => {
                if let Some(response) = schema_pin_mismatch_response(&error) {
                    return response;
                }
                return odata_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "ScopedReadFailed",
                    &error,
                )
                .into_response();
            }
        };
        let mut body = serde_json::to_value(&response.state).unwrap_or_default();
        if authorize_read(
            state,
            tenant,
            security_ctx,
            READ_ACTION,
            &entity_type,
            &id,
            &body,
        )
        .is_err()
        {
            continue;
        }
        hydrate_blob_refs_for_tenant_with_budget(state, tenant, &mut body, hydration_budget).await;
        entities.push(body);
    }
    let bounded_options = QueryOptions {
        filter: query_options.filter.clone(),
        select: None,
        expand: None,
        orderby: query_options.orderby.clone(),
        top: Some(query_options.top.unwrap_or(100).min(100)),
        skip: query_options.skip,
        count: query_options.count,
        skiptoken: None,
    };
    let (mut entities, count) = apply_query_options(entities, &bounded_options);
    if let Some(expand) = query_options.expand.as_ref() {
        for entity in &mut entities {
            if let Err(response) = expand_scoped_entity(
                entity,
                expand,
                &entity_type,
                state,
                tenant,
                security_ctx,
                hydration_budget,
                schema_pin,
            )
            .await
            {
                return response;
            }
        }
    }
    if let Some(select) = query_options.select.as_ref() {
        entities = select_fields(entities, select);
    }
    for entity in &mut entities {
        let id = crate::odata::authz::entity_id_from_body(entity)
            .unwrap_or_default()
            .to_string();
        *entity = annotate_entity(
            std::mem::take(entity),
            format!("$metadata#{set_name}/$entity"),
            Some(format!("{set_name}('{id}')")),
        );
    }
    let mut body = serde_json::json!({
        "@odata.context": format!("$metadata#{set_name}"),
        "value": entities,
    });
    if let Some(count) = count {
        body["@odata.count"] = serde_json::json!(count);
    }
    ODataResponse {
        status: StatusCode::OK,
        body,
    }
    .into_response()
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
    query_params: &std::collections::BTreeMap<String, String>,
    hydration_budget: &BlobHydrationBudget,
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
    for entity in &mut result {
        hydrate_blob_refs_for_tenant_with_budget(state, tenant, entity, hydration_budget).await;
    }
    if let Some(ref expand_items) = query_options.expand {
        for entity in &mut result {
            if let Err(response) = expand_entity(
                entity,
                expand_items,
                &entity_type,
                state,
                tenant,
                security_ctx,
                hydration_budget,
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
    if let Some(token) = read_result.next_skiptoken {
        body["@odata.nextLink"] = serde_json::json!(next_link(name, query_params, &token));
    }
    ODataResponse {
        status: StatusCode::OK,
        body,
    }
    .into_response()
}

/// Build the `@odata.nextLink` for a truncated list read: the request's own
/// query options with `$skip`/`$skiptoken` replaced by the continuation token.
///
/// Values are percent-encoded so the link round-trips through the client and the
/// `Query` extractor back to the original options; the base64url token is already
/// URL-safe.
fn next_link(
    set_name: &str,
    query_params: &std::collections::BTreeMap<String, String>,
    token: &str,
) -> String {
    let mut pairs: Vec<String> = query_params
        .iter()
        .filter(|(key, _)| key.as_str() != "$skip" && key.as_str() != "$skiptoken")
        .map(|(key, value)| {
            format!(
                "{}={}",
                encode_query_component(key),
                encode_query_component(value)
            )
        })
        .collect();
    pairs.push(format!("$skiptoken={token}"));
    format!("{set_name}?{}", pairs.join("&"))
}

/// Percent-encode a query-string key or value, leaving the RFC 3986 unreserved
/// set (`A-Za-z0-9-._~`) intact. Over-encoding is safe; the server decodes it.
fn encode_query_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push_str(&format!("{byte:02X}"));
        }
    }
    out
}

/// Handle `Entity` path: fetch a single entity by key.
async fn handle_entity(
    state: &ServerState,
    tenant: &TenantId,
    security_ctx: &SecurityContext,
    set_name: &str,
    key: &temper_odata::path::KeyValue,
    query_options: &QueryOptions,
    hydration_budget: &BlobHydrationBudget,
) -> axum::response::Response {
    let entity_type = match resolve_entity_type(state, tenant, set_name) {
        Some(t) => t,
        None => return entity_set_not_found_response(state, tenant, set_name).await,
    };
    let key_str = resolve_entity_request_key(state, tenant, &entity_type, key).await;

    if state.is_pg_actor_backed(tenant, &entity_type)
        && let Some(actor_sys) = &state.pg_actor_system
    {
        if let Err(error) = validate_global_entity_id(&key_str) {
            return odata_error(StatusCode::BAD_REQUEST, "InvalidEntityId", &error).into_response();
        }
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
        ReadContext::new(state, tenant, security_ctx, hydration_budget),
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
struct NavigationReadPin<'a> {
    pin: Option<&'a SchemaExecutionPin>,
    resolve_durable: bool,
}

#[expect(
    clippy::too_many_arguments,
    reason = "navigation resolution keeps authority, schema pin, query, and hydration budget explicit"
)]
async fn handle_navigation_property(
    state: &ServerState,
    tenant: &TenantId,
    security_ctx: &SecurityContext,
    navigation_pin: NavigationReadPin<'_>,
    parent: &ODataPath,
    property: &str,
    query_options: &QueryOptions,
    hydration_budget: &BlobHydrationBudget,
) -> axum::response::Response {
    let NavigationReadPin {
        pin: schema_pin,
        resolve_durable: resolve_durable_pin,
    } = navigation_pin;
    let resolved_parent = match resolve_parent_entity(
        parent,
        state,
        tenant,
        security_ctx,
        schema_pin,
        resolve_durable_pin,
        hydration_budget,
    )
    .await
    {
        Ok(r) => r,
        Err((status, msg)) => {
            return odata_error(status, "InvalidPath", &msg).into_response();
        }
    };
    let parent_type = resolved_parent.entity_type;
    let parent_key = resolved_parent.entity_id;
    let parent_set = resolved_parent.entity_set;
    let parent_pin = resolved_parent.schema_pin;

    let parent_body = match load_authorized_entity_body_for_pin(
        state,
        tenant,
        &parent_type,
        &parent_set,
        &parent_key,
        security_ctx,
        parent_pin.as_ref(),
        hydration_budget,
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

    let expanded = match parent_pin.as_ref() {
        Some(pin) => {
            expand_scoped_entity(
                &mut parent_body,
                &[expand_item],
                &parent_type,
                state,
                tenant,
                security_ctx,
                hydration_budget,
                pin,
            )
            .await
        }
        None => {
            expand_entity(
                &mut parent_body,
                &[expand_item],
                &parent_type,
                state,
                tenant,
                security_ctx,
                hydration_budget,
            )
            .await
        }
    };
    if let Err(response) = expanded {
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
struct NavigationEntityTarget<'a> {
    property: &'a str,
    key: &'a temper_odata::path::KeyValue,
}

#[expect(
    clippy::too_many_arguments,
    reason = "navigation resolution keeps authority, schema pin, query, and hydration budget explicit"
)]
async fn handle_navigation_entity(
    state: &ServerState,
    tenant: &TenantId,
    security_ctx: &SecurityContext,
    navigation_pin: NavigationReadPin<'_>,
    parent: &ODataPath,
    target: NavigationEntityTarget<'_>,
    query_options: &QueryOptions,
    hydration_budget: &BlobHydrationBudget,
) -> axum::response::Response {
    let NavigationReadPin {
        pin: schema_pin,
        resolve_durable: resolve_durable_pin,
    } = navigation_pin;
    let NavigationEntityTarget { property, key } = target;
    let resolved_parent = match resolve_parent_entity(
        parent,
        state,
        tenant,
        security_ctx,
        schema_pin,
        resolve_durable_pin,
        hydration_budget,
    )
    .await
    {
        Ok(r) => r,
        Err((status, msg)) => {
            return odata_error(status, "InvalidPath", &msg).into_response();
        }
    };
    let parent_type = resolved_parent.entity_type;
    let parent_key = resolved_parent.entity_id;
    let parent_set = resolved_parent.entity_set;
    let parent_pin = resolved_parent.schema_pin;
    if let Err(response) = load_authorized_entity_body_for_pin(
        state,
        tenant,
        &parent_type,
        &parent_set,
        &parent_key,
        security_ctx,
        parent_pin.as_ref(),
        hydration_budget,
    )
    .await
    {
        return response;
    }

    let Ok(target_type) =
        resolve_navigation_target_type(state, tenant, parent_pin.as_ref(), &parent_type, property)
    else {
        return odata_error(
            StatusCode::NOT_FOUND,
            "NavigationPropertyNotFound",
            &format!("Navigation property '{property}' not found on '{parent_type}'"),
        )
        .into_response();
    };

    let key_str = extract_key(key);
    let target_pin = if resolve_durable_pin {
        match schema_pin {
            Some(pin) => match state
                .resolve_scope_only_scoped_entity_pin(tenant, &target_type, &key_str, pin.clone())
                .await
            {
                Ok(pin) => Some(pin),
                Err(error) => {
                    return odata_error(StatusCode::CONFLICT, "SchemaPinMismatch", &error)
                        .into_response();
                }
            },
            None => None,
        }
    } else {
        parent_pin
    };
    let target_set =
        resolve_entity_set_name_for_pin(state, tenant, target_pin.as_ref(), &target_type);

    if let Some(pin) = target_pin.as_ref() {
        return handle_scoped_entity(
            state,
            tenant,
            security_ctx,
            ScopedReadPin {
                pin,
                resolve_durable: false,
            },
            &target_set,
            key,
            query_options,
            hydration_budget,
        )
        .await;
    }

    let context = ReadContext::new(state, tenant, security_ctx, hydration_budget);

    match build_entity_body(
        context,
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
    security_ctx: &SecurityContext,
    parent: &ODataPath,
    function: &str,
    query_options: &QueryOptions,
    hydration_budget: &BlobHydrationBudget,
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
        ReadContext::new(state, tenant, security_ctx, hydration_budget),
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
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    headers: HeaderMap,
    axum::extract::Path(path): axum::extract::Path<String>,
    Query(query_params): Query<std::collections::BTreeMap<String, String>>,
) -> impl IntoResponse {
    let authenticated = match require_authenticated_context(authenticated) {
        Ok(context) => context,
        Err(error) => return error.into_response(),
    };
    let tenant = authenticated.tenant().clone();
    let security_ctx = authenticated.security_context().clone();
    let schema_pin = match extract_schema_pin(&headers, &state, &tenant).await {
        Ok(pin) => pin,
        Err(error) => return schema_pin_extraction_error_response(error),
    };
    let scope_only_schema_pin =
        schema_pin.is_some() && !headers.contains_key("x-temper-schema-bundle-digest");
    handle_odata_get_for_tenant(
        state,
        tenant,
        security_ctx,
        path,
        query_params,
        schema_pin,
        scope_only_schema_pin,
    )
    .await
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
    let schema_pin = match extract_schema_pin(&headers, &state, &tenant).await {
        Ok(pin) => pin,
        Err(error) => return schema_pin_extraction_error_response(error),
    };
    ODataResponse {
        status: StatusCode::OK,
        body: service_document_body_for_pin(&state, &tenant, schema_pin.as_ref())
            .unwrap_or_else(|| serde_json::json!({"error": "Scoped schema bundle not found"})),
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
    let schema_pin = match extract_schema_pin(&headers, &state, &tenant).await {
        Ok(pin) => pin,
        Err(error) => return schema_pin_extraction_error_response(error),
    };
    let Some(body) = tenant_csdl_xml_for_pin(&state, &tenant, schema_pin.as_ref()) else {
        return (StatusCode::CONFLICT, "Scoped schema bundle not found").into_response();
    };
    ODataXmlResponse { body }.into_response()
}

#[instrument(skip_all, fields(otel.name = "GET /odata/hints"))]
pub async fn handle_hints(
    State(state): State<ServerState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
) -> impl IntoResponse {
    let authenticated = match require_authenticated_context(authenticated) {
        Ok(context) => context,
        Err(error) => return error.into_response(),
    };
    let hints = state
        .agent_hints
        .read()
        .expect("agent hints lock should not be poisoned")
        .get(authenticated.tenant())
        .cloned()
        .unwrap_or_default();
    ODataResponse {
        status: StatusCode::OK,
        body: serde_json::to_value(&hints).unwrap_or_default(),
    }
    .into_response()
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
    hydration_budget: &BlobHydrationBudget,
) -> axum::response::Response {
    if let Some((set_name, key, property)) = resolve_blob_primitive_value_parent(parent) {
        return handle_blob_primitive_stream(
            state,
            tenant,
            security_ctx,
            &set_name,
            &key,
            &property,
        )
        .await;
    }

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
        hydration_budget,
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
        .invoke_wasm_direct(
            tenant,
            "blob_adapter",
            inv_ctx,
            streams.clone(),
            security_ctx,
        )
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

fn resolve_blob_primitive_value_parent(parent: &ODataPath) -> Option<(String, String, String)> {
    let ODataPath::NavigationProperty { parent, property } = parent else {
        return None;
    };
    let ODataPath::Entity(set_name, key) = parent.as_ref() else {
        return None;
    };
    Some((set_name.clone(), extract_key(key), property.clone()))
}

#[cfg(test)]
mod next_link_tests {
    use super::{encode_query_component, next_link};
    use std::collections::BTreeMap;

    #[test]
    fn query_component_encodes_reserved_but_not_unreserved() {
        assert_eq!(
            encode_query_component("Status eq 'Published'"),
            "Status%20eq%20%27Published%27"
        );
        assert_eq!(encode_query_component("Id-9._~"), "Id-9._~");
    }

    #[test]
    fn next_link_carries_options_and_replaces_paging() {
        let mut params = BTreeMap::new();
        params.insert("$filter".to_string(), "Status eq 'Published'".to_string());
        params.insert("$skip".to_string(), "100".to_string());
        params.insert("$skiptoken".to_string(), "STALE".to_string());
        let link = next_link("DesignLanguages", &params, "TOKEN-9");
        // $filter is preserved (percent-encoded); $skip and the old $skiptoken are dropped.
        assert!(link.starts_with("DesignLanguages?"));
        assert!(link.contains("%24filter=Status%20eq%20%27Published%27"));
        assert!(!link.contains("%24skip="));
        assert!(link.contains("$skiptoken=TOKEN-9"));
        assert_eq!(link.matches("skiptoken").count(), 1);
    }
}
