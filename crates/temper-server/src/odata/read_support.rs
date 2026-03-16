//! Shared helpers for OData read handlers.

use std::sync::OnceLock;

use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;

use crate::state::ServerState;
use crate::state::trajectory::{TrajectoryEntry, TrajectorySource};

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name) // determinism-ok: read once at startup
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default)
}

pub(super) fn odata_default_page_size() -> usize {
    static DEFAULT_PAGE_SIZE: OnceLock<usize> = OnceLock::new();
    *DEFAULT_PAGE_SIZE.get_or_init(|| env_usize("TEMPER_ODATA_DEFAULT_PAGE_SIZE", 100))
}

pub(super) fn odata_max_entities() -> usize {
    static MAX_ENTITIES: OnceLock<usize> = OnceLock::new();
    *MAX_ENTITIES.get_or_init(|| env_usize("TEMPER_ODATA_MAX_ENTITIES", 1000))
}

pub(super) fn select_entity_ids_for_materialization(
    mut entity_ids: Vec<String>,
    query_options: &temper_odata::query::types::QueryOptions,
    default_page_size: usize,
    max_entities: usize,
) -> (
    Vec<String>,
    temper_odata::query::types::QueryOptions,
    Option<usize>,
) {
    let has_filter_or_order = query_options.filter.is_some() || query_options.orderby.is_some();
    let mut precomputed_count = None;

    let apply_options = if !has_filter_or_order {
        let total_available = entity_ids.len();
        if query_options.count == Some(true) {
            precomputed_count = Some(total_available);
        }

        let skip = query_options.skip.unwrap_or(0);
        let top = query_options.top.unwrap_or(default_page_size);
        let requested = top.min(max_entities);
        entity_ids = entity_ids
            .into_iter()
            .skip(skip)
            .take(requested)
            .collect::<Vec<_>>();

        let mut adjusted = query_options.clone();
        adjusted.skip = None;
        adjusted.top = None;
        adjusted.count = None;
        adjusted
    } else {
        if entity_ids.len() > max_entities {
            entity_ids.truncate(max_entities);
        }

        let mut adjusted = query_options.clone();
        if adjusted.top.is_none() {
            adjusted.top = Some(default_page_size);
        } else if let Some(top) = adjusted.top {
            adjusted.top = Some(top.min(max_entities));
        }
        adjusted
    };

    (entity_ids, apply_options, precomputed_count)
}

/// Resolve an entity set name from an entity type name.
///
/// Reverse-lookups the entity_set_map to find the set name for a given type.
pub(super) fn resolve_entity_set_name(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
) -> String {
    let registry = state.registry.read().unwrap(); // ci-ok: infallible lock
    if let Some(tc) = registry.get_tenant(tenant) {
        for (set_name, type_name) in &tc.entity_set_map {
            if type_name == entity_type {
                return set_name.clone();
            }
        }
    }
    // Fallback: pluralize entity type
    format!("{entity_type}s")
}

/// Record a trajectory entry for an EntitySetNotFound error.
pub(super) async fn record_entity_set_not_found(state: &ServerState, tenant: &str, set_name: &str) {
    tracing::warn!(tenant = %tenant, entity_set = %set_name, "entity set not found");
    let entry = TrajectoryEntry {
        timestamp: sim_now().to_rfc3339(),
        tenant: tenant.to_string(),
        entity_type: set_name.to_string(),
        entity_id: "".to_string(),
        action: "EntitySetLookup".to_string(),
        success: false,
        from_status: None,
        to_status: None,
        error: Some(format!(
            "EntitySetNotFound: entity set '{}' not found",
            set_name
        )),
        agent_id: None,
        session_id: None,
        authz_denied: None,
        denied_resource: None,
        denied_module: None,
        source: Some(TrajectorySource::Platform),
        spec_governed: None,
        agent_type: None,
        request_body: None,
        intent: None,
    };
    if let Err(e) = state.persist_trajectory_entry(&entry).await {
        tracing::error!(error = %e, "failed to persist entity-set-not-found trajectory");
    }
}

#[cfg(test)]
mod tests {
    use super::select_entity_ids_for_materialization;
    use temper_odata::query::types::{
        FilterExpr, ODataValue, OrderByClause, OrderDirection, QueryOptions,
    };

    #[test]
    fn default_pagination_applies_when_top_missing_and_no_filter_orderby() {
        let ids: Vec<String> = (0..150).map(|i| format!("id-{i}")).collect();
        let opts = QueryOptions {
            count: Some(true),
            ..QueryOptions::default()
        };

        let (selected, apply_opts, count) =
            select_entity_ids_for_materialization(ids, &opts, 100, 1000);

        assert_eq!(selected.len(), 100);
        assert_eq!(selected.first().unwrap(), "id-0");
        assert_eq!(selected.last().unwrap(), "id-99");
        assert_eq!(count, Some(150));
        assert_eq!(apply_opts.top, None);
        assert_eq!(apply_opts.skip, None);
        assert_eq!(apply_opts.count, None);
    }

    #[test]
    fn explicit_skip_top_are_applied_before_materialization() {
        let ids: Vec<String> = (0..50).map(|i| format!("id-{i}")).collect();
        let opts = QueryOptions {
            top: Some(10),
            skip: Some(5),
            count: Some(true),
            ..QueryOptions::default()
        };

        let (selected, _apply_opts, count) =
            select_entity_ids_for_materialization(ids, &opts, 100, 1000);

        assert_eq!(selected.len(), 10);
        assert_eq!(selected.first().unwrap(), "id-5");
        assert_eq!(selected.last().unwrap(), "id-14");
        assert_eq!(count, Some(50));
    }

    #[test]
    fn hard_cap_limits_materialization_and_filter_orderby_path_sets_default_top() {
        let ids: Vec<String> = (0..2500).map(|i| format!("id-{i}")).collect();
        let opts = QueryOptions {
            filter: Some(FilterExpr::Literal(ODataValue::Boolean(true))),
            orderby: Some(vec![OrderByClause {
                property: "Status".to_string(),
                direction: OrderDirection::Asc,
            }]),
            ..QueryOptions::default()
        };

        let (selected, apply_opts, count) =
            select_entity_ids_for_materialization(ids, &opts, 100, 1000);

        assert_eq!(selected.len(), 1000);
        assert_eq!(count, None);
        assert_eq!(apply_opts.top, Some(100));
    }
}
