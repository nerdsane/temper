use temper_odata::query::types::OrderDirection;

use super::super::super::filter_sql;
use super::super::types::{QueryPlaneCoverageReport, QueryPlaneReadError};
use super::{
    CandidateScanResult, NativeCandidatePagePlan, RowAuthz, ScanCounters, TelemetryInput,
    apply_select_only, base_telemetry, budget_rejection, decide_row_authz,
    materialize_filter_and_authorize_ids,
};
use crate::odata::query_plane_read::types::{
    QueryPlaneFallbackReason, QueryPlaneReadRequest, QueryPlaneReadStrategy,
};
use crate::storage::{QueryFieldIndexOrder, QueryFieldIndexOrderDirection, QueryFieldIndexPage};

fn native_order_by(request: &QueryPlaneReadRequest<'_>) -> Option<Vec<QueryFieldIndexOrder>> {
    let query_options = request.query_options;
    query_options
        .orderby
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|clause| {
            native_order_property_is_lossless(request, &clause.property).then(|| {
                QueryFieldIndexOrder {
                    field_name: clause.property.clone(),
                    direction: match clause.direction {
                        OrderDirection::Asc => QueryFieldIndexOrderDirection::Asc,
                        OrderDirection::Desc => QueryFieldIndexOrderDirection::Desc,
                    },
                }
            })
        })
        .collect()
}

fn native_order_property_is_lossless(request: &QueryPlaneReadRequest<'_>, property: &str) -> bool {
    // Kernel event metadata is in the catalog state, never the app field index.
    if temper_spec::automaton::is_event_metadata_field(property) {
        return false;
    }
    if matches!(property, "entity_id" | "Id" | "id" | "Status" | "status") {
        return true;
    }

    let registry = request
        .state
        .registry
        .read()
        .expect("registry lock should not be poisoned"); // ci-ok: infallible lock
    registry
        .get_tenant(request.tenant)
        .and_then(|tenant_config| {
            tenant_config
                .csdl
                .schemas
                .iter()
                .flat_map(|schema| &schema.entity_types)
                .find(|entity_type| entity_type.name == request.entity_type)
        })
        .and_then(|entity_type| {
            entity_type
                .properties
                .iter()
                .find(|candidate| candidate.name == property)
        })
        .is_some_and(|property| {
            !property.nullable && native_order_type_is_lossless(&property.type_name)
        })
}

fn native_order_type_is_lossless(type_name: &str) -> bool {
    matches!(
        type_name,
        "Edm.String"
            | "Edm.Guid"
            | "Edm.Date"
            | "Edm.DateTimeOffset"
            | "Edm.TimeOfDay"
            | "Edm.Byte"
            | "Edm.SByte"
            | "Edm.Int16"
            | "Edm.Int32"
            | "Edm.Int64"
            | "Edm.Decimal"
            | "Edm.Double"
            | "Edm.Single"
    )
}

pub(in crate::odata::query_plane_read) fn native_candidate_page_plan(
    request: &QueryPlaneReadRequest<'_>,
) -> Option<NativeCandidatePagePlan> {
    let query_options = request.query_options;
    let defaults = crate::odata::read_defaults::ReadDefaults::for_entity(
        request.state,
        request.tenant,
        request.entity_type,
    )
    .field_values(&serde_json::Value::Null);
    let order_by = native_order_by(request)?;
    let (where_clause, params, filter_pushdown, filter_missing_fields) =
        match query_options.filter.as_ref() {
            Some(filter) => {
                if let Some(translated) =
                    filter_sql::try_translate_candidate_filter_with_defaults(filter, &defaults)
                {
                    // Missing index entries include omitted/oversized stored fields.
                    // Recheck candidates before counting or applying skip/top.
                    (
                        translated.where_clause,
                        translated.params,
                        true,
                        translated
                            .missing_default_fields
                            .into_iter()
                            .collect::<Vec<_>>(),
                    )
                } else {
                    ("TRUE".to_string(), Vec::new(), false, Vec::new())
                }
            }
            None => ("TRUE".to_string(), Vec::new(), false, Vec::new()),
        };
    Some(NativeCandidatePagePlan {
        where_clause,
        params,
        order_by,
        filter_pushdown,
        filter_missing_fields,
    })
}

async fn try_query_native_page(
    request: &QueryPlaneReadRequest<'_>,
    plan: &NativeCandidatePagePlan,
    skip: usize,
    top: usize,
    include_count: bool,
) -> Result<Option<QueryFieldIndexPage>, ()> {
    let Some(query_plane) = request.state.query_plane_store() else {
        return Ok(None);
    };
    query_plane
        .query_field_index_page(
            request.tenant.as_str(),
            request.entity_type,
            &plan.where_clause,
            plan.params.clone(),
            &plan.order_by,
            skip,
            top,
            include_count,
        )
        .await
        .map_err(|error| {
            tracing::warn!(
                error = %error,
                tenant = %request.tenant,
                entity_type = %request.entity_type,
                "native OData query-plane page failed; falling back"
            );
        })
}

