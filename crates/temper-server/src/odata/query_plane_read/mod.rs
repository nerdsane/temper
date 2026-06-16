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
use scan::{
    ScanCounters, budget_rejection, native_candidate_page_plan, read_from_source_cursor,
    try_native_page_read,
};
use types::{QueryPlaneCoverageReport, QueryPlaneFallbackReason, QueryPlaneReadStrategy};

fn should_try_native_before_catalog_coverage(
    request: &QueryPlaneReadRequest<'_>,
    plan: &scan::NativeCandidatePagePlan,
) -> bool {
    plan.filter_pushdown && request.query_options.count != Some(true)
}

fn should_try_lazy_catalog_repair_after_native_result(
    request: &QueryPlaneReadRequest<'_>,
    indexed_entity_ids: &[String],
    result: &scan::CandidateScanResult,
) -> bool {
    indexed_entity_ids.is_empty()
        && result.entities.is_empty()
        && request.budget.requested_top(request.query_options) > 0
}

fn should_check_source_cursor_catalog_coverage(
    candidate_count: usize,
    budget: QueryPlaneReadBudget,
) -> bool {
    candidate_count <= budget.scan_candidate_budget()
}

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

    let native_plan = native_candidate_page_plan(&request);
    if let Some(plan) = native_plan.clone()
        && should_try_native_before_catalog_coverage(&request, &plan)
    {
        let indexed_entity_ids = request
            .state
            .list_entity_ids(request.tenant, request.entity_type);
        let can_repair_with_bounded_coverage = !indexed_entity_ids.is_empty()
            && indexed_entity_ids.len() <= request.budget.scan_candidate_budget();
        if can_repair_with_bounded_coverage {
            let (coverage, missing_ids) =
                catalog_coverage_report(&request, &indexed_entity_ids).await;
            if coverage.missing == 0 {
                if let Some(result) = try_native_page_read(&request, plan, coverage).await {
                    return result.map(|result| QueryPlaneReadResult {
                        entities: result.entities,
                        count: result.count,
                        telemetry: result.telemetry,
                    });
                }
            } else {
                let result = read_from_source_cursor(
                    &request,
                    &indexed_entity_ids,
                    coverage,
                    &missing_ids,
                    QueryPlaneFallbackReason::CatalogCoverageGap,
                )
                .await?;
                return Ok(QueryPlaneReadResult {
                    entities: result.entities,
                    count: result.count,
                    telemetry: result.telemetry,
                });
            }
        } else if let Some(result) =
            try_native_page_read(&request, plan, QueryPlaneCoverageReport::default()).await
        {
            let result = result?;
            if !should_try_lazy_catalog_repair_after_native_result(
                &request,
                &indexed_entity_ids,
                &result,
            ) {
                return Ok(QueryPlaneReadResult {
                    entities: result.entities,
                    count: result.count,
                    telemetry: result.telemetry,
                });
            }
        }
    }

    let all_entity_ids = request
        .state
        .list_entity_ids_lazy(request.tenant, request.entity_type)
        .await;
    let needs_full_proof = request.query_options.filter.is_some()
        || request.query_options.orderby.is_some()
        || request.query_options.count == Some(true);
    if needs_full_proof
        && !should_check_source_cursor_catalog_coverage(all_entity_ids.len(), request.budget)
    {
        return Err(budget_rejection(
            &request,
            QueryPlaneReadStrategy::ReadSourceCursor,
            false,
            request.state.query_plane_store().is_some(),
            QueryPlaneCoverageReport::default(),
            ScanCounters {
                candidate_count: all_entity_ids.len(),
                ..ScanCounters::empty()
            },
        ));
    }
    let (coverage, missing_ids) = catalog_coverage_report(&request, &all_entity_ids).await;

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
