//! OData entity-set reads through the query-plane contract.

mod scan;
#[cfg(test)]
mod tests;
mod types;

pub(in crate::odata) use types::{
    QueryPlaneReadBudget, QueryPlaneReadError, QueryPlaneReadRequest, QueryPlaneReadResult,
};

use super::authz::{LIST_ACTION, authorize_read};
use super::read_support::missing_catalog_entity_ids;
use scan::{native_candidate_page_plan, read_from_source_cursor, try_native_page_read};
use types::{QueryPlaneCoverageReport, QueryPlaneFallbackReason};

async fn catalog_coverage_report(
    request: &QueryPlaneReadRequest<'_>,
    all_entity_ids: &[String],
) -> (QueryPlaneCoverageReport, Vec<String>) {
    let missing_ids = missing_catalog_entity_ids(
        request.state,
        request.tenant,
        request.entity_type,
        all_entity_ids,
    )
    .await;
    (
        QueryPlaneCoverageReport {
            missing: missing_ids.len(),
            matched: 0,
        },
        missing_ids,
    )
}

/// Execute one OData entity-set read through the query-plane contract.
pub(in crate::odata) async fn read_entity_set_from_query_plane(
    request: QueryPlaneReadRequest<'_>,
) -> Result<QueryPlaneReadResult, QueryPlaneReadError> {
    if let Err(response) = authorize_read(
        request.state,
        request.tenant,
        request.security_ctx,
        LIST_ACTION,
        request.entity_type,
        "",
        &serde_json::json!({}),
    ) {
        return Err(QueryPlaneReadError::AuthorizationDenied(response));
    }

    let all_entity_ids = request
        .state
        .list_entity_ids_lazy(request.tenant, request.entity_type)
        .await;
    let (coverage, missing_ids) = catalog_coverage_report(&request, &all_entity_ids).await;
    let native_plan = native_candidate_page_plan(&request);

    if coverage.missing == 0
        && let Some(plan) = native_plan.clone()
        && let Some(result) = try_native_page_read(&request, plan, coverage).await
    {
        return result.map(|result| QueryPlaneReadResult {
            entities: result.entities,
            count: result.count,
            telemetry: result.telemetry,
        });
    }

    let fallback_reason = if coverage.missing > 0 {
        QueryPlaneFallbackReason::CatalogCoverageGap
    } else if request.query_options.filter.is_some() && native_plan.is_none() {
        QueryPlaneFallbackReason::FilterPushdownUnavailable
    } else if request.state.query_plane_store().is_some() {
        QueryPlaneFallbackReason::NativePageUnavailable
    } else {
        QueryPlaneFallbackReason::NoFilterPushdown
    };
    let result = read_from_source_cursor(
        &request,
        &all_entity_ids,
        coverage,
        &missing_ids,
        fallback_reason,
    )
    .await?;
    Ok(QueryPlaneReadResult {
        entities: result.entities,
        count: result.count,
        telemetry: result.telemetry,
    })
}
