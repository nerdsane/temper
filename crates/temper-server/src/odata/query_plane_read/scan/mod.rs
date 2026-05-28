mod native;
mod read_source;

pub(super) use native::{native_candidate_page_plan, try_native_page_read};
pub(super) use read_source::read_from_source_cursor;

use temper_odata::query::types::QueryOptions;

use super::super::authz::{READ_ACTION, authorize_read, entity_id_from_body};
use super::super::read_support::materialize_entity_set_entities;
use super::types::{
    QueryPlaneCoverageReport, QueryPlaneFallbackReason, QueryPlaneReadBudget, QueryPlaneReadError,
    QueryPlaneReadRequest, QueryPlaneReadStrategy, QueryPlaneReadTelemetry,
};
use crate::query_eval::apply_query_options;
use crate::storage::QueryFieldIndexOrder;

#[derive(Clone, Debug)]
pub(super) struct NativeCandidatePagePlan {
    pub(super) where_clause: String,
    pub(super) params: Vec<String>,
    pub(super) order_by: Vec<QueryFieldIndexOrder>,
    pub(super) filter_pushdown: bool,
}

pub(super) struct CandidateScanResult {
    pub(super) entities: Vec<serde_json::Value>,
    pub(super) count: Option<usize>,
    pub(super) telemetry: QueryPlaneReadTelemetry,
}

#[derive(Clone, Copy)]
pub(super) struct ScanCounters {
    pub(super) candidate_count: usize,
    pub(super) materialized_count: usize,
    pub(super) catalog_shadow_check_budget: usize,
    pub(super) catalog_shadow_check_scheduled: usize,
    pub(super) pushdown_sparse_probe_count: usize,
    pub(super) pushdown_page_count: usize,
}

impl ScanCounters {
    pub(super) fn empty() -> Self {
        Self {
            candidate_count: 0,
            materialized_count: 0,
            catalog_shadow_check_budget: 0,
            catalog_shadow_check_scheduled: 0,
            pushdown_sparse_probe_count: 0,
            pushdown_page_count: 0,
        }
    }
}

pub(super) struct TelemetryInput {
    pub(super) strategy: QueryPlaneReadStrategy,
    pub(super) fallback_reason: QueryPlaneFallbackReason,
    pub(super) filter_pushdown: bool,
    pub(super) catalog_materialization: bool,
    pub(super) coverage: QueryPlaneCoverageReport,
    pub(super) counters: ScanCounters,
    pub(super) returned_count: usize,
}

pub(super) fn select_requested(query_options: &QueryOptions) -> bool {
    query_options
        .select
        .as_ref()
        .is_some_and(|select| !select.is_empty())
}

fn select_count(query_options: &QueryOptions) -> usize {
    query_options.select.as_ref().map_or(0, Vec::len)
}

pub(super) fn query_options_with_default_page(
    query_options: &QueryOptions,
    budget: QueryPlaneReadBudget,
) -> QueryOptions {
    let mut adjusted = query_options.clone();
    if adjusted.top.is_none() {
        adjusted.top = Some(budget.default_page_size);
    } else if let Some(top) = adjusted.top {
        adjusted.top = Some(top.min(budget.max_entities));
    }
    adjusted
}

pub(super) fn select_only_query_options(query_options: &QueryOptions) -> QueryOptions {
    QueryOptions {
        select: query_options.select.clone(),
        ..QueryOptions::default()
    }
}

pub(super) fn filter_only_query_options(query_options: &QueryOptions) -> QueryOptions {
    QueryOptions {
        filter: query_options.filter.clone(),
        ..QueryOptions::default()
    }
}

