//! OData query option evaluation against entity state collections.
//!
//! Applies `$filter`, `$select`, `$orderby`, `$top`, `$skip`, and `$count`
//! to in-memory entity result sets. Uses the parsed AST from `temper-odata`.

use axum::response::IntoResponse;
use temper_odata::query::types::{
    BinaryOperator, FilterExpr, ODataValue, OrderByClause, OrderDirection, QueryOptions,
};
use temper_runtime::persistence::schema_deployment::SchemaExecutionPin;

use crate::blobs::{BlobHydrationBudget, hydrate_blob_refs_for_tenant_with_budget};

/// Maximum nesting depth for recursive $expand (prevents infinite loops).
const MAX_EXPAND_DEPTH: u8 = 3;

/// Apply all query options to a collection of entity JSON values.
///
/// Order of operations follows OData v4 spec:
/// 1. $filter — reduce the set
/// 2. $orderby — sort
/// 3. $skip — offset
/// 4. $top — limit
/// 5. $select — prune fields (applied last to preserve sort/filter keys)
pub fn apply_query_options(
    entities: Vec<serde_json::Value>,
    options: &QueryOptions,
) -> (Vec<serde_json::Value>, Option<usize>) {
    let mut result = entities;

    // 1. $filter
    if let Some(filter) = &options.filter {
        result = filter_entities(result, filter);
    }

    // Count after filter but before pagination
    let count = if options.count == Some(true) {
        Some(result.len())
    } else {
        None
    };

    // 2. $orderby
    if let Some(orderby) = &options.orderby {
        sort_entities(&mut result, orderby);
    }

    // 3. $skip
    if let Some(skip) = options.skip {
        result = result.into_iter().skip(skip).collect();
    }

    // 4. $top
    if let Some(top) = options.top {
        result = result.into_iter().take(top).collect();
    }

    // 5. $select
    if let Some(select) = &options.select {
        result = select_fields(result, select);
    }

    (result, count)
}

/// Filter entities by evaluating a `FilterExpr` against each entity.
fn filter_entities(
    entities: Vec<serde_json::Value>,
    filter: &FilterExpr,
) -> Vec<serde_json::Value> {
    entities
        .into_iter()
        .filter(|entity| evaluate_filter(entity, filter).unwrap_or(false))
        .collect()
}

/// Evaluate a filter expression against a single entity, returning a bool.
fn evaluate_filter(entity: &serde_json::Value, expr: &FilterExpr) -> Option<bool> {
    match expr {
        FilterExpr::BinaryOp { left, op, right } => {
            match op {
                BinaryOperator::And => {
                    let l = evaluate_filter(entity, left)?;
                    let r = evaluate_filter(entity, right)?;
                    Some(l && r)
                }
                BinaryOperator::Or => {
                    let l = evaluate_filter(entity, left)?;
                    let r = evaluate_filter(entity, right)?;
                    Some(l || r)
                }
                _ => {
                    // Comparison operators, with OData/SQL null semantics for the
                    // operator→null mapping of the native pushdown (`filter_sql.rs`):
                    // `prop eq null` → IS NULL, which MUST match a row that omits the
                    // property (a Directory root has no `ParentId`); `prop ne null` → IS
                    // NOT NULL; any other comparison touching null is UNKNOWN → excludes.
                    //
                    // Only a property (absent → NULL) or a literal is a valid comparison
                    // operand. An operand that cannot be evaluated (e.g. an unsupported
                    // function call) leaves the comparison undefined → exclude the row,
                    // rather than mistaking it for NULL.
                    //
                    // ARN-68: previously an absent property made `evaluate_value` return
                    // `None`, and the `?` collapsed the ENTIRE filter to `None` →
                    // `unwrap_or(false)`. So a root lookup `Name eq '/' and WorkspaceId
                    // eq '..' and ParentId eq null` dropped every root, and `ensure_dirs`
                    // recreated the root on every write — the duplicate-root bug.
                    match (
                        comparison_operand(entity, left),
                        comparison_operand(entity, right),
                    ) {
                        (Some(left_val), Some(right_val)) => {
                            Some(compare_nullable(left_val.as_ref(), right_val.as_ref(), op))
                        }
                        _ => Some(false),
                    }
                }
            }
        }
        FilterExpr::UnaryOp { op: _, operand } => {
            // Only "not" operator
            let val = evaluate_filter(entity, operand)?;
            Some(!val)
        }
        FilterExpr::FunctionCall { name, args } => evaluate_function(entity, name, args),
        // A bare property or literal used as boolean
        FilterExpr::Property(prop) => resolve_property(entity, prop).and_then(|v| v.as_bool()),
        FilterExpr::Literal(ODataValue::Boolean(b)) => Some(*b),
        _ => None,
    }
}

