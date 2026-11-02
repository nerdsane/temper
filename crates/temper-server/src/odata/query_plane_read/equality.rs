//! Equality reads over the indexed matches and the durable projection delta.
use std::collections::BTreeSet;

use super::super::filter_sql::equality_field_predicates;
use super::scan::{
    ScanCounters, TelemetryInput, base_telemetry, decide_row_authz, entity_read_allowed,
    materialize_and_authorize_ids, query_options_with_default_page,
};
use super::types::{
    QueryPlaneCoverageReport, QueryPlaneFallbackReason, QueryPlaneReadError, QueryPlaneReadRequest,
    QueryPlaneReadResult, QueryPlaneReadStrategy,
};
use crate::query_eval::apply_query_options;

// Work per catalog request, never a ceiling on the candidate union.
const CATALOG_READ_BATCH: usize = 256;

// Only conjuncts are required of every matching row. This also retains the
// indexed equality when pagination appends its keyset range/OR expression.
// Never extract an equality from inside an OR: that would miss its other arm.
fn required_equalities(
    filter: &temper_odata::query::types::FilterExpr,
    out: &mut Vec<(String, serde_json::Value)>,
) {
    use temper_odata::query::types::{BinaryOperator, FilterExpr};
    if let FilterExpr::BinaryOp {
        left,
        op: BinaryOperator::And,
        right,
    } = filter
    {
        required_equalities(left, out);
        required_equalities(right, out);
    } else if let Some(pairs) = equality_field_predicates(filter) {
        out.extend(pairs);
    }
}