/// A bounded probe detects sort values that need default materialization.
/// Filter completeness is checked inside the uniform page's SQL snapshot.
async fn native_values_present(
    request: &QueryPlaneReadRequest<'_>,
    plan: &NativeCandidatePagePlan,
    fields: &[String],
) -> Option<bool> {
    for field in fields {
        let mut probe = plan.clone();
        let field_param = probe.params.len() + 3;
        probe.params.push(field.clone());
        probe.where_clause = format!(
            "({}) AND entity_id NOT IN (SELECT entity_id FROM entity_field_index \
             WHERE tenant = ?1 AND entity_type = ?2 AND field_name = ?{field_param})",
            probe.where_clause,
        );
        probe.order_by.clear();
        let page = try_query_native_page(request, &probe, 0, 1, false)
            .await
            .ok()??;
        if !page.entity_ids.is_empty() {
            return Some(false);
        }
    }
    Some(true)
}

/// Suppress the uniform page when any candidate needs its effective values
/// rechecked. The proof and window count run in the same SQL statement, so a
/// write cannot invalidate a separate completeness probe before counting.
fn uniform_page_plan(plan: &NativeCandidatePagePlan) -> NativeCandidatePagePlan {
    let mut uniform = plan.clone();
    if plan.filter_missing_fields.is_empty() {
        return uniform;
    }
    let gaps = plan
        .filter_missing_fields
        .iter()
        .map(|field| {
            let field_param = uniform.params.len() + 3;
            uniform.params.push(field.clone());
            format!(
                "entity_id NOT IN (SELECT entity_id FROM entity_field_index \
                 WHERE tenant = ?1 AND entity_type = ?2 AND field_name = ?{field_param})"
            )
        })
        .collect::<Vec<_>>()
        .join(" OR ");
    uniform.where_clause = format!(
        "({}) AND NOT EXISTS (SELECT 1 FROM entity_catalog \
         WHERE tenant = ?1 AND entity_type = ?2 AND ({}) AND ({gaps}))",
        plan.where_clause, plan.where_clause,
    );
    uniform
}