/// Evaluate a filter expression to a JSON value (for comparison).
fn evaluate_value(entity: &serde_json::Value, expr: &FilterExpr) -> Option<serde_json::Value> {
    match expr {
        FilterExpr::Property(prop) => resolve_property(entity, prop),
        FilterExpr::Literal(val) => Some(odata_value_to_json(val)),
        _ => None,
    }
}

/// Resolve a property name against an entity, checking top-level first,
/// then falling back to the `fields` sub-object.
///
/// `pub(crate)` so server-driven paging builds a keyset cursor from the same
/// property values the `$orderby` sort compares — the cursor and the sort must
/// resolve a property identically or a continuation could skip or repeat rows.
pub(crate) fn resolve_property(
    entity: &serde_json::Value,
    prop: &str,
) -> Option<serde_json::Value> {
    if prop == "Status" {
        return entity
            .get("Status")
            .or_else(|| entity.get("status"))
            .cloned()
            .or_else(|| {
                entity.get("fields").and_then(|fields| {
                    fields
                        .get("Status")
                        .or_else(|| fields.get("status"))
                        .cloned()
                })
            });
    }
    if prop == "status" {
        return entity
            .get("status")
            .or_else(|| entity.get("Status"))
            .cloned()
            .or_else(|| {
                entity.get("fields").and_then(|fields| {
                    fields
                        .get("status")
                        .or_else(|| fields.get("Status"))
                        .cloned()
                })
            });
    }

    entity
        .get(prop)
        .cloned()
        .or_else(|| entity.get("fields").and_then(|f| f.get(prop)).cloned())
}

/// Convert an OData literal to a serde_json::Value.
fn odata_value_to_json(val: &ODataValue) -> serde_json::Value {
    match val {
        ODataValue::Null => serde_json::Value::Null,
        ODataValue::Boolean(b) => serde_json::Value::Bool(*b),
        ODataValue::Int(i) => serde_json::json!(i),
        ODataValue::Float(f) => serde_json::json!(f),
        ODataValue::String(s) => serde_json::Value::String(s.clone()),
        ODataValue::Guid(g) => serde_json::Value::String(g.to_string()),
        ODataValue::DateTimeOffset(dt) => serde_json::Value::String(dt.to_rfc3339()),
    }
}

/// Compare two JSON values with a binary operator.
fn compare_values(
    left: &serde_json::Value,
    right: &serde_json::Value,
    op: &BinaryOperator,
) -> bool {
    match op {
        BinaryOperator::Eq => json_eq(left, right),
        BinaryOperator::Ne => !json_eq(left, right),
        BinaryOperator::Gt => {
            json_cmp(left, right).is_some_and(|o| o == std::cmp::Ordering::Greater)
        }
        BinaryOperator::Ge => json_cmp(left, right).is_some_and(|o| o != std::cmp::Ordering::Less),
        BinaryOperator::Lt => json_cmp(left, right).is_some_and(|o| o == std::cmp::Ordering::Less),
        BinaryOperator::Le => {
            json_cmp(left, right).is_some_and(|o| o != std::cmp::Ordering::Greater)
        }
        _ => false, // And/Or/Has handled above
    }
}

/// Check equality between two JSON values, coercing types where reasonable.
fn json_eq(left: &serde_json::Value, right: &serde_json::Value) -> bool {
    match (left, right) {
        (serde_json::Value::String(a), serde_json::Value::String(b)) => a == b,
        (serde_json::Value::Number(a), serde_json::Value::Number(b)) => a.as_f64() == b.as_f64(),
        (serde_json::Value::Bool(a), serde_json::Value::Bool(b)) => a == b,
        (serde_json::Value::Null, serde_json::Value::Null) => true,
        _ => left == right,
    }
}