pub(super) fn base_telemetry(
    request: &QueryPlaneReadRequest<'_>,
    input: TelemetryInput,
) -> QueryPlaneReadTelemetry {
    QueryPlaneReadTelemetry {
        strategy: input.strategy,
        fallback_reason: input.fallback_reason,
        filter_pushdown: input.filter_pushdown,
        catalog_materialization: input.catalog_materialization,
        candidate_count: input.counters.candidate_count,
        materialized_count: input.counters.materialized_count,
        returned_count: input.returned_count,
        catalog_shadow_check_budget: input.counters.catalog_shadow_check_budget,
        catalog_shadow_check_scheduled: input.counters.catalog_shadow_check_scheduled,
        coverage: input.coverage,
        select_requested: select_requested(request.query_options),
        catalog_select_projection: false,
        select_count: select_count(request.query_options),
        pushdown_sparse_page: input.strategy == QueryPlaneReadStrategy::NativePagePushdown,
        pushdown_sparse_probe_count: input.counters.pushdown_sparse_probe_count,
        pushdown_page_count: input.counters.pushdown_page_count,
    }
}

pub(super) fn budget_rejection(
    request: &QueryPlaneReadRequest<'_>,
    strategy: QueryPlaneReadStrategy,
    filter_pushdown: bool,
    catalog_materialization: bool,
    coverage: QueryPlaneCoverageReport,
    counters: ScanCounters,
) -> QueryPlaneReadError {
    QueryPlaneReadError::QueryTooLarge {
        telemetry: base_telemetry(
            request,
            TelemetryInput {
                strategy,
                fallback_reason: QueryPlaneFallbackReason::FallbackCandidateBudget,
                filter_pushdown,
                catalog_materialization,
                coverage,
                counters,
                returned_count: 0,
            },
        ),
    }
}

fn is_authorized_entity(request: &QueryPlaneReadRequest<'_>, entity: &serde_json::Value) -> bool {
    entity_id_from_body(entity).is_some_and(|entity_id| {
        authorize_read(
            request.state,
            request.tenant,
            request.security_ctx,
            READ_ACTION,
            request.entity_type,
            entity_id,
            entity,
        )
        .is_ok()
    })
}

pub(super) fn apply_select_only(
    entities: Vec<serde_json::Value>,
    query_options: &QueryOptions,
) -> Vec<serde_json::Value> {
    let (entities, _) = apply_query_options(entities, &select_only_query_options(query_options));
    entities
}

pub(super) async fn materialize_and_authorize_ids(
    request: &QueryPlaneReadRequest<'_>,
    entity_ids: &[String],
    prefer_catalog: bool,
    counters: &mut ScanCounters,
) -> Vec<serde_json::Value> {
    counters.candidate_count += entity_ids.len();
    let materialized = materialize_entity_set_entities(
        request.state,
        request.tenant,
        request.entity_type,
        request.entity_set_name,
        entity_ids,
        prefer_catalog,
        None,
    )
    .await;
    counters.materialized_count += materialized.entities.len();
    counters.catalog_shadow_check_budget += materialized.catalog_shadow_check_budget;
    counters.catalog_shadow_check_scheduled += materialized.catalog_shadow_check_scheduled;
    materialized
        .entities
        .into_iter()
        .filter(|entity| is_authorized_entity(request, entity))
        .collect()
}

pub(super) async fn materialize_filter_and_authorize_ids(
    request: &QueryPlaneReadRequest<'_>,
    entity_ids: &[String],
    prefer_catalog: bool,
    counters: &mut ScanCounters,
) -> Vec<serde_json::Value> {
    counters.candidate_count += entity_ids.len();
    let materialized = materialize_entity_set_entities(
        request.state,
        request.tenant,
        request.entity_type,
        request.entity_set_name,
        entity_ids,
        prefer_catalog,
        None,
    )
    .await;
    counters.materialized_count += materialized.entities.len();
    counters.catalog_shadow_check_budget += materialized.catalog_shadow_check_budget;
    counters.catalog_shadow_check_scheduled += materialized.catalog_shadow_check_scheduled;

    let entities = if request.query_options.filter.is_some() {
        let (entities, _) = apply_query_options(
            materialized.entities,
            &filter_only_query_options(request.query_options),
        );
        entities
    } else {
        materialized.entities
    };

    entities
        .into_iter()
        .filter(|entity| is_authorized_entity(request, entity))
        .collect()
}
