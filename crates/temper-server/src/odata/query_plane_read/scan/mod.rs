mod native;
mod read_source;

pub(super) use native::{native_candidate_page_plan, try_native_page_read};
pub(super) use read_source::read_from_source_cursor;

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use temper_authz::{ListRead, RowRule};
use temper_odata::query::types::QueryOptions;

use super::super::authz::{READ_ACTION, entity_id_from_body, resource_attrs_from_body};
use super::super::read_support::{
    catalog_select_projection_fields, materialize_entity_set_entities,
};
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
    /// Fields whose absent index entries require candidate rechecking.
    pub(super) filter_missing_fields: Vec<String>,
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
    pub(super) catalog_select_projection: bool,
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
            catalog_select_projection: false,
        }
    }
}

/// How row `read` decisions apply to one list.
#[derive(Clone)]
pub(super) enum RowAuthz {
    /// One Cedar decision allowed every row of this entity type.
    AllowAll,
    /// One Cedar decision denied every row of this entity type.
    DenyAll,
    /// At least one policy can tell rows apart.
    ///
    /// `attribute_keys` is `Some` when those policies only read the listed
    /// resource attributes (plus the entity id and `has_spec`). `rule` holds
    /// the list-wide part of the decision, evaluated once, and the policies
    /// each row is checked against.
    PerRow {
        attribute_keys: Option<BTreeSet<String>>,
        rule: Arc<RowRule>,
    },
}

/// Decide this list once: the policies that cannot tell rows apart are
/// evaluated a single time, and each row is checked only against the rest.
pub(super) fn decide_row_authz(request: &QueryPlaneReadRequest<'_>) -> RowAuthz {
    let probe = serde_json::json!({
        "entity_id": "_list",
        "status": "",
        "fields": {}
    });
    let probe_attrs = row_attrs(request, "_list", &probe);
    let authz = match request.state.authz.list_read_for_tenant(
        request.tenant.as_str(),
        request.security_ctx,
        READ_ACTION,
        request.entity_type,
        &probe_attrs,
    ) {
        ListRead::AllowAll => RowAuthz::AllowAll,
        ListRead::DenyAll => RowAuthz::DenyAll,
        ListRead::PerRow(rule) => RowAuthz::PerRow {
            attribute_keys: rule.attribute_keys.clone(),
            rule: Arc::from(rule),
        },
    };
    // On `odata.entity_set_read`: how this list's rows were decided, and how
    // many policies each row was checked against (0: the whole set).
    let span = tracing::Span::current();
    match &authz {
        RowAuthz::AllowAll => span.record("row_authz", "allow_all"),
        RowAuthz::DenyAll => span.record("row_authz", "deny_all"),
        RowAuthz::PerRow { rule, .. } => {
            span.record("row_authz", "per_row");
            span.record("row_policies", rule.row_policy_count as u64)
        }
    };
    authz
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
        catalog_select_projection: input.counters.catalog_select_projection,
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

fn row_attrs(
    request: &QueryPlaneReadRequest<'_>,
    entity_id: &str,
    entity: &serde_json::Value,
) -> HashMap<String, serde_json::Value> {
    resource_attrs_from_body(
        request.state,
        request.tenant,
        request.entity_type,
        entity_id,
        entity,
    )
    .into_iter()
    .collect() // determinism-ok: Cedar API requires HashMap
}

/// Whether this list may show `entity`.
pub(super) fn entity_read_allowed(
    request: &QueryPlaneReadRequest<'_>,
    authz: &RowAuthz,
    entity: &serde_json::Value,
) -> bool {
    match authz {
        RowAuthz::AllowAll => true,
        RowAuthz::DenyAll => false,
        RowAuthz::PerRow { rule, .. } => entity_id_from_body(entity).is_some_and(|entity_id| {
            request.state.authz.list_row_allowed(
                request.tenant.as_str(),
                request.security_ctx,
                READ_ACTION,
                request.entity_type,
                rule,
                &row_attrs(request, entity_id, entity),
            )
        }),
    }
}

fn retain_authorized(
    request: &QueryPlaneReadRequest<'_>,
    entities: Vec<serde_json::Value>,
    authz: &RowAuthz,
) -> Vec<serde_json::Value> {
    match authz {
        RowAuthz::AllowAll => entities,
        RowAuthz::DenyAll => Vec::new(),
        RowAuthz::PerRow { .. } => entities
            .into_iter()
            .filter(|entity| entity_read_allowed(request, authz, entity))
            .collect(),
    }
}

fn projection_fields<'a>(
    request: &'a QueryPlaneReadRequest<'_>,
    authz: &RowAuthz,
) -> Option<&'a [String]> {
    let select = catalog_select_projection_fields(request.query_options)?;
    match authz {
        RowAuthz::AllowAll | RowAuthz::DenyAll => Some(select),
        RowAuthz::PerRow {
            attribute_keys: Some(keys),
            ..
        } => keys
            .iter()
            .all(|key| {
                matches!(key.as_str(), "has_spec" | "id" | "Id" | "entity_id")
                    || select.iter().any(|field| field == key)
            })
            .then_some(select),
        RowAuthz::PerRow {
            attribute_keys: None,
            ..
        } => None,
    }
}

