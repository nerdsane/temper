//! OData entity-set reads through the query-plane contract.

use std::collections::BTreeSet;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use temper_authz::SecurityContext;
use temper_odata::query::types::QueryOptions;
use temper_runtime::tenant::TenantId;
use tracing::Span;

use super::authz::{LIST_ACTION, READ_ACTION, authorize_read, entity_id_from_body};
use super::read_support::{
    catalog_select_projection_fields, materialize_entity_set_entities, missing_catalog_entity_ids,
    odata_default_page_size, odata_max_entities, select_entity_ids_for_materialization,
};
use crate::query_eval::apply_query_options;
use crate::response::odata_error;
use crate::state::ServerState;

/// Explicit budgets for one OData query-plane read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct QueryPlaneReadBudget {
    /// Default page size when `$top` is absent.
    pub(super) default_page_size: usize,
    /// Maximum page size accepted by the OData read path.
    pub(super) max_entities: usize,
}

impl QueryPlaneReadBudget {
    /// Load the read budget from OData configuration.
    pub(super) fn from_config() -> Self {
        Self {
            default_page_size: odata_default_page_size(),
            max_entities: odata_max_entities(),
        }
    }

    fn fallback_candidate_budget(self) -> usize {
        self.max_entities
            .saturating_mul(10)
            .max(self.default_page_size)
    }
}

/// Strategy selected for a query-plane read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum QueryPlaneReadStrategy {
    /// Query projection supplied filtered candidate IDs.
    FilterIdPushdown,
    /// Entity IDs came from the runtime read-source union.
    ReadSourceUnion,
}

impl QueryPlaneReadStrategy {
    fn id_source(self) -> &'static str {
        match self {
            Self::FilterIdPushdown => "filter_pushdown",
            Self::ReadSourceUnion => "read_source_union",
        }
    }
}

/// Typed fallback or skip reason for a query-plane read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum QueryPlaneFallbackReason {
    /// No query projection filter pushdown was used.
    NoFilterPushdown,
    /// Row authorization must run before paging/count, so page pushdown is skipped.
    CedarRowAuthorization,
    /// Candidate set exceeded the bounded fallback budget.
    FallbackCandidateBudget,
}

impl QueryPlaneFallbackReason {
    /// Stable low-cardinality label for spans and metrics.
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::NoFilterPushdown => "no_filter_pushdown",
            Self::CedarRowAuthorization => "cedar_row_authorization",
            Self::FallbackCandidateBudget => "fallback_candidate_budget",
        }
    }
}

/// Catalog coverage observed while reconciling pushed-down filter results.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct QueryPlaneCoverageReport {
    /// IDs missing from the catalog before repair materialization.
    pub(super) missing: usize,
    /// Missing rows that matched the original filter after actor materialization.
    pub(super) matched: usize,
}

/// Telemetry produced by one OData query-plane read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct QueryPlaneReadTelemetry {
    pub(super) strategy: QueryPlaneReadStrategy,
    pub(super) fallback_reason: QueryPlaneFallbackReason,
    pub(super) filter_pushdown: bool,
    pub(super) catalog_materialization: bool,
    pub(super) candidate_count: usize,
    pub(super) materialized_count: usize,
    pub(super) returned_count: usize,
    pub(super) catalog_shadow_check_budget: usize,
    pub(super) catalog_shadow_check_scheduled: usize,
    pub(super) coverage: QueryPlaneCoverageReport,
    pub(super) catalog_select_projection: bool,
    pub(super) select_count: usize,
    pub(super) pushdown_sparse_page: bool,
    pub(super) pushdown_sparse_probe_count: usize,
    pub(super) pushdown_page_count: usize,
}

impl QueryPlaneReadTelemetry {
    /// Record this read contract's telemetry onto the current OData span.
    pub(super) fn record(&self, span: &Span) {
        span.record("filter_pushdown", self.filter_pushdown);
        span.record("catalog_materialization", self.catalog_materialization);
        span.record("id_source", self.strategy.id_source());
        span.record("candidate_count", self.candidate_count as u64);
        span.record("materialized_count", self.materialized_count as u64);
        span.record("returned_count", self.returned_count as u64);
        span.record(
            "catalog_shadow_check_budget",
            self.catalog_shadow_check_budget as u64,
        );
        span.record(
            "catalog_shadow_check_scheduled",
            self.catalog_shadow_check_scheduled as u64,
        );
        span.record("catalog_coverage_missing", self.coverage.missing as u64);
        span.record("catalog_coverage_matched", self.coverage.matched as u64);
        span.record("catalog_select_projection", self.catalog_select_projection);
        span.record("select_count", self.select_count as u64);
        span.record("pushdown_sparse_page", self.pushdown_sparse_page);
        span.record(
            "pushdown_sparse_probe_count",
            self.pushdown_sparse_probe_count as u64,
        );
        span.record("pushdown_page_count", self.pushdown_page_count as u64);
        span.record("pushdown_sparse_skip_reason", self.fallback_reason.as_str());
    }
}

