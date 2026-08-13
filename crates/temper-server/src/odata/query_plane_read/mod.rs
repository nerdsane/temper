//! OData entity-set reads through the query-plane contract.

mod pagination;
mod scan;
#[cfg(test)]
mod tests;
mod types;

pub(in crate::odata) use pagination::read_entity_set_from_query_plane;
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

/// ADR-0153: when the read's `$filter` is exactly a declared `[[key]]`, resolve
/// it to the single matching `entity_id` via `entity_key_index` — a bounded
/// candidate set (no full-type scan, so the budget cannot trip → no 413).
///
/// Returns `Some(vec![id])` on a keyed **hit**; `None` otherwise — including a
/// keyed **miss**, which falls back to the full scan because (until the backfill
/// gate lands) a missing key row may be a pre-backfill entity rather than a true
/// absence. So hits are fast and correct now; authoritative absence follows the
/// per-tenant backfill watermark. `$orderby`/`$count` also decline (a point read
/// has neither to honor).
async fn keyed_candidate_ids(request: &QueryPlaneReadRequest<'_>) -> Option<Vec<String>> {
    if request.query_options.orderby.is_some() || request.query_options.count == Some(true) {
        return None;
    }
    let filter = request.query_options.filter.as_ref()?;
    let pairs = super::filter_sql::equality_field_predicates(filter)?;
    // Resolve declared keys via the registry-aware path: runtime-installed os-app
    // entities (File, Directory, …) live in the per-tenant registry, NOT in
    // `transition_tables`. Reading `transition_tables` here would return `None` for
    // them, disabling the keyed path so every point read scans and 413s (ARN-68).
    let keys = request
        .state
        .declared_keys_for(request.tenant, request.entity_type);
    let (key_name, key_hash) = crate::key_index::resolve_query_to_key(&keys, &pairs)?;
    let (store, _) = request.state.event_journal()?;
    match store
        .lookup_by_key(
            request.tenant.as_str(),
            request.entity_type,
            &key_name,
            &key_hash,
        )
        .await
    {
        Ok(Some(entity_id)) => Some(vec![entity_id]),
        Ok(None) => {
            // A miss is authoritative absence ONLY once the backfill watermark says
            // `entity_key_index` is complete for this (tenant, type) — then we can
            // answer "not found" with an empty candidate set (no full-type scan, no
            // 413). Before the watermark, a missing row may be a pre-backfill entity,
            // so we fall back to the scan (correct, just not bounded). This is the
            // retirement of #324's reconcile scan, gated per ADR-0153.
            // Authoritative absence requires the CURRENT declared key-set to be fully
            // backfilled — otherwise a just-declared key (e.g. a newly-added `ws_path`)
            // whose rows are not yet assigned would make a present entity read as absent.
            let current_key_set = crate::key_index::declared_key_set_signature(&keys);
            if request
                .state
                .key_index_backfill_complete(request.tenant, request.entity_type, &current_key_set)
                .await
            {
                Some(Vec::new())
            } else {
                None
            }
        }
        // Error: fall back to the full path (never trust a transient failure as absence).
        Err(_) => None,
    }
}

