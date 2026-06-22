use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use temper_authz::SecurityContext;
use temper_odata::query::types::QueryOptions;
use temper_runtime::tenant::TenantId;
use tracing::Span;

use crate::response::odata_error;
use crate::state::ServerState;

use super::super::read_support::{odata_default_page_size, odata_max_entities};

/// Explicit budgets for one OData query-plane read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::odata) struct QueryPlaneReadBudget {
    /// Default page size when `$top` is absent.
    pub(in crate::odata::query_plane_read) default_page_size: usize,
    /// Maximum page size accepted by the OData read path.
    pub(in crate::odata::query_plane_read) max_entities: usize,
}

impl QueryPlaneReadBudget {
    /// Load the read budget from OData configuration.
    pub(in crate::odata) fn from_config() -> Self {
        Self {
            default_page_size: odata_default_page_size(),
            max_entities: odata_max_entities(),
        }
    }

    pub(in crate::odata::query_plane_read) fn scan_candidate_budget(self) -> usize {
        self.max_entities
            .saturating_mul(10)
            .max(self.default_page_size)
    }

    pub(in crate::odata::query_plane_read) fn requested_top(
        self,
        query_options: &QueryOptions,
    ) -> usize {
        query_options
            .top
            .unwrap_or(self.default_page_size)
            .min(self.max_entities)
    }

    pub(in crate::odata::query_plane_read) fn scan_page_size(
        self,
        query_options: &QueryOptions,
    ) -> usize {
        self.default_page_size
            .max(self.requested_top(query_options))
            .min(self.scan_candidate_budget())
            .max(1)
    }
}

/// Strategy selected for a query-plane read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::odata::query_plane_read) enum QueryPlaneReadStrategy {
    /// Storage supplied bounded ordered candidate pages.
    NativePagePushdown,
    /// Entity IDs came from the runtime read-source union.
    ReadSourceCursor,
}

impl QueryPlaneReadStrategy {
    pub(in crate::odata::query_plane_read) fn id_source(self) -> &'static str {
        match self {
            Self::NativePagePushdown => "native_page_pushdown",
            Self::ReadSourceCursor => "read_source_cursor",
        }
    }
}

/// Typed fallback or skip reason for a query-plane read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::odata::query_plane_read) enum QueryPlaneFallbackReason {
    /// The primary strategy completed without falling back.
    None,
    /// No query projection filter pushdown was possible.
    NoFilterPushdown,
    /// Storage did not support native candidate pages for this query.
    NativePageUnavailable,
    /// A translated filter could not be pushed into the query projection.
    FilterPushdownUnavailable,
    /// Catalog coverage was incomplete, so catalog-only pages would be unsafe.
    CatalogCoverageGap,
    /// Candidate evaluation exceeded the bounded proof budget.
    FallbackCandidateBudget,
    /// An exact-match resolution returned an empty native page that may be a
    /// lagging projection of a just-committed entity, so the read was
    /// reconciled against authoritative state (ARN-89).
    ProjectionLagReconcile,
}

impl QueryPlaneFallbackReason {
    /// Stable low-cardinality label for spans and metrics.
    pub(in crate::odata::query_plane_read) fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::NoFilterPushdown => "no_filter_pushdown",
            Self::NativePageUnavailable => "native_page_unavailable",
            Self::FilterPushdownUnavailable => "filter_pushdown_unavailable",
            Self::CatalogCoverageGap => "catalog_coverage_gap",
            Self::FallbackCandidateBudget => "fallback_candidate_budget",
            Self::ProjectionLagReconcile => "projection_lag_reconcile",
        }
    }
}

/// Catalog coverage observed before trusting catalog-only candidate pages.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(in crate::odata::query_plane_read) struct QueryPlaneCoverageReport {
    /// IDs missing from the catalog before read planning.
    pub(in crate::odata::query_plane_read) missing: usize,
    /// Missing rows that matched the original query after actor materialization.
    pub(in crate::odata::query_plane_read) matched: usize,
}

/// Telemetry produced by one OData query-plane read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::odata) struct QueryPlaneReadTelemetry {
    pub(in crate::odata::query_plane_read) strategy: QueryPlaneReadStrategy,
    pub(in crate::odata::query_plane_read) fallback_reason: QueryPlaneFallbackReason,
    pub(in crate::odata::query_plane_read) filter_pushdown: bool,
    pub(in crate::odata::query_plane_read) catalog_materialization: bool,
    pub(in crate::odata::query_plane_read) candidate_count: usize,
    pub(in crate::odata::query_plane_read) materialized_count: usize,
    pub(in crate::odata::query_plane_read) returned_count: usize,
    pub(in crate::odata::query_plane_read) catalog_shadow_check_budget: usize,
    pub(in crate::odata::query_plane_read) catalog_shadow_check_scheduled: usize,
    pub(in crate::odata::query_plane_read) coverage: QueryPlaneCoverageReport,
    pub(in crate::odata::query_plane_read) select_requested: bool,
    pub(in crate::odata::query_plane_read) catalog_select_projection: bool,
    pub(in crate::odata::query_plane_read) select_count: usize,
    pub(in crate::odata::query_plane_read) pushdown_sparse_page: bool,
    pub(in crate::odata::query_plane_read) pushdown_sparse_probe_count: usize,
    pub(in crate::odata::query_plane_read) pushdown_page_count: usize,
}

impl QueryPlaneReadTelemetry {
    /// Record this read contract's telemetry onto the current OData span.
    pub(in crate::odata) fn record(&self, span: &Span) {
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
        span.record("select_requested", self.select_requested);
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
pub(in crate::odata) struct QueryPlaneReadRequest<'a> {
    pub(in crate::odata) state: &'a ServerState,
    pub(in crate::odata) tenant: &'a TenantId,
    pub(in crate::odata) security_ctx: &'a SecurityContext,
    pub(in crate::odata) entity_type: &'a str,
    pub(in crate::odata) entity_set_name: &'a str,
    pub(in crate::odata) query_options: &'a QueryOptions,
    pub(in crate::odata) budget: QueryPlaneReadBudget,
}

/// Materialized result for one OData query-plane read.
pub(in crate::odata) struct QueryPlaneReadResult {
    /// Final entities after row authorization and OData query options, before `$expand`.
    pub(in crate::odata) entities: Vec<serde_json::Value>,
    /// `$count` after filtering and authorization, when requested.
    pub(in crate::odata) count: Option<usize>,
    /// Strategy and fallback telemetry for the read.
    pub(in crate::odata) telemetry: QueryPlaneReadTelemetry,
}

/// Error returned by the OData query-plane read contract.
pub(in crate::odata) enum QueryPlaneReadError {
    /// Cedar denied the collection-level list action.
    AuthorizationDenied(Box<Response>),
    /// The read would exceed the bounded candidate proof budget.
    QueryTooLarge { telemetry: QueryPlaneReadTelemetry },
}

impl QueryPlaneReadError {
    pub(in crate::odata) fn record_telemetry(&self, span: &Span) {
        match self {
            Self::AuthorizationDenied(_) => {}
            Self::QueryTooLarge { telemetry, .. } => telemetry.record(span),
        }
    }

    pub(in crate::odata) fn into_response(self) -> Response {
        match self {
            Self::AuthorizationDenied(response) => *response,
            Self::QueryTooLarge { .. } => odata_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "QueryTooLarge",
                "This query requires evaluating more candidate entities than the bounded OData read budget permits. Use a narrower filter or a smaller page/count request.",
            )
            .into_response(),
        }
    }
}
