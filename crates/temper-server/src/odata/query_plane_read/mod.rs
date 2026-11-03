//! OData entity-set reads through the query-plane contract.

mod equality;
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
use scan::{
    ScanCounters, budget_rejection, native_candidate_page_plan, read_from_source_cursor,
    try_native_page_read,
};
use types::{QueryPlaneCoverageReport, QueryPlaneFallbackReason, QueryPlaneReadStrategy};

fn should_try_native_page_first(
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
    // bounds the reconcile to the rows the native page cannot answer for (ARN-68) —
    // see `read_entity_set_from_query_plane`.
    request
        .query_options
        .filter
        .as_ref()
        .and_then(super::filter_sql::equality_field_predicates)
        .is_some()
}

/// The ids among `all_entity_ids` whose rows the native page cannot answer
/// for, or `None` when the store cannot say.
///
/// A projection writes a row's catalog entry and its field index from the same
/// fields, so a current catalog row whose fields the index can see holds every
/// committed value the page filters on (`projection_catalog_freshness`). Only
/// the other rows can hold a match the page missed: one with no catalog row
/// yet, one whose journal has a newer event, and one whose fields sit inside a
/// `fields` object the index does not look into, and one projected before the
/// field index existed, which has no index row at all until a backfill reaches
/// it (every projection indexes at least the row's `Id`). A row with no value
/// for a filtered field has no index row for it and cannot match an equality
/// on it. A field the spec keeps out of the index (`query_indexed = false`)
/// has no rows at all, so then no row is answered for.
async fn unvouched_ids(
    request: &QueryPlaneReadRequest<'_>,
    field_names: &std::collections::BTreeSet<String>,
    all_entity_ids: &[String],
) -> Option<std::collections::BTreeSet<String>> {
    let query_plane = request.state.query_plane_store()?;
    if field_names.iter().any(|field| {
        !request
            .state
            .query_projection_indexes_field(request.tenant, request.entity_type, field)
    }) {
        return Some(all_entity_ids.iter().cloned().collect());
    }
    let rows = match query_plane
        .projection_catalog_freshness(request.tenant.as_str(), request.entity_type)
        .await
    {
        Ok(rows) => rows?,
        Err(error) => {
            tracing::warn!(
                tenant = %request.tenant, entity_type = request.entity_type, error = %error,
                "projection catalog freshness read failed; an empty exact match is not reconciled by gap"
            );
            return None;
        }
    };
    let indexed: std::collections::BTreeSet<String> = match query_plane
        .query_field_index(
            request.tenant.as_str(),
            request.entity_type,
            "entity_id IN (SELECT entity_id FROM entity_field_index \
             WHERE tenant = ?1 AND entity_type = ?2)",
            Vec::new(),
        )
        .await
    {
        Ok(Some(ids)) => ids.into_iter().collect(),
        Ok(None) | Err(_) => return None,
    };
    let vouched: std::collections::BTreeSet<String> = rows
        .into_iter()
        .filter(|row| {
            row.current && row.wrapped_fields.is_none() && indexed.contains(&row.entity_id)
        })
        .map(|row| row.entity_id)
        .collect();
    let gap: std::collections::BTreeSet<String> = all_entity_ids
        .iter()
        .filter(|id| !vouched.contains(*id))
        .cloned()
        .collect();
    debug_assert!(gap.len() <= all_entity_ids.len());
    Some(gap)
}

