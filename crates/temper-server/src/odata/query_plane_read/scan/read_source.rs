use std::collections::BTreeSet;

use super::super::super::authz::entity_id_from_body;
use super::super::types::{
    QueryPlaneCoverageReport, QueryPlaneFallbackReason, QueryPlaneReadAuthorization,
    QueryPlaneReadError, QueryPlaneReadRequest, QueryPlaneReadStrategy,
};
use super::{
    CandidateScanResult, ScanCounters, TelemetryInput, apply_select_only, base_telemetry,
    budget_rejection, filter_only_query_options, materialize_and_authorize_ids,
    query_options_with_default_page,
};
use crate::query_eval::apply_query_options;

async fn read_source_cursor_without_filter_or_count(
    request: &QueryPlaneReadRequest<'_>,
    all_entity_ids: &[String],
    coverage: QueryPlaneCoverageReport,
    fallback_reason: QueryPlaneFallbackReason,
    allow_catalog: bool,
    authorization: QueryPlaneReadAuthorization,
) -> Result<CandidateScanResult, QueryPlaneReadError> {
    let requested_top = request.budget.requested_top(request.query_options);
    let skip = request.query_options.skip.unwrap_or(0);
    if requested_top == 0 {
        let counters = ScanCounters::empty();
        let telemetry = base_telemetry(
            request,
            TelemetryInput {
                strategy: QueryPlaneReadStrategy::ReadSourceCursor,
                fallback_reason,
                filter_pushdown: false,
                catalog_materialization: allow_catalog
                    && request.state.query_plane_store().is_some(),
                coverage,
                counters,
                returned_count: 0,
            },
        );
        return Ok(CandidateScanResult {
            entities: Vec::new(),
            count: None,
            telemetry,
        });
    }

    let scan_budget = request.budget.scan_candidate_budget();
    let page_size = request.budget.scan_page_size(request.query_options);
    let mut offset = 0usize;
    let mut authorized_seen = 0usize;
    let mut returned_before_select = Vec::new();
    let mut counters = ScanCounters::empty();

    while offset < all_entity_ids.len() {
        let remaining_budget = scan_budget.saturating_sub(counters.candidate_count);
        if remaining_budget == 0 {
            return Err(budget_rejection(
                request,
                QueryPlaneReadStrategy::ReadSourceCursor,
                false,
                allow_catalog && request.state.query_plane_store().is_some(),
                coverage,
                counters,
            ));
        }

        let end = all_entity_ids
            .len()
            .min(offset + page_size.min(remaining_budget));
        let authorized = materialize_and_authorize_ids(
            request,
            &all_entity_ids[offset..end],
            allow_catalog,
            &mut counters,
            authorization,
        )
        .await;
        for entity in authorized {
            let index = authorized_seen;
            authorized_seen += 1;
            if index >= skip && returned_before_select.len() < requested_top {
                returned_before_select.push(entity);
            }
        }

        offset = end;
        if returned_before_select.len() >= requested_top {
            break;
        }
    }

    let entities = apply_select_only(returned_before_select, request.query_options);
    let returned_count = entities.len();
    let telemetry = base_telemetry(
        request,
        TelemetryInput {
            strategy: QueryPlaneReadStrategy::ReadSourceCursor,
            fallback_reason,
            filter_pushdown: false,
            catalog_materialization: allow_catalog && request.state.query_plane_store().is_some(),
            coverage,
            counters,
            returned_count,
        },
    );
    Ok(CandidateScanResult {
        entities,
        count: None,
        telemetry,
    })
}

async fn read_source_full_proof(
    request: &QueryPlaneReadRequest<'_>,
    all_entity_ids: &[String],
    mut coverage: QueryPlaneCoverageReport,
    missing_ids: &[String],
    fallback_reason: QueryPlaneFallbackReason,
    allow_catalog: bool,
    authorization: QueryPlaneReadAuthorization,
) -> Result<CandidateScanResult, QueryPlaneReadError> {
    let scan_budget = request.budget.scan_candidate_budget();
    if all_entity_ids.len() > scan_budget {
        let counters = ScanCounters {
            candidate_count: all_entity_ids.len(),
            ..ScanCounters::empty()
        };
        return Err(budget_rejection(
            request,
            QueryPlaneReadStrategy::ReadSourceCursor,
            false,
            allow_catalog && request.state.query_plane_store().is_some(),
            coverage,
            counters,
        ));
    }

    let mut counters = ScanCounters::empty();
    let authorized = materialize_and_authorize_ids(
        request,
        all_entity_ids,
        allow_catalog,
        &mut counters,
        authorization,
    )
    .await;

    if !missing_ids.is_empty() && request.query_options.filter.is_some() {
        let missing = missing_ids.iter().cloned().collect::<BTreeSet<_>>();
        let (matching_missing, _) = apply_query_options(
            authorized.clone(),
            &filter_only_query_options(request.query_options),
        );
        coverage.matched = matching_missing
            .iter()
            .filter_map(entity_id_from_body)
            .filter(|entity_id| missing.contains(*entity_id))
            .count();
    }

    let apply_options = query_options_with_default_page(request.query_options, request.budget);
    let (entities, count) = apply_query_options(authorized, &apply_options);
    let returned_count = entities.len();
    let telemetry = base_telemetry(
        request,
        TelemetryInput {
            strategy: QueryPlaneReadStrategy::ReadSourceCursor,
            fallback_reason,
            filter_pushdown: false,
            catalog_materialization: allow_catalog && request.state.query_plane_store().is_some(),
            coverage,
            counters,
            returned_count,
        },
    );
    Ok(CandidateScanResult {
        entities,
        count,
        telemetry,
    })
}

pub(in crate::odata::query_plane_read) async fn read_from_source_cursor(
    request: &QueryPlaneReadRequest<'_>,
    all_entity_ids: &[String],
    coverage: QueryPlaneCoverageReport,
    missing_ids: &[String],
    fallback_reason: QueryPlaneFallbackReason,
    allow_catalog: bool,
    authorization: QueryPlaneReadAuthorization,
) -> Result<CandidateScanResult, QueryPlaneReadError> {
    let needs_full_proof = request.query_options.filter.is_some()
        || request.query_options.orderby.is_some()
        || request.query_options.count == Some(true);
    if needs_full_proof {
        read_source_full_proof(
            request,
            all_entity_ids,
            coverage,
            missing_ids,
            fallback_reason,
            allow_catalog,
            authorization,
        )
        .await
    } else {
        read_source_cursor_without_filter_or_count(
            request,
            all_entity_ids,
            coverage,
            fallback_reason,
            allow_catalog,
            authorization,
        )
        .await
    }
}