/// Resolve one comparison operand to its NULL-aware value:
/// - `Some(Some(v))` — a present property value or a literal,
/// - `Some(None)` — a property that is absent/omitted, which is SQL NULL,
/// - `None` — not a comparison operand we can evaluate (e.g. an unsupported function
///   call); the caller excludes the row instead of treating it as NULL.
fn comparison_operand(
    entity: &serde_json::Value,
    expr: &FilterExpr,
) -> Option<Option<serde_json::Value>> {
    match expr {
        FilterExpr::Property(prop) => Some(resolve_property(entity, prop)),
        FilterExpr::Literal(val) => Some(Some(odata_value_to_json(val))),
        _ => None,
    }
}

/// Compare two operands with OData/SQL null semantics for the operator→null mapping of
/// the native pushdown in `filter_sql.rs`. An absent property (`None`) or JSON `null`
/// is treated as SQL NULL:
/// - `eq`: NULL eq NULL → true (IS NULL); NULL eq value → false; else value equality.
/// - `ne`: value ne NULL → true, NULL ne NULL → false (IS NOT NULL); NULL ne value →
///   false (UNKNOWN → excluded); else value inequality.
/// - ordering: any NULL operand → false (UNKNOWN → excluded).
///
/// Note this treats an *absent* property the same as an *explicit* JSON null, which is
/// intentionally broader than the SQL field index: the index writes a row only for an
/// explicit null (`filter_sql.rs` / turso `indexed_projection_fields`), so native
/// `field_value IS NULL` matches explicit nulls but not absent properties. The two
/// paths agree only because null-equality is non-lossless (`lossless_eq_comparison`
/// returns false for null), so `prop eq null` is never SQL-pushed and this in-memory
/// eval is authoritative. If null-eq is ever made lossless, the index must first index
/// absent-as-null for the paths to stay consistent.
fn compare_nullable(
    left: Option<&serde_json::Value>,
    right: Option<&serde_json::Value>,
    op: &BinaryOperator,
) -> bool {
    // Normalize each operand: an absent operand (`None`) or a JSON `null` becomes SQL
    // NULL (`None`); a real value stays `Some`. Matching on the normalized pair avoids
    // any `unwrap`.
    let left = left.filter(|value| !value.is_null());
    let right = right.filter(|value| !value.is_null());
    match (op, left, right) {
        // `eq`: IS NULL when both are null; value equality when both present; else false.
        (BinaryOperator::Eq, None, None) => true,
        (BinaryOperator::Eq, Some(l), Some(r)) => json_eq(l, r),
        (BinaryOperator::Eq, _, _) => false,
        // `ne`: IS NOT NULL — `value ne null` → true, `null ne null` → false; value
        // inequality when both present; `null ne value` is UNKNOWN → excluded.
        (BinaryOperator::Ne, Some(l), Some(r)) => !json_eq(l, r),
        (BinaryOperator::Ne, Some(_), None) => true,
        (BinaryOperator::Ne, _, _) => false,
        // ordering: both operands must be present (any NULL → UNKNOWN → excluded).
        (_, Some(l), Some(r)) => compare_values(l, r, op),
        (_, _, _) => false,
    }
}

/// Compare two JSON values, returning an ordering if they're comparable.
fn json_cmp(left: &serde_json::Value, right: &serde_json::Value) -> Option<std::cmp::Ordering> {
    match (left, right) {
        (serde_json::Value::Number(a), serde_json::Value::Number(b)) => {
            let af = a.as_f64()?;
            let bf = b.as_f64()?;
            af.partial_cmp(&bf)
        }
        (serde_json::Value::String(a), serde_json::Value::String(b)) => Some(a.cmp(b)),
        _ => None,
    }
}

/// Evaluate built-in OData filter functions.
fn evaluate_function(entity: &serde_json::Value, name: &str, args: &[FilterExpr]) -> Option<bool> {
    match name {
        "contains" if args.len() == 2 => {
            let haystack = evaluate_value(entity, &args[0])?.as_str()?.to_string();
            let needle = evaluate_value(entity, &args[1])?.as_str()?.to_string();
            Some(haystack.contains(&needle))
        }
        "startswith" if args.len() == 2 => {
            let s = evaluate_value(entity, &args[0])?.as_str()?.to_string();
            let prefix = evaluate_value(entity, &args[1])?.as_str()?.to_string();
            Some(s.starts_with(&prefix))
        }
        "endswith" if args.len() == 2 => {
            let s = evaluate_value(entity, &args[0])?.as_str()?.to_string();
            let suffix = evaluate_value(entity, &args[1])?.as_str()?.to_string();
            Some(s.ends_with(&suffix))
        }
        _ => None,
    }
}

