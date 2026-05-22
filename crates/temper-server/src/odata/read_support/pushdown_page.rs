use std::collections::BTreeSet;

use temper_odata::query::types::{FilterExpr, QueryOptions};
use temper_runtime::tenant::TenantId;

use crate::query_eval::apply_query_options;
use crate::state::ServerState;

/// Result of sparse page planning over SQL-pushed OData candidate IDs.
pub(in crate::odata) struct PushdownPageSelection {
    /// Final entity IDs to hydrate for the response page, in response order.
    pub(in crate::odata) entity_ids: Vec<String>,
    /// `$count` after filtering and before pagination, when requested.
    pub(in crate::odata) count: Option<usize>,
    /// Number of sparse candidate rows read to choose the page.
    pub(in crate::odata) sparse_materialized_count: usize,
}

/// Reason sparse page planning was safely skipped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::odata) enum PushdownPageSkipReason {
    /// SQL pushdown returned no candidate IDs.
    EmptyCandidates,
    /// Sparse planning requires a filter to define the candidate set.
    NoFilter,
    /// Expanded reads need full entity bodies before navigation hydration.
    HasExpand,
    /// The requested page would not reduce the candidate set.
    PageNotReduced,
    /// Required filter/order fields could not be derived.
    MissingRequiredFields,
}

impl PushdownPageSkipReason {
    /// Stable low-cardinality span label.
    pub(in crate::odata) fn as_str(self) -> &'static str {
        match self {
            Self::EmptyCandidates => "empty_candidates",
            Self::NoFilter => "no_filter",
            Self::HasExpand => "has_expand",
            Self::PageNotReduced => "page_not_reduced",
            Self::MissingRequiredFields => "missing_required_fields",
        }
    }
}

/// Inputs for sparse page planning over SQL-pushed OData candidate IDs.
pub(in crate::odata) struct PushdownPageRequest<'a> {
    /// Tenant whose catalog rows should be inspected.
    pub(in crate::odata) tenant: &'a TenantId,
    /// Logical entity type stored in the query-plane catalog.
    pub(in crate::odata) entity_type: &'a str,
    /// OData entity set name used for response materialization.
    pub(in crate::odata) entity_set_name: &'a str,
    /// Candidate IDs already narrowed by SQL filter pushdown.
    pub(in crate::odata) pushed_ids: &'a [String],
    /// Original OData query options to evaluate over sparse rows.
    pub(in crate::odata) query_options: &'a QueryOptions,
    /// Default collection page size applied when `$top` is absent.
    pub(in crate::odata) default_page_size: usize,
    /// Maximum server-side page size budget.
    pub(in crate::odata) max_entities: usize,
}

/// Choose a response page from sparse catalog rows after SQL filter pushdown.
///
/// This path keeps filter/order/top correctness by evaluating the existing
/// in-memory OData query engine over tiny projected candidate rows, then
/// returning only the final page IDs for full response hydration.
pub(in crate::odata) async fn try_select_paged_pushdown_entity_ids(
    state: &ServerState,
    request: PushdownPageRequest<'_>,
) -> Result<PushdownPageSelection, PushdownPageSkipReason> {
    if let Some(reason) = sparse_page_skip_reason(
        request.pushed_ids.len(),
        request.query_options,
        request.default_page_size,
        request.max_entities,
    ) {
        return Err(reason);
    }

    let required_fields = sparse_page_fields(request.query_options)
        .ok_or(PushdownPageSkipReason::MissingRequiredFields)?;
    let sparse = super::materialize_entity_set_entities(
        state,
        request.tenant,
        request.entity_type,
        request.entity_set_name,
        request.pushed_ids,
        true,
        Some(&required_fields),
    )
    .await;
    let sparse_materialized_count = sparse.entities.len();

    let mut page_options = request.query_options.clone();
    page_options.select = None;
    page_options.expand = None;
    if page_options.top.is_none() {
        page_options.top = Some(request.default_page_size);
    } else if let Some(top) = page_options.top {
        page_options.top = Some(top.min(request.max_entities));
    }

    let (entity_ids, count) = select_sparse_page_entity_ids(sparse.entities, &page_options);

    Ok(PushdownPageSelection {
        entity_ids,
        count,
        sparse_materialized_count,
    })
}

fn sparse_page_skip_reason(
    candidate_count: usize,
    query_options: &QueryOptions,
    default_page_size: usize,
    max_entities: usize,
) -> Option<PushdownPageSkipReason> {
    if candidate_count == 0 {
        return Some(PushdownPageSkipReason::EmptyCandidates);
    }
    if query_options.filter.is_none() {
        return Some(PushdownPageSkipReason::NoFilter);
    }
    if query_options.expand.is_some() {
        return Some(PushdownPageSkipReason::HasExpand);
    }

    let skip = query_options.skip.unwrap_or(0);
    let top = query_options
        .top
        .unwrap_or(default_page_size)
        .min(max_entities);
    if skip > 0 || top < candidate_count {
        None
    } else {
        Some(PushdownPageSkipReason::PageNotReduced)
    }
}

#[cfg(test)]
fn should_plan_sparse_page(
    candidate_count: usize,
    query_options: &QueryOptions,
    default_page_size: usize,
    max_entities: usize,
) -> bool {
    sparse_page_skip_reason(
        candidate_count,
        query_options,
        default_page_size,
        max_entities,
    )
    .is_none()
}

fn select_sparse_page_entity_ids(
    sparse_entities: Vec<serde_json::Value>,
    page_options: &QueryOptions,
) -> (Vec<String>, Option<usize>) {
    let (page, count) = apply_query_options(sparse_entities, page_options);
    let entity_ids = page
        .into_iter()
        .filter_map(|entity| {
            entity
                .get("entity_id")
                .and_then(|value| value.as_str())
                .map(str::to_string)
        })
        .collect::<Vec<_>>();
    (entity_ids, count)
}