fn should_check_source_cursor_catalog_coverage(
    candidate_count: usize,
    budget: QueryPlaneReadBudget,
) -> bool {
    candidate_count <= budget.scan_candidate_budget()
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
    // ADR-0153 fast path: if the filter is exactly a declared `[[key]]` (the
    // shape behind the agents' 413: `Files?$filter=WorkspaceId eq … and Path eq …`),
    // probe `entity_key_index` for the one matching entity_id instead of listing the
    // whole entity type. The candidate set becomes bounded (0 or 1), so the rest of
    // the read (coverage, budget, materialization, row-auth) runs unchanged and the
    // scan budget can never trip. On a miss we fall back to the full list, which still
    // covers pre-backfill entities: a safe additive fast path until #324 is retired.
    let keyed = keyed_candidate_ids(&request).await;
    if keyed.is_none()
        && let Some(result) = equality::try_reconciled_equality_read(&request).await
    {
        return result;
    }
    // Without an indexed string-equality conjunct (for example a null-only
    // filter, a non-indexed field, or an OR), or without a durable delta for
    // this actual journal, retain the general planner below. Its conservative
    // scan is still needed when no narrower complete candidate proof exists.

    // A keyed result of `Some([])` is authoritative absence (the watermark is set):
    // the co-committed `entity_key_index` has no such entity. No native page read
    // runs after that, here or below: the eventually-consistent field index and
    // catalog could still hold a row for an entity whose key has since changed, and
    // returning it would contradict the authoritative answer. So the key index is
    // asked first, and an absence goes straight to the empty candidate set.
    let keyed_authoritative_absent = matches!(&keyed, Some(ids) if ids.is_empty());
    // Set when an empty native page for an exact-match resolution is treated as
    // a possibly-lagging projection and we fall through to the authoritative
    // path below (ARN-89). It suppresses the native retry on that path and
    // selects the `ProjectionLagReconcile` telemetry reason. If the type then
    // turns out to be too large for the reconcile scan, the budget gate bounds
    // the reconcile to the rows the native page cannot answer for instead of
    // rejecting (ARN-68).
    let mut reconciling_exact_match_lag = false;
    if !keyed_authoritative_absent
        && let Some(plan) = native_plan.clone()
        && should_try_native_page_first(&request, &plan)
    {
        let indexed_entity_ids = request
            .state
            .list_entity_ids(request.tenant, request.entity_type);
        if let Some(result) =
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

    let all_entity_ids = match keyed {
        Some(ids) => ids,
        None => {
            request
                .state
                .list_entity_ids_lazy(request.tenant, request.entity_type)
                .await
        }
    };
    let needs_full_proof = request.query_options.filter.is_some()
        || request.query_options.orderby.is_some()
        || request.query_options.count == Some(true);
    let over_scan_budget = needs_full_proof
        && !should_check_source_cursor_catalog_coverage(all_entity_ids.len(), request.budget);
    // ARN-462: an empty exact-match page must use the gap ∪ native-page union
    // even when the type is *under* `scan_candidate_budget`. The over-budget
    // gate used to be the only caller, so an in-budget type (DesignLanguages
    // 1275 < 10k) hydrated every id and returned 0.
    if reconciling_exact_match_lag || over_scan_budget {
        // ARN-68 (the SessionEntries-list flavor): the full scan is only needed
        // for entities the native page cannot answer for. Reconcile that gap,
        // not the type. A committed-but-unprojected match is FOUND; a genuine
        // miss returns bounded empty. If the union exceeds the scan budget, or
        // the backend cannot say which catalog rows are current, keep the
        // honest over-budget rejection. In-budget probe failure still falls
        // through (no second invented probe).
        if reconciling_exact_match_lag
            && let Some(pairs) = request
                .query_options
                .filter
                .as_ref()
                .and_then(super::filter_sql::equality_field_predicates)
        {
            let field_names: std::collections::BTreeSet<String> =
                pairs.into_iter().map(|(name, _)| name).collect();
            let gap_ids = unvouched_ids(&request, &field_names, &all_entity_ids).await;
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
        if over_scan_budget {
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
    }
    // Every record's catalog row commits with its event, so the catalog
    // covers every id the read lists.
    let coverage = QueryPlaneCoverageReport::default();
    if !reconciling_exact_match_lag
        && !keyed_authoritative_absent
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
    } else if request.query_options.filter.is_some() && native_plan.is_none() {
        QueryPlaneFallbackReason::FilterPushdownUnavailable
    } else if request.state.query_plane_store().is_some() {
        QueryPlaneFallbackReason::NativePageUnavailable
    } else {
        QueryPlaneFallbackReason::NoFilterPushdown
    };
    let result =
        read_from_source_cursor(&request, &all_entity_ids, coverage, &[], fallback_reason).await?;
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