/// A complete union is required before ordering, paging, counting or selecting:
/// even a nonempty projected page can miss a newer match or contain an old one.
/// The backend returns None when it cannot compare this query plane with the
/// actual journal; the caller then retains the existing conservative scan.
pub(super) async fn try_reconciled_equality_read(
    request: &QueryPlaneReadRequest<'_>,
) -> Option<Result<QueryPlaneReadResult, QueryPlaneReadError>> {
    let defaults = crate::odata::read_defaults::ReadDefaults::for_entity(
        request.state,
        request.tenant,
        request.entity_type,
    );
    let default_fields = defaults.field_values(&serde_json::Value::Null);
    let mut pairs = Vec::new();
    required_equalities(request.query_options.filter.as_ref()?, &mut pairs);
    let predicates: Vec<_> = pairs
        .into_iter()
        .filter_map(|(field, value)| {
            // These aliases resolve from entity metadata rather than indexed fields.
            // Null/non-string/oversized values cannot narrow this proof, and a
            // spec can explicitly omit a field from the projection index.
            if matches!(
                field.as_str(),
                "Id" | "id"
                    | "entity_id"
                    | "Status"
                    | "status"
                    | "entity_type"
                    | "item_count"
                    | "counters"
                    | "booleans"
                    | "lists"
                    | "fields"
                    | "events"
                    | "sequence_nr"
                    | "total_event_count"
                    | "first_event_at"
                    | "last_event_at"
                    | "last_event_via"
                    | "last_event_by"
                    | "last_event_for"
                    | "last_event_agent_type"
                    | "last_event_label"
                    | "timer_needs_person"
            ) || !request.state.query_projection_indexes_field(
                request.tenant,
                request.entity_type,
                &field,
            ) {
                return None;
            }
            let value = value.as_str()?;
            // A current projection can still predate this declaration. Its
            // missing field is not a journal delta, so this equality cannot
            // narrow the union. Other required conjuncts remain usable.
            if default_fields
                .get(&field)
                .and_then(serde_json::Value::as_str)
                == Some(value)
            {
                return None;
            }
            (value.len() <= 2000).then(|| (field, value.to_string()))
        })
        .collect();
    if predicates.is_empty() {
        return None;
    }
    let plane = request.state.query_plane_store()?;
    let (journal, _) = request.state.event_journal()?;
    let budget = request.budget.scan_candidate_budget();
    // The legacy handoff skips native preflight for $count and can reject a
    // counted equality even with no gap. Keep its complete proof on this path.
    let needs_complete_count = request.query_options.count == Some(true);
    let mut probe_budget = Some(budget);
    let (ids, pending) = loop {
        let candidates = match plane
            .reconciled_equality_candidates(
                &journal,
                request.tenant.as_str(),
                request.entity_type,
                &predicates,
                probe_budget,
            )
            .await
        {
            Ok(Some(candidates)) => candidates,
            Ok(None) => return None,
            Err(error) => {
                tracing::warn!(tenant = %request.tenant, entity_type = request.entity_type, error = %error,
                    "projection delta unavailable; falling back to conservative reconciliation");
                return None;
            }
        };
        if probe_budget.is_some()
            && candidates.0.len() > budget
            && (needs_complete_count || !candidates.1.is_empty())
        {
            // The output page size does not bound the work needed for exact
            // count/order (ADR-0176). Stream the complete union from
            // a new snapshot, replacing the probe so no truncated or mixed
            // snapshots can omit a newer match. Only matches plus delta are read.
            probe_budget = None;
            continue;
        }
        break candidates;
    };
    let prefix = request
        .query_options
        .skip
        .unwrap_or(0)
        .saturating_add(request.budget.requested_top(request.query_options));
    if !needs_complete_count && pending.is_empty() && (ids.len() > prefix || ids.len() > budget) {
        // No lag: let the native pager hydrate only the requested page, rather
        // than every matching entity. Pending ids were prioritized even when
        // the candidate budget was hit, so this proves the entire gap is empty.
        return None;
    }
    let mut coverage = QueryPlaneCoverageReport {
        missing: pending.len(),
        matched: 0,
    };
    let mut counters = ScanCounters::empty();
    counters.pushdown_sparse_probe_count = if probe_budget.is_some() { 1 } else { 2 };
    let pending: BTreeSet<_> = pending.into_iter().collect();
    let projected: Vec<_> = ids
        .iter()
        .filter(|id| !pending.contains(*id))
        .cloned()
        .collect();
    counters.pushdown_page_count = projected.len();
    let authz = decide_row_authz(request);
    // Keep all fields until the filter and row authorization have run. A $select
    // must not strip the changed field that decides whether the entity matches.
    let full_options = temper_odata::query::types::QueryOptions {
        select: None,
        ..request.query_options.clone()
    };
    let full_request = QueryPlaneReadRequest {
        query_options: &full_options,
        ..*request
    };
    let mut entities = Vec::new();
    // Page catalog hydration so a complete union never creates one oversized
    // SQL IN clause. This batch size controls work per call, not total matches.
    for page in projected.chunks(CATALOG_READ_BATCH) {
        entities.extend(
            materialize_and_authorize_ids(&full_request, page, true, &authz, &mut counters).await,
        );
    }
    for id in ids.iter().filter(|id| pending.contains(*id)) {
        counters.candidate_count += 1;
        // A legacy/wrapped row may already be current. Validate its checkpoint
        // against the actual journal before using it; mere catalog presence is
        // insufficient for a changed entity.
        let pid = format!("{}:{}:{id}", request.tenant, request.entity_type);
        let latest = match journal.latest_sequence_nr(&pid).await {
            Ok(sequence) => sequence,
            Err(_) => return Some(Err(QueryPlaneReadError::ReadUnavailable)),
        };
        if let Some(entity) = super::super::read_support::try_load_entity_body_from_catalog(
            request.state,
            request.tenant,
            request.entity_type,
            request.entity_set_name,
            id,
            true,
        )
        .await
        .filter(|entity| {
            entity["sequence_nr"]
                .as_u64()
                .is_some_and(|seq| seq >= latest)
        }) {
            counters.materialized_count += 1;
            if entity_read_allowed(request, &entity) {
                entities.push(entity);
            }
            continue;
        }
        let response = match request
            .state
            .get_tenant_entity_state(request.tenant, request.entity_type, id)
            .await
        {
            Ok(response) => response,
            Err(error) => {
                tracing::warn!(tenant = %request.tenant, entity_type = request.entity_type, entity_id = id,
                    error = %error, "authoritative projection delta read failed");
                return Some(Err(QueryPlaneReadError::ReadUnavailable));
            }
        };
        if response.state.sequence_nr < latest {
            return Some(Err(QueryPlaneReadError::ReadUnavailable));
        }
        if response.state.is_tombstoned() {
            continue;
        }
        counters.materialized_count += 1;
        let mut entity = serde_json::to_value(&response.state).expect("entity state serializes");
        defaults.apply(&mut entity);
        entity["@odata.id"] = serde_json::json!(format!("{}('{}')", request.entity_set_name, id));
        if entity_read_allowed(request, &entity) {
            entities.push(entity);
        }
    }
    // Match the source cursor's id order when there is no explicit $orderby.
    entities.sort_by(|a, b| a["entity_id"].as_str().cmp(&b["entity_id"].as_str()));
    let (matching, _) = apply_query_options(
        entities.clone(),
        &super::scan::filter_only_query_options(request.query_options),
    );
    coverage.matched = matching
        .iter()
        .filter(|entity| {
            entity["entity_id"]
                .as_str()
                .is_some_and(|id| pending.contains(id))
        })
        .count();
    let options = query_options_with_default_page(request.query_options, request.budget);
    let (entities, count) = apply_query_options(entities, &options);
    let telemetry = base_telemetry(
        request,
        TelemetryInput {
            strategy: if pending.is_empty() {
                QueryPlaneReadStrategy::NativePagePushdown
            } else {
                QueryPlaneReadStrategy::ReadSourceCursor
            },
            fallback_reason: if pending.is_empty() {
                QueryPlaneFallbackReason::None
            } else {
                QueryPlaneFallbackReason::ProjectionLagReconcile
            },
            filter_pushdown: true,
            catalog_materialization: true,
            coverage,
            counters,
            returned_count: entities.len(),
        },
    );
    Some(Ok(QueryPlaneReadResult {
        entities,
        count,
        telemetry,
        next_skiptoken: None,
    }))
}
