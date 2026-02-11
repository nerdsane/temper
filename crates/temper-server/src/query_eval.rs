//! OData query option evaluation against entity state collections.
//!
//! Applies `$filter`, `$select`, `$orderby`, `$top`, `$skip`, and `$count`
//! to in-memory entity result sets. Uses the parsed AST from `temper-odata`.

use temper_odata::query::types::{
    BinaryOperator, FilterExpr, ODataValue, OrderByClause, OrderDirection, QueryOptions,
};

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
fn filter_entities(entities: Vec<serde_json::Value>, filter: &FilterExpr) -> Vec<serde_json::Value> {
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
                    // Comparison operators
                    let left_val = evaluate_value(entity, left)?;
                    let right_val = evaluate_value(entity, right)?;
                    Some(compare_values(&left_val, &right_val, op))
                }
            }
        }
        FilterExpr::UnaryOp { op: _, operand } => {
            // Only "not" operator
            let val = evaluate_filter(entity, operand)?;
            Some(!val)
        }
        FilterExpr::FunctionCall { name, args } => {
            evaluate_function(entity, name, args)
        }
        // A bare property or literal used as boolean
        FilterExpr::Property(prop) => {
            resolve_property(entity, prop).and_then(|v| v.as_bool())
        }
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
fn resolve_property(entity: &serde_json::Value, prop: &str) -> Option<serde_json::Value> {
    entity.get(prop).cloned()
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
fn compare_values(left: &serde_json::Value, right: &serde_json::Value, op: &BinaryOperator) -> bool {
    match op {
        BinaryOperator::Eq => json_eq(left, right),
        BinaryOperator::Ne => !json_eq(left, right),
        BinaryOperator::Gt => json_cmp(left, right).is_some_and(|o| o == std::cmp::Ordering::Greater),
        BinaryOperator::Ge => json_cmp(left, right).is_some_and(|o| o != std::cmp::Ordering::Less),
        BinaryOperator::Lt => json_cmp(left, right).is_some_and(|o| o == std::cmp::Ordering::Less),
        BinaryOperator::Le => json_cmp(left, right).is_some_and(|o| o != std::cmp::Ordering::Greater),
        _ => false, // And/Or/Has handled above
    }
}

/// Check equality between two JSON values, coercing types where reasonable.
fn json_eq(left: &serde_json::Value, right: &serde_json::Value) -> bool {
    match (left, right) {
        (serde_json::Value::String(a), serde_json::Value::String(b)) => a == b,
        (serde_json::Value::Number(a), serde_json::Value::Number(b)) => {
            a.as_f64() == b.as_f64()
        }
        (serde_json::Value::Bool(a), serde_json::Value::Bool(b)) => a == b,
        (serde_json::Value::Null, serde_json::Value::Null) => true,
        _ => left == right,
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
fn select_fields(entities: Vec<serde_json::Value>, select: &[String]) -> Vec<serde_json::Value> {
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

/// Metadata about a navigation property needed for expansion.
struct NavExpansionInfo {
    target_type: String,
    is_collection: bool,
}

/// Resolve navigation properties for $expand on a single entity.
///
/// For each expand item, looks up the navigation property in the CSDL
/// to determine the target entity type, then queries related entities
/// (by convention: entities with a matching parent reference).
///
/// This is single-level expansion only (nested $expand is not yet supported).
pub async fn expand_entity(
    entity: &mut serde_json::Value,
    expand_items: &[temper_odata::query::types::ExpandItem],
    entity_type: &str,
    state: &crate::state::ServerState,
    tenant: &temper_runtime::tenant::TenantId,
) {
    // Resolve all navigation targets up front (while holding registry lock briefly)
    let nav_infos: Vec<(&temper_odata::query::types::ExpandItem, Option<NavExpansionInfo>)> = {
        let registry = state.registry.read().unwrap();
        let tenant_config = registry.get_tenant(tenant);
        expand_items.iter().map(|item| {
            let target = tenant_config.and_then(|tc| {
                find_nav_target(&tc.csdl, entity_type, &item.property)
            }).or_else(|| {
                find_nav_target(&state.csdl, entity_type, &item.property)
            });
            let info = target.map(|target_type| {
                let is_collection = tenant_config.is_some_and(|tc| {
                    is_collection_nav(&tc.csdl, entity_type, &item.property)
                }) || is_collection_nav(&state.csdl, entity_type, &item.property);
                NavExpansionInfo { target_type, is_collection }
            });
            (item, info)
        }).collect()
    }; // Registry lock dropped here

    let entity_id = entity.get("entity_id")
        .and_then(|v| v.as_str())
        .or_else(|| entity.get("fields").and_then(|f| f.get("Id")).and_then(|v| v.as_str()))
        .map(String::from);

    for (item, info) in &nav_infos {
        let Some(info) = info else { continue };

        let related_ids = state.list_entity_ids(tenant, &info.target_type);
        let mut related_entities = Vec::new();

        if let Some(ref parent_id) = entity_id {
            for related_id in &related_ids {
                if let Ok(response) = state.get_tenant_entity_state(tenant, &info.target_type, related_id).await {
                    let related_json = serde_json::to_value(&response.state).unwrap_or_default();
                    // Check if this entity references the parent
                    let matches = related_json.get("fields")
                        .and_then(|f| f.as_object())
                        .is_some_and(|fields| {
                            let parent_id_field = format!("{}Id", entity_type);
                            fields.get("parentId").and_then(|v| v.as_str()) == Some(parent_id)
                                || fields.get(&parent_id_field).and_then(|v| v.as_str()) == Some(parent_id)
                        });
                    if matches {
                        related_entities.push(related_json);
                    }
                }
            }
        }

        // Apply nested query options if present
        if let Some(ref nested_opts) = item.options {
            let nested_query = QueryOptions {
                filter: nested_opts.filter.clone(),
                select: nested_opts.select.clone(),
                expand: None,
                orderby: nested_opts.orderby.clone(),
                top: nested_opts.top,
                skip: nested_opts.skip,
                count: None,
            };
            let (filtered, _) = apply_query_options(related_entities, &nested_query);
            related_entities = filtered;
        }

        if let Some(obj) = entity.as_object_mut() {
            if info.is_collection {
                obj.insert(item.property.clone(), serde_json::json!(related_entities));
            } else {
                obj.insert(
                    item.property.clone(),
                    related_entities.into_iter().next().unwrap_or(serde_json::Value::Null),
                );
            }
        }
    }
}

/// Find the target entity type name for a navigation property.
fn find_nav_target(
    csdl: &temper_spec::csdl::CsdlDocument,
    entity_type: &str,
    nav_prop: &str,
) -> Option<String> {
    for schema in &csdl.schemas {
        if let Some(et) = schema.entity_type(entity_type) {
            if let Some(np) = et.navigation_properties.iter().find(|n| n.name == nav_prop) {
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
        if let Some(et) = schema.entity_type(entity_type) {
            if let Some(np) = et.navigation_properties.iter().find(|n| n.name == nav_prop) {
                return np.type_name.starts_with("Collection(");
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_entities() -> Vec<serde_json::Value> {
        vec![
            serde_json::json!({"Id": "1", "Name": "Alice", "Age": 30, "Status": "Active"}),
            serde_json::json!({"Id": "2", "Name": "Bob", "Age": 25, "Status": "Draft"}),
            serde_json::json!({"Id": "3", "Name": "Charlie", "Age": 35, "Status": "Active"}),
        ]
    }

    #[test]
    fn test_filter_eq() {
        let entities = sample_entities();
        let filter = FilterExpr::BinaryOp {
            left: Box::new(FilterExpr::Property("Status".into())),
            op: BinaryOperator::Eq,
            right: Box::new(FilterExpr::Literal(ODataValue::String("Active".into()))),
        };
        let filtered = filter_entities(entities, &filter);
        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[0]["Name"], "Alice");
        assert_eq!(filtered[1]["Name"], "Charlie");
    }

    #[test]
    fn test_filter_gt() {
        let entities = sample_entities();
        let filter = FilterExpr::BinaryOp {
            left: Box::new(FilterExpr::Property("Age".into())),
            op: BinaryOperator::Gt,
            right: Box::new(FilterExpr::Literal(ODataValue::Int(28))),
        };
        let filtered = filter_entities(entities, &filter);
        assert_eq!(filtered.len(), 2);
    }

    #[test]
    fn test_orderby_asc() {
        let mut entities = sample_entities();
        let orderby = vec![OrderByClause {
            property: "Age".into(),
            direction: OrderDirection::Asc,
        }];
        sort_entities(&mut entities, &orderby);
        assert_eq!(entities[0]["Name"], "Bob");
        assert_eq!(entities[1]["Name"], "Alice");
        assert_eq!(entities[2]["Name"], "Charlie");
    }

    #[test]
    fn test_orderby_desc() {
        let mut entities = sample_entities();
        let orderby = vec![OrderByClause {
            property: "Name".into(),
            direction: OrderDirection::Desc,
        }];
        sort_entities(&mut entities, &orderby);
        assert_eq!(entities[0]["Name"], "Charlie");
        assert_eq!(entities[1]["Name"], "Bob");
        assert_eq!(entities[2]["Name"], "Alice");
    }

    #[test]
    fn test_select_fields() {
        let entities = sample_entities();
        let selected = select_fields(entities, &["Id".into(), "Name".into()]);
        assert_eq!(selected[0].as_object().unwrap().len(), 2);
        assert!(selected[0].get("Id").is_some());
        assert!(selected[0].get("Name").is_some());
        assert!(selected[0].get("Age").is_none());
    }

    #[test]
    fn test_apply_query_options_combined() {
        let entities = sample_entities();
        let options = QueryOptions {
            filter: Some(FilterExpr::BinaryOp {
                left: Box::new(FilterExpr::Property("Status".into())),
                op: BinaryOperator::Eq,
                right: Box::new(FilterExpr::Literal(ODataValue::String("Active".into()))),
            }),
            orderby: Some(vec![OrderByClause {
                property: "Name".into(),
                direction: OrderDirection::Asc,
            }]),
            top: Some(1),
            skip: None,
            select: Some(vec!["Id".into(), "Name".into()]),
            count: Some(true),
            expand: None,
        };

        let (result, count) = apply_query_options(entities, &options);
        assert_eq!(count, Some(2)); // 2 Active entities before pagination
        assert_eq!(result.len(), 1); // $top=1
        assert_eq!(result[0]["Name"], "Alice"); // First alphabetically among Active
    }

    #[test]
    fn test_contains_function() {
        let entities = sample_entities();
        let filter = FilterExpr::FunctionCall {
            name: "contains".into(),
            args: vec![
                FilterExpr::Property("Name".into()),
                FilterExpr::Literal(ODataValue::String("li".into())),
            ],
        };
        let filtered = filter_entities(entities, &filter);
        assert_eq!(filtered.len(), 2); // Alice and Charlie
    }
}