/// Sort entities in place by the given orderby clauses.
fn sort_entities(entities: &mut [serde_json::Value], orderby: &[OrderByClause]) {
    entities.sort_by(|a, b| {
        for clause in orderby {
            let a_val = resolve_property(a, &clause.property);
            let b_val = resolve_property(b, &clause.property);
            let ordering = match (&a_val, &b_val) {
                (Some(av), Some(bv)) => json_cmp(av, bv).unwrap_or(std::cmp::Ordering::Equal),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            };
            let ordering = match clause.direction {
                OrderDirection::Asc => ordering,
                OrderDirection::Desc => ordering.reverse(),
            };
            if ordering != std::cmp::Ordering::Equal {
                return ordering;
            }
        }
        std::cmp::Ordering::Equal
    });
}

/// Prune each entity to only include the selected properties.
///
/// Resolves properties from both top-level and the `fields` sub-object.
pub fn select_fields(
    entities: Vec<serde_json::Value>,
    select: &[String],
) -> Vec<serde_json::Value> {
    entities
        .into_iter()
        .map(|entity| {
            let mut selected = serde_json::Map::new();
            for prop in select {
                if let Some(val) = resolve_property(&entity, prop) {
                    selected.insert(prop.clone(), val);
                }
            }
            // Always include OData annotations
            if let Some(obj) = entity.as_object() {
                for (k, v) in obj {
                    if k.starts_with('@') {
                        selected.insert(k.clone(), v.clone());
                    }
                }
            }
            serde_json::Value::Object(selected)
        })
        .collect()
}

/// How to resolve a foreign key for a navigation property.
enum FkResolution {
    /// Many-to-one: the source entity holds a FK pointing to the target.
    /// E.g., Order→Customer: Order.CustomerId → Customer.Id.
    Forward { source_field: String },
    /// One-to-many: target entities hold a FK pointing back to the source.
    /// E.g., Customer→Orders: Order.CustomerId points to Customer.Id.
    Reverse { target_fk_field: String },
}

/// Metadata about a navigation property needed for expansion.
struct NavExpansionInfo {
    target_type: String,
    is_collection: bool,
    /// RelationGraph-based FK resolution (None = fall back to convention scan).
    fk_resolution: Option<FkResolution>,
}

struct ExpansionContext<'a> {
    state: &'a crate::state::ServerState,
    tenant: &'a temper_runtime::tenant::TenantId,
    security_ctx: &'a temper_authz::SecurityContext,
    schema_pin: Option<&'a SchemaExecutionPin>,
    hydration_budget: &'a BlobHydrationBudget,
}

/// Resolve navigation properties for $expand on a single entity.
///
/// For each expand item, looks up the navigation property in the CSDL
/// to determine the target entity type, then queries related entities
/// (by convention: entities with a matching parent reference).
///
/// Supports nested $expand up to [`MAX_EXPAND_DEPTH`] levels with cycle detection.
pub async fn expand_entity(
    entity: &mut serde_json::Value,
    expand_items: &[temper_odata::query::types::ExpandItem],
    entity_type: &str,
    state: &crate::state::ServerState,
    tenant: &temper_runtime::tenant::TenantId,
    security_ctx: &temper_authz::SecurityContext,
    hydration_budget: &BlobHydrationBudget,
) -> Result<(), axum::response::Response> {
    let context = ExpansionContext {
        state,
        tenant,
        security_ctx,
        schema_pin: None,
        hydration_budget,
    };
    expand_entity_recursive(entity, expand_items, entity_type, &context, 0, &mut vec![]).await
}

/// Resolve `$expand` using one exact immutable scoped bundle.
#[expect(
    clippy::too_many_arguments,
    reason = "scoped expansion keeps authority, storage, pin, and hydration budget explicit"
)]
pub async fn expand_scoped_entity(
    entity: &mut serde_json::Value,
    expand_items: &[temper_odata::query::types::ExpandItem],
    entity_type: &str,
    state: &crate::state::ServerState,
    tenant: &temper_runtime::tenant::TenantId,
    security_ctx: &temper_authz::SecurityContext,
    hydration_budget: &BlobHydrationBudget,
    schema_pin: &SchemaExecutionPin,
) -> Result<(), axum::response::Response> {
    let context = ExpansionContext {
        state,
        tenant,
        security_ctx,
        schema_pin: Some(schema_pin),
        hydration_budget,
    };
    expand_entity_recursive(entity, expand_items, entity_type, &context, 0, &mut vec![]).await
}