/// Request for one OData query-plane read.
pub(super) struct QueryPlaneReadRequest<'a> {
    pub(super) state: &'a ServerState,
    pub(super) tenant: &'a TenantId,
    pub(super) security_ctx: &'a SecurityContext,
    pub(super) entity_type: &'a str,
    pub(super) entity_set_name: &'a str,
    pub(super) query_options: &'a QueryOptions,
    pub(super) budget: QueryPlaneReadBudget,
}

/// Materialized result for one OData query-plane read.
pub(super) struct QueryPlaneReadResult {
    /// Final entities after row authorization and OData query options, before `$expand`.
    pub(super) entities: Vec<serde_json::Value>,
    /// `$count` after filtering and authorization, when requested.
    pub(super) count: Option<usize>,
    /// Strategy and fallback telemetry for the read.
    pub(super) telemetry: QueryPlaneReadTelemetry,
}

/// Error returned by the OData query-plane read contract.
pub(super) enum QueryPlaneReadError {
    /// Cedar denied the collection-level list action.
    AuthorizationDenied(Box<Response>),
    /// The read would exceed the bounded fallback candidate budget.
    QueryTooLarge { telemetry: QueryPlaneReadTelemetry },
}

impl QueryPlaneReadError {
    pub(super) fn record_telemetry(&self, span: &Span) {
        match self {
            Self::AuthorizationDenied(_) => {}
            Self::QueryTooLarge { telemetry, .. } => telemetry.record(span),
        }
    }

    pub(super) fn into_response(self) -> Response {
        match self {
            Self::AuthorizationDenied(response) => *response,
            Self::QueryTooLarge { .. } => odata_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "QueryTooLarge",
                "This filtered query matched more projected entities than the bounded fallback can materialize. Use a narrower filter or a storage backend with native paged push-down.",
            )
            .into_response(),
        }
    }
}

fn filter_only_query_options(query_options: &QueryOptions) -> QueryOptions {
    QueryOptions {
        filter: query_options.filter.clone(),
        ..QueryOptions::default()
    }
}

async fn try_filter_pushdown_ids(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    query_options: &QueryOptions,
) -> Option<Vec<String>> {
    let translated = query_options
        .filter
        .as_ref()
        .and_then(super::filter_sql::try_translate_filter)?;
    let query_plane = state.query_plane_store()?;

    match query_plane
        .query_field_index(
            tenant.as_str(),
            entity_type,
            &translated.where_clause,
            translated.params,
        )
        .await
    {
        Ok(Some(ids)) => {
            tracing::debug!(
                entity_type = %entity_type,
                matched = ids.len(),
                "OData filter push-down succeeded"
            );
            Some(ids)
        }
        Ok(None) => None,
        Err(error) => {
            tracing::warn!(
                error = %error,
                "OData filter push-down query failed, falling back to in-memory"
            );
            None
        }
    }
}

fn telemetry_for_budget_rejection(
    request: &QueryPlaneReadRequest<'_>,
    candidate_count: usize,
) -> QueryPlaneReadTelemetry {
    QueryPlaneReadTelemetry {
        strategy: QueryPlaneReadStrategy::FilterIdPushdown,
        fallback_reason: QueryPlaneFallbackReason::FallbackCandidateBudget,
        filter_pushdown: true,
        catalog_materialization: true,
        candidate_count,
        materialized_count: 0,
        returned_count: 0,
        catalog_shadow_check_budget: 0,
        catalog_shadow_check_scheduled: 0,
        coverage: QueryPlaneCoverageReport::default(),
        catalog_select_projection: catalog_select_projection_fields(request.query_options)
            .is_some(),
        select_count: request.query_options.select.as_ref().map_or(0, Vec::len),
        pushdown_sparse_page: false,
        pushdown_sparse_probe_count: 0,
        pushdown_page_count: 0,
    }
}

