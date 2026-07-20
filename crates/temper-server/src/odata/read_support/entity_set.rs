use temper_runtime::tenant::TenantId;

use crate::state::ServerState;

#[cfg(test)]
#[derive(Debug)]
pub(super) struct SelectedEntityIdsForMaterialization {
    pub(super) entity_ids: Vec<String>,
    pub(super) apply_options: temper_odata::query::types::QueryOptions,
    pub(super) precomputed_count: Option<usize>,
}

#[cfg(test)]
#[derive(Debug, Eq, PartialEq)]
pub(super) struct EntitySelectionTooLarge {
    pub(super) candidate_count: usize,
    pub(super) candidate_budget: usize,
}

#[cfg(test)]
pub(super) fn select_entity_ids_for_materialization(
    mut entity_ids: Vec<String>,
    query_options: &temper_odata::query::types::QueryOptions,
    default_page_size: usize,
    max_entities: usize,
    has_row_authorization: bool,
) -> Result<SelectedEntityIdsForMaterialization, EntitySelectionTooLarge> {
    let has_filter_or_order =
        query_options.filter.is_some() || query_options.orderby.is_some() || has_row_authorization;
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
        // When a $filter or $orderby is present, we must materialise ALL
        // candidate entities before the filter/sort can be applied.
        // Truncating the candidate set before filtering would silently hide
        // entities that match the filter but sort past the cutoff — a
        // correctness bug that caused system skills to vanish from large File
        // collections (see ADR: skill-bootstrap-invisible-in-odata).
        //
        // Safety cap: impose a hard ceiling (10× max_entities) and reject
        // reads that cannot be proven complete inside the candidate budget.
        let safety_cap = max_entities.saturating_mul(10);
        if entity_ids.len() > safety_cap {
            return Err(EntitySelectionTooLarge {
                candidate_count: entity_ids.len(),
                candidate_budget: safety_cap,
            });
        }

        let mut adjusted = query_options.clone();
        if adjusted.top.is_none() {
            adjusted.top = Some(default_page_size);
        } else if let Some(top) = adjusted.top {
            adjusted.top = Some(top.min(max_entities));
        }
        adjusted
    };

    Ok(SelectedEntityIdsForMaterialization {
        entity_ids,
        apply_options,
        precomputed_count,
    })
}

/// Resolve an entity set name from an entity type name.
///
/// Reverse-lookups the entity_set_map to find the set name for a given type.
pub(in crate::odata) fn resolve_entity_set_name(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
) -> String {
    let registry = state
        .registry
        .read()
        .expect("registry lock should not be poisoned"); // ci-ok: infallible lock
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
pub(in crate::odata) async fn record_entity_set_not_found(
    state: &ServerState,
    tenant: &str,
    set_name: &str,
) {
    tracing::warn!(tenant = %tenant, entity_set = %set_name, "entity set not found");
    // Intentionally no trajectory write: read-only operations must not write to the database.
    // Previously this wrote a TrajectoryEntry on every failed EntitySetLookup, creating
    // unbounded junk rows (4,269 rows, 83% of all trajectories) from phantom entity polling.
    let _ = state; // suppress unused warning
}