fn should_reconcile_empty_exact_match_against_authoritative(
    request: &QueryPlaneReadRequest<'_>,
    indexed_entity_ids: &[String],
    result: &scan::CandidateScanResult,
) -> bool {
    if !result.entities.is_empty() || request.budget.requested_top(request.query_options) == 0 {
        return false;
    }
    // Original case: nothing in the in-memory index yet, so a lazy catalog
    // repair may surface entities the native page could not see.
    if indexed_entity_ids.is_empty() {
        return true;
    }
    // ARN-89: an empty native page for a pure equality-conjunction resolution
    // (e.g. `Path eq '..' and WorkspaceId eq '..'`) is not trustworthy under
    // projection lag — a just-committed entity may not yet have its
    // asynchronously-projected row. Reconcile against authoritative state even
    // when the index already holds other entities of this type.
    //
    // Cost tradeoff: a genuinely-absent target then falls through to a bounded
    // authoritative scan (capped by `scan_candidate_budget`) instead of a cheap
    // empty native page. That is the read-path cost of read-after-write
    // correctness; it never serializes the write hot path. The gate is tight —
    // only pushdown-able equality conjunctions pay it; ranges/Or/Ne/contains/
    // functions return `None` from `equality_field_predicates` and are
    // unaffected. When the type is too large for that scan, the budget gate
    // bounds the reconcile to the field-index coverage gap (ARN-68) —
    // see `read_entity_set_from_query_plane`.
    request
        .query_options
        .filter
        .as_ref()
        .and_then(super::filter_sql::equality_field_predicates)
        .is_some()
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

/// Execute one page of an OData entity-set read through the query-plane
/// contract. Pagination (server-driven `@odata.nextLink` continuation) is
/// layered on top by [`read_entity_set_from_query_plane`].
pub(in crate::odata) async fn read_entity_set_page(
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
    // Set when an empty native page for an exact-match resolution is treated as
    // a possibly-lagging projection and we fall through to the authoritative
    // path below (ARN-89). It suppresses the native retry on that path and
    // selects the `ProjectionLagReconcile` telemetry reason. If the type then
    // turns out to be too large for the reconcile scan, the budget gate bounds
    // the reconcile to the field-index coverage gap instead of rejecting (ARN-68).
    let mut reconciling_exact_match_lag = false;
    if let Some(plan) = native_plan.clone()
        && should_try_native_before_catalog_coverage(&request, &plan)
    {
        let indexed_entity_ids =
            crate::application_data::GovernedApplicationDataService::new(request.state)
                .indexed_candidates(request.tenant, request.entity_type);
        let can_repair_with_bounded_coverage = !indexed_entity_ids.is_empty()
            && indexed_entity_ids.len() <= request.budget.scan_candidate_budget();
        if can_repair_with_bounded_coverage {
            let (coverage, missing_ids) =
                catalog_coverage_report(&request, &indexed_entity_ids).await;
            if coverage.missing == 0 {
                if let Some(result) = try_native_page_read(&request, plan, coverage).await {
                    let result = result?;
                    if should_reconcile_empty_exact_match_against_authoritative(
                        &request,
                        &indexed_entity_ids,
                        &result,
                    ) {
                        reconciling_exact_match_lag = true;
                    } else {
                        return Ok(QueryPlaneReadResult {
                            entities: result.entities,
                            count: result.count,
                            telemetry: result.telemetry,
                            next_skiptoken: None,
                        });
                    }
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
                    next_skiptoken: None,
                });
            }
        } else if let Some(result) =
            try_native_page_read(&request, plan, QueryPlaneCoverageReport::default()).await
        {
            let result = result?;
            if should_reconcile_empty_exact_match_against_authoritative(
                &request,
                &indexed_entity_ids,
                &result,
            ) {
                reconciling_exact_match_lag = true;
            } else {
                return Ok(QueryPlaneReadResult {
                    entities: result.entities,
                    count: result.count,
                    telemetry: result.telemetry,
                    next_skiptoken: None,
                });
            }
        }
    }

    // ADR-0153 fast path: if the filter is exactly a declared `[[key]]` (the
    // shape behind the agents' 413 — `Files?$filter=WorkspaceId eq … and Path eq …`),
    // probe `entity_key_index` for the one matching entity_id instead of listing the
    // whole entity type. The candidate set becomes bounded (0 or 1), so the rest of
    // the read (coverage, budget, materialization, row-auth) runs unchanged and the
    // scan budget can never trip. On a miss we fall back to the full list, which still
    // covers pre-backfill entities — a safe additive fast path until #324 is retired.
    let keyed = keyed_candidate_ids(&request).await;
    // A keyed result of `Some([])` is authoritative absence (the watermark is set):
    // the co-committed `entity_key_index` has no such entity. We must NOT then run a
    // native page read — the eventually-consistent field index could still surface a
    // lagging or just-deleted row and contradict the authoritative answer. Fall
    // straight through to materialize the empty candidate set.
    let keyed_authoritative_absent = matches!(&keyed, Some(ids) if ids.is_empty());
    let all_entity_ids = match keyed {
        Some(ids) => ids,
        None => {
            crate::application_data::GovernedApplicationDataService::new(request.state)
                .fallback_candidates(request.tenant, request.entity_type)
                .await
        }
    };
    let needs_full_proof = request.query_options.filter.is_some()
        || request.query_options.orderby.is_some()
        || request.query_options.count == Some(true);
    if needs_full_proof
        && !should_check_source_cursor_catalog_coverage(all_entity_ids.len(), request.budget)
    {
        // ARN-68 (the SessionEntries-list flavor): the reconcile scan is over budget,
        // which used to be an unconditional 413 — so EVERY empty equality-conjunction
        // list on a high-cardinality type failed (e.g. a session bootstrap listing
        // `SessionId eq '<new>'` against 95k entries). The full scan is only needed for
        // entities the native page cannot see: the ones with NO `entity_field_index`
        // row for a filtered field (a just-committed entity whose async projection has
        // not landed, a crash-lost projection, or a pre-projection-era entity). That
        // gap is enumerable per field and normally tiny, so reconcile the GAP instead
        // of the type: union a RE-RUN native page (probe-then-page ordering makes the
        // union complete — anything covered at probe time is visible to the later
        // page, anything uncovered is in the gap, so a projection landing between the
        // first page and the probe is not dropped) with the materialized gap, in one
        // source-cursor pass. A committed-but-unprojected match is FOUND
        // (read-after-write repaired, not rejected); a genuine miss returns bounded
        // empty. If the union exceeds the scan budget, or the backend has no
        // field-index coverage probe, keep the honest rejection. Entities whose field
        // row exists with a STALE value stay invisible — the same trust every
        // non-empty native page already gets at any type size (the small-type ARN-89
        // reconcile is stronger; this gate trades that for boundedness).
        // Reconcile-affordable types never reach this gate, so their ARN-89 repair
        // semantics are unchanged.
        if reconciling_exact_match_lag
            && let Some(pairs) = request
                .query_options
                .filter
                .as_ref()
                .and_then(super::filter_sql::equality_field_predicates)
            && let Some(query_plane) = request.state.query_plane_store()
        {
            let field_names: std::collections::BTreeSet<String> =
                pairs.into_iter().map(|(name, _)| name).collect();
            let mut gap_ids: Option<std::collections::BTreeSet<String>> =
                Some(std::collections::BTreeSet::new());
            for field_name in field_names {
                let covered = query_plane
                    .query_field_index(
                        request.tenant.as_str(),
                        request.entity_type,
                        "entity_id IN (SELECT entity_id FROM entity_field_index \
                         WHERE tenant = ?1 AND entity_type = ?2 AND field_name = ?3)",
                        vec![field_name],
                    )
                    .await;
                match covered {
                    Ok(Some(covered_ids)) => {
                        let covered: std::collections::BTreeSet<&String> =
                            covered_ids.iter().collect();
                        if let Some(gap) = gap_ids.as_mut() {
                            gap.extend(
                                all_entity_ids
                                    .iter()
                                    .filter(|id| !covered.contains(id))
                                    .cloned(),
                            );
                        }
                    }
                    // Unsupported backend or probe failure: never trust the empty
                    // page without coverage proof — fall through to the rejection.
                    Ok(None) | Err(_) => {
                        gap_ids = None;
                        break;
                    }
                }
            }
            // Re-run the page with `$select` stripped (the union needs `entity_id`,
            // which selection would drop) and `$skip` folded into `$top` (the union
            // must carry the FULL covered prefix; the final cursor pass applies the
            // real `$skip`/`$top`/`$select` exactly once).
            let rescue_options = temper_odata::query::types::QueryOptions {
                select: None,
                skip: None,
                top: Some(
                    request.query_options.skip.unwrap_or(0)
                        + request.budget.requested_top(request.query_options),
                ),
                ..request.query_options.clone()
            };
            let rescue_request = QueryPlaneReadRequest {
                query_options: &rescue_options,
                ..request
            };
            if let Some(gap_ids) = gap_ids
                && let Some(plan) = native_plan.clone()
                && let Some(page) =
                    try_native_page_read(&rescue_request, plan, QueryPlaneCoverageReport::default())
                        .await
            {
                let page = page?;
                let gap_len = gap_ids.len();
                let missing_ids: Vec<String> = gap_ids.iter().cloned().collect();
                let mut candidate_ids = gap_ids;
                for entity in &page.entities {
                    if let Some(id) = entity.get("entity_id").and_then(|v| v.as_str()) {
                        candidate_ids.insert(id.to_string());
                    }
                }
                if candidate_ids.len() <= request.budget.scan_candidate_budget() {
                    let ids: Vec<String> = candidate_ids.into_iter().collect();
                    let coverage = QueryPlaneCoverageReport {
                        missing: gap_len,
                        matched: 0,
                    };
                    let result = read_from_source_cursor(
                        &request,
                        &ids,
                        coverage,
                        &missing_ids,
                        QueryPlaneFallbackReason::ProjectionLagReconcile,
                    )
                    .await?;
                    return Ok(QueryPlaneReadResult {
                        entities: result.entities,
                        count: result.count,
                        telemetry: result.telemetry,
                        next_skiptoken: None,
                    });
                }
            }
        }
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

    if !reconciling_exact_match_lag
        && !keyed_authoritative_absent
        && coverage.missing == 0
        && let Some(plan) = native_plan.clone()
        && let Some(result) = try_native_page_read(&request, plan, coverage).await
    {
        return result.map(|result| QueryPlaneReadResult {
            entities: result.entities,
            count: result.count,
            telemetry: result.telemetry,
            next_skiptoken: None,
        });
    }

    let fallback_reason = if keyed_authoritative_absent {
        QueryPlaneFallbackReason::KeyedAbsence
    } else if reconciling_exact_match_lag {
        QueryPlaneFallbackReason::ProjectionLagReconcile
    } else if coverage.missing > 0 {
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
        next_skiptoken: None,
    })
}

// The server-driven paging wrapper (`read_entity_set_from_query_plane`) lives in
// `pagination` and is re-exported above; it layers a keyset `$skiptoken`
// continuation over this core planner.