/// One SQL page when every row is allowed and the filter, if any, is already
/// in the query. `$count` then comes from the window count instead of a
/// second pass over every document.
async fn try_uniform_native_page(
    request: &QueryPlaneReadRequest<'_>,
    plan: &NativeCandidatePagePlan,
    coverage: QueryPlaneCoverageReport,
    authz: &RowAuthz,
    requested_top: usize,
    skip: usize,
    include_count: bool,
) -> Option<Result<CandidateScanResult, QueryPlaneReadError>> {
    if matches!(authz, RowAuthz::DenyAll) {
        let counters = ScanCounters::empty();
        let telemetry = base_telemetry(
            request,
            TelemetryInput {
                strategy: QueryPlaneReadStrategy::NativePagePushdown,
                fallback_reason: QueryPlaneFallbackReason::None,
                filter_pushdown: plan.filter_pushdown,
                catalog_materialization: true,
                coverage,
                counters,
                returned_count: 0,
            },
        );
        return Some(Ok(CandidateScanResult {
            entities: Vec::new(),
            count: include_count.then_some(0),
            telemetry,
        }));
    }
    if !matches!(authz, RowAuthz::AllowAll) {
        return None;
    }
    if request
        .query_options
        .filter
        .as_ref()
        .is_some_and(|filter| !filter_sql::candidate_filter_is_lossless(filter))
    {
        return None;
    }
    if requested_top == 0 {
        return None;
    }

    let uniform_plan = uniform_page_plan(plan);
    let mut page =
        match try_query_native_page(request, &uniform_plan, skip, requested_top, include_count)
            .await
        {
            Ok(Some(page)) => page,
            _ => return None,
        };
    if include_count && page.total_count.is_none() {
        return None;
    }
    let mut page_queries = 1;
    let mut page_ids = page.entity_ids.len();
    // An empty guarded page may mean the proof failed or skip passed the
    // last match. A guarded first page can prove the latter in its own
    // snapshot, retaining bounded counts without materializing documents.
    if !plan.filter_missing_fields.is_empty() && page.entity_ids.is_empty() {
        if skip == 0 {
            return None;
        }
        let first_page = try_query_native_page(request, &uniform_plan, 0, 1, true)
            .await
            .ok()??;
        let count = first_page.total_count?;
        if first_page.entity_ids.is_empty() || count > skip {
            return None;
        }
        page.total_count = Some(count);
        page_queries += 1;
        page_ids += first_page.entity_ids.len();
    }

    let mut counters = ScanCounters::empty();
    counters.pushdown_sparse_probe_count = page_queries;
    counters.pushdown_page_count = page_ids;
    let authorized =
        materialize_filter_and_authorize_ids(request, &page.entity_ids, true, authz, &mut counters)
            .await;
    if authorized.len() != page.entity_ids.len() {
        return None;
    }

    let count = include_count.then_some(page.total_count.unwrap_or(authorized.len()));
    let entities = apply_select_only(authorized, request.query_options);
    let returned_count = entities.len();
    let telemetry = base_telemetry(
        request,
        TelemetryInput {
            strategy: QueryPlaneReadStrategy::NativePagePushdown,
            fallback_reason: QueryPlaneFallbackReason::None,
            filter_pushdown: plan.filter_pushdown,
            catalog_materialization: true,
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

pub(in crate::odata::query_plane_read) async fn try_native_page_read(
    request: &QueryPlaneReadRequest<'_>,
    plan: NativeCandidatePagePlan,
    coverage: QueryPlaneCoverageReport,
) -> Option<Result<CandidateScanResult, QueryPlaneReadError>> {
    let requested_top = request.budget.requested_top(request.query_options);
    let skip = request.query_options.skip.unwrap_or(0);
    let include_count = request.query_options.count == Some(true);
    if requested_top == 0 && !include_count {
        let counters = ScanCounters::empty();
        let telemetry = base_telemetry(
            request,
            TelemetryInput {
                strategy: QueryPlaneReadStrategy::NativePagePushdown,
                fallback_reason: QueryPlaneFallbackReason::None,
                filter_pushdown: plan.filter_pushdown,
                catalog_materialization: true,
                coverage,
                counters,
                returned_count: 0,
            },
        );
        return Some(Ok(CandidateScanResult {
            entities: Vec::new(),
            count: None,
            telemetry,
        }));
    }

    let authz = decide_row_authz(request);
    if !matches!(authz, RowAuthz::DenyAll) {
        let defaults = crate::odata::read_defaults::ReadDefaults::for_entity(
            request.state,
            request.tenant,
            request.entity_type,
        )
        .field_values(&serde_json::Value::Null);
        let sort_fields = plan
            .order_by
            .iter()
            .filter(|clause| defaults.get(&clause.field_name).is_some())
            .map(|clause| clause.field_name.clone())
            .collect::<Vec<_>>();
        if !native_values_present(request, &plan, &sort_fields).await? {
            return None;
        }
    }
    if let Some(result) = try_uniform_native_page(
        request,
        &plan,
        coverage,
        &authz,
        requested_top,
        skip,
        include_count,
    )
    .await
    {
        return Some(result);
    }

    let scan_budget = request.budget.scan_candidate_budget();
    let page_size = request.budget.scan_page_size(request.query_options);
    let mut storage_offset = 0;
    let mut authorized_seen = 0usize;
    let mut returned_before_select = Vec::new();
    let mut counters = ScanCounters::empty();

    loop {
        let remaining_budget = scan_budget.saturating_sub(counters.candidate_count);
        if remaining_budget == 0 {
            return Some(Err(budget_rejection(
                request,
                QueryPlaneReadStrategy::NativePagePushdown,
                plan.filter_pushdown,
                true,
                coverage,
                counters,
            )));
        }
        let storage_top = page_size.min(remaining_budget);
        let page =
            match try_query_native_page(request, &plan, storage_offset, storage_top, include_count)
                .await
            {
                Ok(Some(page)) => page,
                Ok(None) => return None,
                Err(()) => return None,
            };
        counters.pushdown_sparse_probe_count += 1;
        counters.pushdown_page_count += page.entity_ids.len();

        if page.entity_ids.is_empty() {
            break;
        }

        let page_len = page.entity_ids.len();
        let authorized = materialize_filter_and_authorize_ids(
            request,
            &page.entity_ids,
            true,
            &authz,
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

        storage_offset += page_len;
        if !include_count && returned_before_select.len() >= requested_top {
            break;
        }
        if page_len < storage_top {
            break;
        }
    }

    let count = include_count.then_some(authorized_seen);
    let entities = apply_select_only(returned_before_select, request.query_options);
    let returned_count = entities.len();
    let telemetry = base_telemetry(
        request,
        TelemetryInput {
            strategy: QueryPlaneReadStrategy::NativePagePushdown,
            fallback_reason: QueryPlaneFallbackReason::None,
            filter_pushdown: plan.filter_pushdown,
            catalog_materialization: true,
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
