use std::collections::BTreeSet;

use super::super::super::authz::entity_id_from_body;
use super::super::types::{
    QueryPlaneCoverageReport, QueryPlaneFallbackReason, QueryPlaneReadError, QueryPlaneReadRequest,
    QueryPlaneReadStrategy,
};
use super::{
    CandidateScanResult, RowAuthz, ScanCounters, TelemetryInput, apply_select_only, base_telemetry,
    budget_rejection, decide_row_authz, filter_only_query_options, materialize_and_authorize_ids,
    query_options_with_default_page,
};
use crate::query_eval::apply_query_options;

/// Selected documents held at once while a per-entity list is filtered.
///
/// Same size as the catalog SQL batch (`CATALOG_READ_BATCH` in
/// `temper-store-postgres`). One chunk is one read, then the documents are
/// dropped; only the allowed ids are kept.
const SPARSE_DECISION_BATCH: usize = 32;

async fn read_source_cursor_without_filter_or_count(
    request: &QueryPlaneReadRequest<'_>,
    all_entity_ids: &[String],
    coverage: QueryPlaneCoverageReport,
    fallback_reason: QueryPlaneFallbackReason,
    authz: &RowAuthz,
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
                catalog_materialization: request.state.query_plane_store().is_some(),
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
                request.state.query_plane_store().is_some(),
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
            request.state.query_plane_store().is_some(),
            authz,
            &mut counters,
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
            catalog_materialization: request.state.query_plane_store().is_some(),
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
    authz: &RowAuthz,
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
            request.state.query_plane_store().is_some(),
            coverage,
            counters,
        ));
    }

    if request.query_options.filter.is_none()
        && request.query_options.orderby.is_none()
        && let Some(result) =
            try_bounded_unfiltered_page(request, all_entity_ids, coverage, fallback_reason, authz)
                .await
    {
        return result;
    }

    let mut counters = ScanCounters::empty();
    let authorized = materialize_and_authorize_ids(
        request,
        all_entity_ids,
        request.state.query_plane_store().is_some(),
        authz,
        &mut counters,
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
            catalog_materialization: request.state.query_plane_store().is_some(),
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

/// Page an unfiltered list without hydrating every catalog document.
///
/// `$count` used to mark the read as a full proof and load `fields` and
/// `state` for every id. Returns `None` when a per-row policy cannot be
/// decided from a sparse catalog read, so the caller keeps the full proof.
async fn try_bounded_unfiltered_page(
    request: &QueryPlaneReadRequest<'_>,
    all_entity_ids: &[String],
    coverage: QueryPlaneCoverageReport,
    fallback_reason: QueryPlaneFallbackReason,
    authz: &RowAuthz,
) -> Option<Result<CandidateScanResult, QueryPlaneReadError>> {
    let allowed_ids = match authz {
        RowAuthz::DenyAll => Vec::new(),
        RowAuthz::AllowAll => all_entity_ids.iter().collect(),
        RowAuthz::PerRow {
            attribute_keys: Some(keys),
            ..
        } => sparse_allowed_ids(request, all_entity_ids, keys, authz).await?,
        RowAuthz::PerRow {
            attribute_keys: None,
            ..
        } => return None,
    };

    let skip = request.query_options.skip.unwrap_or(0);
    let top = request.budget.requested_top(request.query_options);
    let include_count = request.query_options.count == Some(true);
    let prefer_catalog = request.state.query_plane_store().is_some();
    let count = include_count.then_some(allowed_ids.len());
    let page_ids = allowed_ids
        .into_iter()
        .skip(skip)
        .take(top)
        .cloned()
        .collect::<Vec<_>>();

    let mut counters = ScanCounters::empty();
    let returned_before_select =
        materialize_and_authorize_ids(request, &page_ids, prefer_catalog, authz, &mut counters)
            .await;
    let entities = apply_select_only(returned_before_select, request.query_options);
    let returned_count = entities.len();
    let telemetry = base_telemetry(
        request,
        TelemetryInput {
            strategy: QueryPlaneReadStrategy::ReadSourceCursor,
            fallback_reason,
            filter_pushdown: false,
            catalog_materialization: prefer_catalog,
            coverage,
            counters,
            returned_count,
        },
    );
    Some(Ok(CandidateScanResult {
        entities,
        count,
        telemetry,
    }))
}

async fn sparse_allowed_ids<'a>(
    request: &QueryPlaneReadRequest<'_>,
    all_entity_ids: &'a [String],
    attribute_keys: &BTreeSet<String>,
    authz: &RowAuthz,
) -> Option<Vec<&'a String>> {
    let query_plane = request.state.query_plane_store()?;
    let mut fields = attribute_keys
        .iter()
        .filter(|key| !matches!(key.as_str(), "has_spec" | "id" | "Id" | "entity_id"))
        .cloned()
        .collect::<Vec<_>>();
    if !fields
        .iter()
        .any(|field| field == "status" || field == "Status")
    {
        fields.push("status".to_string());
    }
    fields.push("entity_id".to_string());
    // Decide a batch and drop it. Holding every selected document for the
    // whole id list (up to the scan budget) is what made a per-entity list
    // the size of the catalog. One batch matches the SQL read size, and the
    // allowed result is only ids, in the same order as `all_entity_ids`.
    let defaults = crate::odata::read_defaults::ReadDefaults::for_entity(
        request.state,
        request.tenant,
        request.entity_type,
    );
    let catalog_fields = defaults.catalog_fields(&fields);
    let mut allowed = Vec::new();
    for chunk in all_entity_ids.chunks(SPARSE_DECISION_BATCH) {
        let loaded = crate::storage::load_selected_catalog_rows_by_id(
            &query_plane,
            request.tenant.as_str(),
            request.entity_type,
            chunk,
            &catalog_fields,
        )
        .await
        .ok()?;
        let crate::storage::CatalogRowsLoad::Available(rows) = loaded else {
            return None;
        };
        for entity_id in chunk {
            let Some(row) = rows.get(entity_id) else {
                continue;
            };
            let mut body = super::super::super::read_support::catalog_row_to_selected_entity_body(
                request.entity_type,
                request.entity_set_name,
                row.clone(),
                &fields,
            );
            let values = defaults.field_values(row.state.as_ref().unwrap_or(&row.fields));
            defaults.apply_selected(&mut body, &fields, &values);
            if super::entity_read_allowed(request, authz, &body) {
                allowed.push(entity_id);
            }
        }
    }
    Some(allowed)
}

pub(in crate::odata::query_plane_read) async fn read_from_source_cursor(
    request: &QueryPlaneReadRequest<'_>,
    all_entity_ids: &[String],
    coverage: QueryPlaneCoverageReport,
    missing_ids: &[String],
    fallback_reason: QueryPlaneFallbackReason,
) -> Result<CandidateScanResult, QueryPlaneReadError> {
    let authz = decide_row_authz(request);
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
            &authz,
        )
        .await
    } else {
        read_source_cursor_without_filter_or_count(
            request,
            all_entity_ids,
            coverage,
            fallback_reason,
            &authz,
        )
        .await
    }
}