pub(super) fn apply_select_only(
    entities: Vec<serde_json::Value>,
    query_options: &QueryOptions,
) -> Vec<serde_json::Value> {
    let (entities, _) = apply_query_options(entities, &select_only_query_options(query_options));
    entities
        .into_iter()
        .map(|mut entity| {
            if entity.get("entity_id").is_none()
                && let Some(entity_id) = entity_id_from_odata_id(&entity)
                && let Some(object) = entity.as_object_mut()
            {
                object.insert("entity_id".to_string(), serde_json::json!(entity_id));
            }
            entity
        })
        .collect()
}

fn entity_id_from_odata_id(entity: &serde_json::Value) -> Option<String> {
    let odata_id = entity.get("@odata.id")?.as_str()?;
    let (_, rest) = odata_id.split_once('(')?;
    let inner = rest.strip_suffix(')')?;
    Some(inner.trim_matches('\'').to_string())
}

pub(super) async fn materialize_and_authorize_ids(
    request: &QueryPlaneReadRequest<'_>,
    entity_ids: &[String],
    prefer_catalog: bool,
    authz: &RowAuthz,
    counters: &mut ScanCounters,
) -> Vec<serde_json::Value> {
    counters.candidate_count += entity_ids.len();
    if matches!(authz, RowAuthz::DenyAll) || entity_ids.is_empty() {
        return Vec::new();
    }
    let projection = projection_fields(request, authz);
    let materialized = materialize_entity_set_entities(
        request.state,
        request.tenant,
        request.entity_type,
        request.entity_set_name,
        entity_ids,
        prefer_catalog,
        projection,
    )
    .await;
    counters.materialized_count += materialized.entities.len();
    counters.catalog_shadow_check_budget += materialized.catalog_shadow_check_budget;
    counters.catalog_shadow_check_scheduled += materialized.catalog_shadow_check_scheduled;
    counters.catalog_select_projection |= materialized.catalog_select_projection;
    retain_authorized(request, materialized.entities, authz)
}

pub(super) async fn materialize_filter_and_authorize_ids(
    request: &QueryPlaneReadRequest<'_>,
    entity_ids: &[String],
    prefer_catalog: bool,
    authz: &RowAuthz,
    counters: &mut ScanCounters,
) -> Vec<serde_json::Value> {
    counters.candidate_count += entity_ids.len();
    if matches!(authz, RowAuthz::DenyAll) || entity_ids.is_empty() {
        return Vec::new();
    }
    let projection = projection_fields(request, authz);
    let materialized = materialize_entity_set_entities(
        request.state,
        request.tenant,
        request.entity_type,
        request.entity_set_name,
        entity_ids,
        prefer_catalog,
        projection,
    )
    .await;
    counters.materialized_count += materialized.entities.len();
    counters.catalog_shadow_check_budget += materialized.catalog_shadow_check_budget;
    counters.catalog_shadow_check_scheduled += materialized.catalog_shadow_check_scheduled;
    counters.catalog_select_projection |= materialized.catalog_select_projection;

    let entities = if request.query_options.filter.is_some() {
        let (entities, _) = apply_query_options(
            materialized.entities,
            &filter_only_query_options(request.query_options),
        );
        entities
    } else {
        materialized.entities
    };

    retain_authorized(request, entities, authz)
}