/// Execute one OData entity-set read through the query-plane contract.
pub(super) async fn read_entity_set_from_query_plane(
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

    let sql_pushdown_ids = try_filter_pushdown_ids(
        request.state,
        request.tenant,
        request.entity_type,
        request.query_options,
    )
    .await;

    let filter_pushdown = sql_pushdown_ids.is_some();
    let prefer_catalog_materialization =
        filter_pushdown || request.state.query_plane_store().is_some();
    let mut coverage_entities = Vec::new();
    let mut coverage = QueryPlaneCoverageReport::default();
    let fallback_reason = if filter_pushdown {
        QueryPlaneFallbackReason::CedarRowAuthorization
    } else {
        QueryPlaneFallbackReason::NoFilterPushdown
    };

    let strategy;
    let candidate_count_for_span;
    let (entity_ids, apply_options, precomputed_count) = if let Some(pushed_ids) = sql_pushdown_ids
    {
        strategy = QueryPlaneReadStrategy::FilterIdPushdown;
        candidate_count_for_span = pushed_ids.len();
        let fallback_candidate_budget = request.budget.fallback_candidate_budget();
        if pushed_ids.len() > fallback_candidate_budget {
            tracing::warn!(
                tenant = %request.tenant,
                entity_type = %request.entity_type,
                candidate_count = pushed_ids.len(),
                fallback_candidate_budget,
                "OData pushed-down candidate set requires native paged push-down"
            );
            return Err(QueryPlaneReadError::QueryTooLarge {
                telemetry: telemetry_for_budget_rejection(&request, pushed_ids.len()),
            });
        }

        let all_entity_ids = request
            .state
            .list_entity_ids_lazy(request.tenant, request.entity_type)
            .await;
        let pushed_id_set = pushed_ids.iter().cloned().collect::<BTreeSet<_>>();
        let mut missing_ids = missing_catalog_entity_ids(
            request.state,
            request.tenant,
            request.entity_type,
            &all_entity_ids,
        )
        .await;
        missing_ids.retain(|id| !pushed_id_set.contains(id));
        coverage.missing = missing_ids.len();
        if !missing_ids.is_empty() {
            let missing = materialize_entity_set_entities(
                request.state,
                request.tenant,
                request.entity_type,
                request.entity_set_name,
                &missing_ids,
                true,
                None,
            )
            .await;
            let filter_options = filter_only_query_options(request.query_options);
            let (matching_missing, _) = apply_query_options(missing.entities, &filter_options);
            coverage.matched = matching_missing
                .iter()
                .filter_map(|entity| entity.get("entity_id").and_then(|id| id.as_str()))
                .count();
            coverage_entities = matching_missing;
        }

        let mut opts = request.query_options.clone();
        if opts.top.is_none() {
            opts.top = Some(request.budget.default_page_size);
        } else if let Some(top) = opts.top {
            opts.top = Some(top.min(request.budget.max_entities));
        }
        (pushed_ids, opts, None)
    } else {
        strategy = QueryPlaneReadStrategy::ReadSourceUnion;
        let selected = select_entity_ids_for_materialization(
            request
                .state
                .list_entity_ids_lazy(request.tenant, request.entity_type)
                .await,
            request.query_options,
            request.budget.default_page_size,
            request.budget.max_entities,
            true,
        );
        candidate_count_for_span = selected.0.len();
        selected
    };

    // Cedar policies may inspect fields omitted by `$select`; selected catalog
    // projection can only be reintroduced when authorization has the same data.
    let selected_catalog_fields = None;
    let materialized = materialize_entity_set_entities(
        request.state,
        request.tenant,
        request.entity_type,
        request.entity_set_name,
        &entity_ids,
        prefer_catalog_materialization,
        selected_catalog_fields,
    )
    .await;

    let coverage_entity_count = coverage_entities.len();
    let materialized_count = materialized.entities.len() + coverage_entity_count;
    let mut entities = materialized.entities;
    entities.extend(coverage_entities);
    entities.retain(|entity| {
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
    });

    let (entities, mut count) = apply_query_options(entities, &apply_options);
    if count.is_none() {
        count = precomputed_count;
    }
    let returned_count = entities.len();

    let telemetry = QueryPlaneReadTelemetry {
        strategy,
        fallback_reason,
        filter_pushdown,
        catalog_materialization: prefer_catalog_materialization,
        candidate_count: candidate_count_for_span + coverage_entity_count,
        materialized_count,
        returned_count,
        catalog_shadow_check_budget: materialized.catalog_shadow_check_budget,
        catalog_shadow_check_scheduled: materialized.catalog_shadow_check_scheduled,
        coverage,
        catalog_select_projection: catalog_select_projection_fields(request.query_options)
            .is_some(),
        select_count: request.query_options.select.as_ref().map_or(0, Vec::len),
        pushdown_sparse_page: false,
        pushdown_sparse_probe_count: 0,
        pushdown_page_count: 0,
    };

    Ok(QueryPlaneReadResult {
        entities,
        count,
        telemetry,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_reason_labels_are_stable() {
        assert_eq!(
            QueryPlaneFallbackReason::NoFilterPushdown.as_str(),
            "no_filter_pushdown"
        );
        assert_eq!(
            QueryPlaneFallbackReason::CedarRowAuthorization.as_str(),
            "cedar_row_authorization"
        );
        assert_eq!(
            QueryPlaneFallbackReason::FallbackCandidateBudget.as_str(),
            "fallback_candidate_budget"
        );
    }

    #[test]
    fn fallback_candidate_budget_matches_existing_odata_cap() {
        let small_default = QueryPlaneReadBudget {
            default_page_size: 100,
            max_entities: 1000,
        };
        assert_eq!(small_default.fallback_candidate_budget(), 10_000);

        let large_default = QueryPlaneReadBudget {
            default_page_size: 20_000,
            max_entities: 1000,
        };
        assert_eq!(large_default.fallback_candidate_budget(), 20_000);
    }
}
