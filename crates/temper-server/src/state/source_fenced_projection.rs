//! Exact-source reconciliation for derived query projections.

use temper_runtime::actor::ActorError;
use temper_runtime::persistence::{ProjectionSourceFence, SnapshotBackfillFence};
use temper_runtime::tenant::TenantId;

use crate::entity_actor::{
    EntityRecoveryContext, StableEntitySource, recover_entity_state_from_stable_sources,
    stable_entity_source_is_current,
};
use crate::storage::BoxedEventStore;

use super::ServerState;

const MAX_DIRTY_PROJECTION_REPAIR_ATTEMPTS: usize = 3;

/// Reconcile one recovered entity projection while its exact durable source is
/// current, then close the source fence. `false` means the caller must recover
/// and retry from a new generation.
pub(crate) async fn repair_projection_from_stable_source(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
    store: &BoxedEventStore,
    persistence_id: &str,
    source: &StableEntitySource,
) -> Result<bool, ActorError> {
    let Some(query_plane) = state.query_plane_store() else {
        return stable_entity_source_is_current(store, persistence_id, source).await;
    };
    let source_fence = ProjectionSourceFence {
        expected_journal_sequence: source.journal_sequence,
        expected_snapshot: source
            .snapshot
            .as_ref()
            .map(|snapshot| SnapshotBackfillFence {
                sequence_nr: snapshot.sequence_nr,
                state: snapshot.state.as_slice(),
            }),
    };

    let Some(entity_state) = source.state.as_ref() else {
        let applied = query_plane
            .clear_projection_dirty_if_source(
                tenant.as_str(),
                entity_type,
                entity_id,
                source_fence,
            )
            .await
            .map_err(|error| {
                ActorError::custom(format!(
                    "source-fenced empty projection acknowledgement failed for {entity_type}:{entity_id}: {error}"
                ))
            })?;
        if !applied {
            return Ok(false);
        }
        return stable_entity_source_is_current(store, persistence_id, source).await;
    };

    if entity_state.status == "Deleted" {
        let applied = query_plane
            .remove_projection_if_source(tenant.as_str(), entity_type, entity_id, source_fence)
            .await
            .map_err(|error| {
                ActorError::custom(format!(
                    "source-fenced projection removal failed for {entity_type}:{entity_id}: {error}"
                ))
            })?;
        if !applied {
            return Ok(false);
        }
        return stable_entity_source_is_current(store, persistence_id, source).await;
    }

    let fields = state.query_projection_fields(tenant, entity_type, &entity_state.fields);
    let projected_state = state.query_projection_state(entity_state);
    let sequence_nr = source.durable_sequence();
    let applied = query_plane
        .upsert_projection_if_source(
            tenant.as_str(),
            entity_type,
            entity_id,
            &entity_state.status,
            &fields,
            &projected_state,
            sequence_nr,
            source_fence,
        )
        .await
        .map_err(|error| {
            ActorError::custom(format!(
                "source-fenced projection upsert failed for {entity_type}:{entity_id}: {error}"
            ))
        })?;
    if !applied {
        return Ok(false);
    }

    match stable_entity_source_is_current(store, persistence_id, source).await {
        Ok(true) => Ok(true),
        closing_result => {
            // The source advanced after the conditional write committed. Remove
            // only that exact attempted catalog row; a concurrent newer row with
            // the same sequence but different full state is preserved. Projection
            // absence is fail-closed because query reads detect the field-index
            // coverage gap and reconcile from authoritative state.
            query_plane
                .remove_projection_if_exact(
                    tenant.as_str(),
                    entity_type,
                    entity_id,
                    &entity_state.status,
                    &fields,
                    &projected_state,
                    sequence_nr,
                )
                .await
                .map_err(|cleanup_error| {
                    ActorError::custom(format!(
                        "failed to clean unstable projection for {entity_type}:{entity_id}: {cleanup_error}"
                    ))
                })?;
            match closing_result {
                Ok(false) => Ok(false),
                Err(error) => Err(error),
                Ok(true) => unreachable!("handled above"),
            }
        }
    }
}

/// Repair every durably dirty projection for one entity type before a native
/// query-plane read can trust the catalog or field index. Source writers mark
/// dirty in the same transaction as their journal/snapshot mutation; a
/// source-fenced projection mutation clears it in the same backend transaction.
/// Exhaustion leaves the marker durable so the read fails closed and retries.
pub(crate) async fn repair_dirty_projections_before_read(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    repair_budget: usize,
) -> Result<(), ActorError> {
    let repair_budget = repair_budget.max(1);
    let Some(query_plane) = state.query_plane_store() else {
        return Ok(());
    };
    let dirty_ids = query_plane
        .dirty_projection_entity_ids(
            tenant.as_str(),
            entity_type,
            repair_budget.saturating_add(1),
        )
        .await
        .map_err(|error| {
            ActorError::custom(format!(
                "failed to enumerate dirty query projections for {entity_type}: {error}"
            ))
        })?;
    let Some(mut dirty_ids) = dirty_ids else {
        return Ok(());
    };
    if dirty_ids.is_empty() {
        return Ok(());
    }
    let exceeded_budget = dirty_ids.len() > repair_budget;
    dirty_ids.truncate(repair_budget);

    let Some((store, backend)) = state.event_journal() else {
        return Err(ActorError::custom(format!(
            "dirty query projections for {entity_type} cannot be repaired without an event journal"
        )));
    };
    let Some(table) = super::projection_backfill::transition_table_for(state, tenant, entity_type)
    else {
        return Err(ActorError::custom(format!(
            "dirty query projections for {entity_type} cannot be repaired without a transition table"
        )));
    };
    let tenant_blob_store = state.blob_store_for_tenant(tenant).ok();
    let initial_fields = serde_json::json!({});

    for entity_id in dirty_ids {
        let persistence_id = format!("{tenant}:{entity_type}:{entity_id}");
        let mut repaired = false;
        for _attempt in 1..=MAX_DIRTY_PROJECTION_REPAIR_ATTEMPTS {
            let source = recover_entity_state_from_stable_sources(EntityRecoveryContext {
                tenant: tenant.as_str(),
                entity_type,
                entity_id: &entity_id,
                table: &table,
                store: &store,
                backend,
                initial_fields: &initial_fields,
                blob_store: tenant_blob_store.as_ref(),
            })
            .await?;
            if repair_projection_from_stable_source(
                state,
                tenant,
                entity_type,
                &entity_id,
                &store,
                &persistence_id,
                &source,
            )
            .await?
            {
                repaired = true;
                break;
            }
        }
        if !repaired {
            return Err(ActorError::custom(format!(
                "dirty query projection for {entity_type}:{entity_id} did not stabilize after {MAX_DIRTY_PROJECTION_REPAIR_ATTEMPTS} attempts"
            )));
        }
    }

    let remaining = query_plane
        .dirty_projection_entity_ids(tenant.as_str(), entity_type, 1)
        .await
        .map_err(|error| {
            ActorError::custom(format!(
                "failed to close dirty query projection repair for {entity_type}: {error}"
            ))
        })?;
    if remaining.as_ref().is_some_and(|ids| !ids.is_empty()) {
        return Err(ActorError::custom(format!(
            "dirty query projections remain for {entity_type} after bounded repair of {repair_budget} entities{}",
            if exceeded_budget {
                " (initial batch exceeded the repair budget)"
            } else {
                ""
            }
        )));
    }
    Ok(())
}