fn sparse_page_fields(query_options: &QueryOptions) -> Option<Vec<String>> {
    query_options.filter.as_ref()?;

    let mut fields = BTreeSet::new();
    fields.insert("entity_id".to_string());
    if let Some(filter) = &query_options.filter {
        collect_filter_properties(filter, &mut fields);
    }
    if let Some(orderby) = &query_options.orderby {
        for clause in orderby {
            fields.insert(clause.property.clone());
        }
    }

    Some(fields.into_iter().collect())
}

fn collect_filter_properties(expr: &FilterExpr, fields: &mut BTreeSet<String>) {
    match expr {
        FilterExpr::Property(name) => {
            fields.insert(name.clone());
        }
        FilterExpr::BinaryOp { left, right, .. } => {
            collect_filter_properties(left, fields);
            collect_filter_properties(right, fields);
        }
        FilterExpr::UnaryOp { operand, .. } => collect_filter_properties(operand, fields),
        FilterExpr::FunctionCall { args, .. } => {
            for arg in args {
                collect_filter_properties(arg, fields);
            }
        }
        FilterExpr::Literal(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use temper_odata::query::parse_query_options;
    use temper_odata::query::types::{BinaryOperator, ODataValue, OrderByClause, OrderDirection};

    #[test]
    fn sparse_page_fields_collect_filter_order_and_entity_id() {
        let options = QueryOptions {
            filter: Some(FilterExpr::BinaryOp {
                left: Box::new(FilterExpr::Property("SessionId".to_string())),
                op: BinaryOperator::Eq,
                right: Box::new(FilterExpr::Literal(ODataValue::String("ss-1".to_string()))),
            }),
            orderby: Some(vec![OrderByClause {
                property: "Sequence".to_string(),
                direction: OrderDirection::Desc,
            }]),
            ..QueryOptions::default()
        };

        let fields = sparse_page_fields(&options).expect("fields");

        assert_eq!(fields, vec!["Sequence", "SessionId", "entity_id"]);
    }

    #[test]
    fn sparse_page_planning_requires_filter_and_reduced_page() {
        let filter = Some(FilterExpr::Literal(ODataValue::Boolean(true)));
        let reduced = QueryOptions {
            filter: filter.clone(),
            top: Some(1),
            ..QueryOptions::default()
        };
        assert!(should_plan_sparse_page(100, &reduced, 100, 1000));

        let full_page = QueryOptions {
            filter,
            top: Some(100),
            ..QueryOptions::default()
        };
        assert!(!should_plan_sparse_page(100, &full_page, 100, 1000));

        let no_filter = QueryOptions {
            top: Some(1),
            ..QueryOptions::default()
        };
        assert!(!should_plan_sparse_page(100, &no_filter, 100, 1000));
    }

    #[test]
    fn sparse_page_planning_uses_default_page_size() {
        let options = QueryOptions {
            filter: Some(FilterExpr::Literal(ODataValue::Boolean(true))),
            ..QueryOptions::default()
        };

        assert!(should_plan_sparse_page(101, &options, 100, 1000));
        assert!(!should_plan_sparse_page(100, &options, 100, 1000));
    }

    #[test]
    fn sparse_page_planning_accepts_sessionentries_latest_query_shape() {
        let options =
            parse_query_options("$filter=SessionId eq 'ss-1'&$orderby=Sequence desc&$top=1")
                .expect("query options");

        assert_eq!(sparse_page_skip_reason(1141, &options, 100, 1000), None);
        assert!(should_plan_sparse_page(1141, &options, 100, 1000));
    }

    #[test]
    fn sparse_page_skip_reason_explains_unsupported_shapes() {
        let no_filter = QueryOptions {
            top: Some(1),
            ..QueryOptions::default()
        };
        assert_eq!(
            sparse_page_skip_reason(1141, &no_filter, 100, 1000),
            Some(PushdownPageSkipReason::NoFilter)
        );

        let full_page = QueryOptions {
            filter: Some(FilterExpr::Literal(ODataValue::Boolean(true))),
            top: Some(100),
            ..QueryOptions::default()
        };
        assert_eq!(
            sparse_page_skip_reason(100, &full_page, 100, 1000),
            Some(PushdownPageSkipReason::PageNotReduced)
        );
    }

    #[test]
    fn sparse_page_selection_preserves_order_top_and_count() {
        let options = QueryOptions {
            filter: Some(FilterExpr::BinaryOp {
                left: Box::new(FilterExpr::Property("SessionId".to_string())),
                op: BinaryOperator::Eq,
                right: Box::new(FilterExpr::Literal(ODataValue::String("ss-1".to_string()))),
            }),
            orderby: Some(vec![OrderByClause {
                property: "Sequence".to_string(),
                direction: OrderDirection::Desc,
            }]),
            top: Some(1),
            count: Some(true),
            ..QueryOptions::default()
        };
        let sparse_entities = vec![
            json!({"entity_id": "en-1", "SessionId": "ss-1", "Sequence": 1}),
            json!({"entity_id": "en-2", "SessionId": "ss-1", "Sequence": 3}),
            json!({"entity_id": "en-3", "SessionId": "ss-2", "Sequence": 9}),
            json!({"entity_id": "en-4", "SessionId": "ss-1", "Sequence": 2}),
        ];

        let (entity_ids, count) = select_sparse_page_entity_ids(sparse_entities, &options);

        assert_eq!(entity_ids, vec!["en-2"]);
        assert_eq!(count, Some(3));
    }
}