/// Recursive implementation of $expand with depth and cycle guards.
async fn expand_entity_recursive(
    entity: &mut serde_json::Value,
    expand_items: &[temper_odata::query::types::ExpandItem],
    entity_type: &str,
    context: &ExpansionContext<'_>,
    depth: u8,
    visited: &mut Vec<String>,
) -> Result<(), axum::response::Response> {
    let ExpansionContext {
        state,
        tenant,
        security_ctx,
        schema_pin,
        hydration_budget,
    } = context;
    if depth >= MAX_EXPAND_DEPTH {
        return Ok(());
    }
    if visited.contains(&entity_type.to_string()) {
        return Ok(());
    }
    visited.push(entity_type.to_string());
    // Resolve all navigation targets up front (while holding registry lock briefly)
    let nav_infos: Vec<(
        &temper_odata::query::types::ExpandItem,
        Option<NavExpansionInfo>,
    )> = {
        let registry = state.registry.read().unwrap(); // ci-ok: infallible lock
        let tenant_config = match schema_pin {
            Some(pin) => {
                registry.get_scoped_config_at_digest(tenant, &pin.scope, &pin.bundle_digest)
            }
            None => registry.get_tenant(tenant),
        };
        expand_items
            .iter()
            .map(|item| {
                let target = tenant_config
                    .and_then(|tc| find_nav_target(&tc.csdl, entity_type, &item.property))
                    .or_else(|| {
                        schema_pin
                            .is_none()
                            .then(|| find_nav_target(&state.csdl, entity_type, &item.property))
                            .flatten()
                    });
                let info = target.map(|target_type| {
                    let is_collection = tenant_config
                        .is_some_and(|tc| is_collection_nav(&tc.csdl, entity_type, &item.property))
                        || (schema_pin.is_none()
                            && is_collection_nav(&state.csdl, entity_type, &item.property));
                    // Compute FK resolution from RelationGraph
                    let fk_resolution = tenant_config.and_then(|tc| {
                        find_fk_resolution(
                            &tc.relation_graph,
                            entity_type,
                            &target_type,
                            &item.property,
                            is_collection,
                        )
                    });
                    NavExpansionInfo {
                        target_type,
                        is_collection,
                        fk_resolution,
                    }
                });
                (item, info)
            })
            .collect()
    }; // Registry lock dropped here

    let entity_id = resolve_entity_id_with_fallback(entity);

    for (item, info) in &nav_infos {
        let Some(info) = info else { continue };
        let mut related_entities = Vec::new();

        if info.is_collection
            && let Err(response) = crate::odata::authz::authorize_read(
                state,
                tenant,
                security_ctx,
                crate::odata::authz::LIST_ACTION,
                &info.target_type,
                "",
                &serde_json::json!({}),
            )
        {
            return Err(*response);
        }

        if let Some(ref parent_id) = entity_id {
            match &info.fk_resolution {
                Some(FkResolution::Forward { source_field }) => {
                    // Many-to-one: entity holds FK to target.
                    // e.g., Order.CustomerId → fetch Customer by that ID directly.
                    let fk_value = entity
                        .get("fields")
                        .and_then(|f| f.get(source_field.as_str()))
                        .and_then(|v| v.as_str());
                    if let Some(fk) = fk_value {
                        let response = match schema_pin {
                            Some(pin) => {
                                state
                                    .get_scoped_entity_state(
                                        tenant,
                                        &info.target_type,
                                        fk,
                                        (*pin).clone(),
                                    )
                                    .await
                            }
                            None => {
                                state
                                    .get_tenant_entity_state(tenant, &info.target_type, fk)
                                    .await
                            }
                        };
                        if let Ok(response) = response {
                            let json = serde_json::to_value(&response.state).unwrap_or_default();
                            related_entities.push(json);
                        }
                    }
                }
                Some(FkResolution::Reverse { target_fk_field }) => {
                    // One-to-many: target entities hold FK back to us.
                    // e.g., Customer→Orders: filter Orders where CustomerId == parent_id.
                    let related_ids = expansion_entity_ids(context, &info.target_type).await?;
                    for related_id in &related_ids {
                        if let Ok(response) =
                            expansion_entity_state(context, &info.target_type, related_id).await
                        {
                            let json = serde_json::to_value(&response.state).unwrap_or_default();
                            let matches = json
                                .get("fields")
                                .and_then(|f| f.get(target_fk_field.as_str()))
                                .and_then(|v| v.as_str())
                                == Some(parent_id.as_str());
                            if matches {
                                related_entities.push(json);
                            }
                        }
                    }
                }
                None => {
                    // Fallback: convention scan (parentId / {EntityType}Id).
                    let related_ids = expansion_entity_ids(context, &info.target_type).await?;
                    for related_id in &related_ids {
                        if let Ok(response) =
                            expansion_entity_state(context, &info.target_type, related_id).await
                        {
                            let json = serde_json::to_value(&response.state).unwrap_or_default();
                            if matches_parent_reference(&json, entity_type, parent_id) {
                                related_entities.push(json);
                            }
                        }
                    }
                    if related_entities.is_empty() {
                        tracing::warn!(
                            entity_type,
                            nav_property = %item.property,
                            target_type = %info.target_type,
                            parent_id = %parent_id,
                            scanned = related_ids.len(),
                            "FK fallback scan found no related entities for $expand"
                        );
                    }
                }
            }
        }

        related_entities.retain(|entity| {
            crate::odata::authz::entity_id_from_body(entity).is_some_and(|entity_id| {
                crate::odata::authz::authorize_read(
                    state,
                    tenant,
                    security_ctx,
                    crate::odata::authz::READ_ACTION,
                    &info.target_type,
                    entity_id,
                    entity,
                )
                .is_ok()
            })
        });

        // Keep authorization ahead of object-store I/O. Relationship matching
        // and Cedar evaluation use descriptor metadata; only rows the caller
        // may read are allowed to consume the shared hydration budget.
        for entity in &mut related_entities {
            hydrate_blob_refs_for_tenant_with_budget(state, tenant, entity, hydration_budget).await;
        }

        // Apply nested query options if present
        if let Some(ref nested_opts) = item.options {
            let nested_query = QueryOptions {
                filter: nested_opts.filter.clone(),
                select: nested_opts.select.clone(),
                expand: nested_opts.expand.clone(),
                orderby: nested_opts.orderby.clone(),
                top: nested_opts.top,
                skip: nested_opts.skip,
                count: None,
                skiptoken: None,
            };
            let (filtered, _) = apply_query_options(related_entities, &nested_query);
            related_entities = filtered;
        }

        // Recursively expand nested $expand on related entities
        if let Some(ref nested_opts) = item.options
            && let Some(ref nested_expand) = nested_opts.expand
        {
            for related in &mut related_entities {
                Box::pin(expand_entity_recursive(
                    related,
                    nested_expand,
                    &info.target_type,
                    context,
                    depth + 1,
                    visited,
                ))
                .await?;
            }
        }

        if let Some(obj) = entity.as_object_mut() {
            if info.is_collection {
                obj.insert(item.property.clone(), serde_json::json!(related_entities));
            } else {
                obj.insert(
                    item.property.clone(),
                    related_entities
                        .into_iter()
                        .next()
                        .unwrap_or(serde_json::Value::Null),
                );
            }
        }
    }
    visited.pop();
    Ok(())
}

const SCOPED_EXPAND_SCAN_BUDGET: usize = 1_000;

async fn expansion_entity_ids(
    context: &ExpansionContext<'_>,
    entity_type: &str,
) -> Result<Vec<String>, axum::response::Response> {
    let Some(pin) = context.schema_pin else {
        return Ok(context.state.list_entity_ids(context.tenant, entity_type));
    };
    let types = vec![entity_type.to_string()];
    let rows = context
        .state
        .page_scoped_entity_ids(
            context.tenant,
            &types,
            pin,
            None,
            SCOPED_EXPAND_SCAN_BUDGET + 1,
        )
        .await
        .map_err(|error| {
            crate::response::odata_error(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "ScopedReadFailed",
                &error,
            )
            .into_response()
        })?;
    if rows.len() > SCOPED_EXPAND_SCAN_BUDGET {
        return Err(crate::response::odata_error(
            axum::http::StatusCode::PAYLOAD_TOO_LARGE,
            "ScopedExpandBudgetExceeded",
            "Scoped navigation expansion exceeded its entity scan budget",
        )
        .into_response());
    }
    Ok(rows.into_iter().map(|(_, entity_id)| entity_id).collect())
}

async fn expansion_entity_state(
    context: &ExpansionContext<'_>,
    entity_type: &str,
    entity_id: &str,
) -> Result<crate::entity_actor::EntityResponse, String> {
    match context.schema_pin {
        Some(pin) => {
            context
                .state
                .get_scoped_entity_state(context.tenant, entity_type, entity_id, (*pin).clone())
                .await
        }
        None => {
            context
                .state
                .get_tenant_entity_state(context.tenant, entity_type, entity_id)
                .await
        }
    }
}

/// Resolve an entity's id from the top-level `entity_id` field, falling back
/// to `fields.Id`.
fn resolve_entity_id_with_fallback(entity: &serde_json::Value) -> Option<String> {
    entity
        .get("entity_id")
        .and_then(|v| v.as_str())
        .or_else(|| {
            entity
                .get("fields")
                .and_then(|f| f.get("Id"))
                .and_then(|v| v.as_str())
        })
        .map(String::from)
}

/// Convention-based parent match for the FK fallback scan: true when the
/// candidate's `fields.parentId` or `fields.{EntityType}Id` equals `parent_id`.
fn matches_parent_reference(
    candidate: &serde_json::Value,
    entity_type: &str,
    parent_id: &str,
) -> bool {
    candidate
        .get("fields")
        .and_then(|f| f.as_object())
        .is_some_and(|fields| {
            let parent_id_field = format!("{}Id", entity_type);
            fields.get("parentId").and_then(|v| v.as_str()) == Some(parent_id)
                || fields.get(&parent_id_field).and_then(|v| v.as_str()) == Some(parent_id)
        })
}

/// Find the target entity type name for a navigation property.
pub(crate) fn find_nav_target(
    csdl: &temper_spec::csdl::CsdlDocument,
    entity_type: &str,
    nav_prop: &str,
) -> Option<String> {
    for schema in &csdl.schemas {
        if let Some(et) = schema.entity_type(entity_type)
            && let Some(np) = et.navigation_properties.iter().find(|n| n.name == nav_prop)
        {
            // Type name is like "Collection(Namespace.EntityType)" or "Namespace.EntityType"
            let type_name = np.type_name.trim();
            let inner = if type_name.starts_with("Collection(") && type_name.ends_with(')') {
                &type_name[11..type_name.len() - 1]
            } else {
                type_name
            };
            // Extract simple name from qualified name
            return Some(inner.rsplit('.').next().unwrap_or(inner).to_string());
        }
    }
    None
}

/// Determine FK resolution strategy from the [`RelationGraph`].
///
/// For non-collection nav (many-to-one), finds an outgoing edge from the source
/// entity matching the nav property. For collection nav (one-to-many), finds an
/// outgoing edge from the target entity that points back to the source.
fn find_fk_resolution(
    graph: &crate::registry::RelationGraph,
    entity_type: &str,
    target_type: &str,
    nav_property: &str,
    is_collection: bool,
) -> Option<FkResolution> {
    if !is_collection {
        // Many-to-one: look for an outgoing edge from entity_type matching the nav property.
        if let Some(edges) = graph.outgoing.get(entity_type) {
            for edge in edges {
                if edge.navigation_property == nav_property && edge.to_entity == target_type {
                    return Some(FkResolution::Forward {
                        source_field: edge.source_field.clone(),
                    });
                }
            }
        }
    } else {
        // One-to-many: look for an outgoing edge from target_type pointing back to entity_type.
        if let Some(edges) = graph.outgoing.get(target_type) {
            for edge in edges {
                if edge.to_entity == entity_type {
                    return Some(FkResolution::Reverse {
                        target_fk_field: edge.source_field.clone(),
                    });
                }
            }
        }
    }
    None
}

/// Check if a navigation property is a collection type.
fn is_collection_nav(
    csdl: &temper_spec::csdl::CsdlDocument,
    entity_type: &str,
    nav_prop: &str,
) -> bool {
    for schema in &csdl.schemas {
        if let Some(et) = schema.entity_type(entity_type)
            && let Some(np) = et.navigation_properties.iter().find(|n| n.name == nav_prop)
        {
            return np.type_name.starts_with("Collection(");
        }
    }
    false
}

#[cfg(test)]
#[path = "query_eval_test.rs"]
mod tests;
