use std::collections::BTreeMap;
use std::time::Instant;

use temper_runtime::persistence::{PersistenceError, validate_latest_event_batch};
use temper_runtime::tenant::TenantId;

use super::ServerState;
use crate::entity_actor::recover_entity_state_from_store;

mod key_index;
mod replay_parity;
mod vector_index;

pub(super) use key_index::populate_key_index_from_snapshots;
pub(super) use replay_parity::verify_query_projection_replay_parity;
pub(super) use vector_index::populate_vector_index_from_snapshots;

pub(super) fn transition_table_for(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
) -> Option<temper_jit::TransitionTable> {
    {
        let registry = state.registry.read().unwrap();
        registry
            .get_table_live(tenant, entity_type)
            .map(|table| table.read().expect("table lock poisoned").clone())
    }
    .or_else(|| {
        state
            .transition_tables
            .get(entity_type)
            .map(|table| (**table).clone())
    })
}

/// Outcome of loading one entity's current state for an index backfill (ADR-0153,
/// ADR-0155). Shared by the key and vector backfills so they classify entities the
/// same way — the distinction is the watermark soundness gate.
pub(super) enum EntityLoadOutcome {
    /// Loaded — index it from these fields.
    Fields(serde_json::Value),
    /// Definitively skippable: deleted, or a phantom with no events. Correctly NOT
    /// indexed, and NOT a failure (it must not block the watermark).
    Skip,
    /// The entity exists (it was enumerated from the durable store) but its current
    /// state could not be loaded — no transition table to replay with, an unreadable
    /// snapshot, or a replay error. Indexing it is impossible, so the type must NOT be
    /// watermarked; otherwise a read would treat a present-but-unindexed entity as
    /// authoritatively covered. This is the soundness gate.
    LoadFailed,
}

/// Load one entity's CURRENT state for an index backfill: snapshot if present and
/// readable, else strict event replay (so a field mutated after the last snapshot is
/// indexed at its current value, and a journal read failure fails the watermark
/// rather than silently "starting fresh").
pub(super) async fn load_entity_current_fields(
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
    table: Option<&temper_jit::TransitionTable>,
    store: &crate::storage::BoxedEventStore,
    backend: crate::storage::BackendLabel,
    blob_store: Option<&crate::blob_store::BlobStore>,
) -> EntityLoadOutcome {
    let Some(table) = table else {
        return EntityLoadOutcome::LoadFailed;
    };
    match recover_entity_state_from_store(
        tenant.as_str(),
        entity_type,
        entity_id,
        table,
        store,
        backend,
        &serde_json::json!({}),
        blob_store,
        true, // strict: a journal read failure → Err → LoadFailed (don't watermark)
    )
    .await
    {
        Err(_) => EntityLoadOutcome::LoadFailed,
        Ok(state) if state.status == "Deleted" => EntityLoadOutcome::Skip,
        Ok(state) if state.total_event_count == 0 => EntityLoadOutcome::Skip,
        Ok(state) => EntityLoadOutcome::Fields(state.fields),
    }
}

/// Backfill the broad `entity_field_index` (every field of every entity) so the native
/// AND-equality candidate pushdown can bound any non-keyed point lookup (e.g. `Path eq
/// '/souls' and WorkspaceId eq …`) instead of full-scanning and 413ing at tenant scale
/// (ARN-68). Enumerates authoritatively (registry types + `store.list_entity_ids_by_type`),
/// rebuilds each entity's current state from its snapshot plus journal tail, and upserts the
/// query projection. Idempotent (re-runs converge; it re-processes every entity — there
/// is no watermark/skip like the key-index backfill has); runs as a background task off
/// the boot path. This is the generic counterpart to the declared-key backfill — covers ALL
/// types, where the key backfill covers only declared-key shapes (incl. null components).
pub(super) async fn populate_field_index_from_snapshots(
    state: &ServerState,
    tenant: &TenantId,
) -> Result<(), PersistenceError> {
    let overall_started_at = Instant::now(); // determinism-ok: production-only backfill duration metric
    let Some((store, backend)) = state.event_journal() else {
        return Ok(());
    };
    let Some(query_plane) = state.query_plane_store() else {
        return Ok(());
    };

    // Enumerate authoritatively: every entity type from the registry, and its entity
    // ids from `store.list_entity_ids_by_type`. It must NOT read `state.entity_index`,
    // which is populated only when an actor spawns (lazy) and is therefore near-empty at
    // boot — the original bug that left pre-existing entities out of the field index, so
    // their non-keyed equality lookups (e.g. `Path eq '/souls' and WorkspaceId eq …`)
    // fell back to the full-type scan and 413'd at tenant scale (ARN-68). This mirrors
    // the authoritative enumeration the declared-key backfill already uses; the field
    // index covers ALL types (not just keyed ones), so no key filter is applied.
    let entities = {
        let entity_types: Vec<String> = {
            let registry = state.registry.read().unwrap();
            registry
                .entity_types(tenant)
                .into_iter()
                .map(ToString::to_string)
                .collect()
        };
        let mut result = Vec::new();
        let mut failed_types = Vec::new();
        for entity_type in &entity_types {
            match super::entity_enumeration::bounded_entity_ids_by_type(&store, tenant, entity_type)
                .await
            {
                Ok(ids) => {
                    for id in ids {
                        result.push((entity_type.clone(), id));
                    }
                }
                Err(e) => {
                    tracing::error!(
                        tenant = %tenant, entity_type = %entity_type, error = %e,
                        "field index backfill: failed to enumerate entities; type skipped"
                    );
                    failed_types.push(entity_type.clone());
                }
            }
        }
        (result, failed_types)
    };

    let (entities, enumeration_failures) = entities;

    let total = entities.len();
    let mut considered_by_type = BTreeMap::<String, u64>::new();
    for (entity_type, _) in &entities {
        *considered_by_type.entry(entity_type.clone()).or_default() += 1;
    }
    for (entity_type, count) in considered_by_type {
        crate::query_projection_metrics::record_backfill_entities(
            tenant.as_str(),
            &entity_type,
            "backfill",
            "considered",
            count,
        );
    }

    let mut indexed = 0usize;
    let mut errors = enumeration_failures.len();
    let mut failed_types = enumeration_failures
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();

    for (entity_type, entity_id) in &entities {
        let Some(table) = transition_table_for(state, tenant, entity_type) else {
            tracing::error!(
                entity_type = %entity_type,
                entity_id = %entity_id,
                "field index backfill: no transition table available for replay"
            );
            errors += 1;
            failed_types.insert(entity_type.clone());
            if let Err(error) = remove_projection_at_current_tail(
                &query_plane,
                &store,
                tenant,
                entity_type,
                entity_id,
            )
            .await
            {
                tracing::error!(
                    %error,
                    entity_type = %entity_type,
                    entity_id = %entity_id,
                    "field index backfill: failed to quarantine projection without a table"
                );
                errors += 1;
                failed_types.insert(entity_type.clone());
            }
            crate::query_projection_metrics::record_backfill_entities(
                tenant.as_str(),
                entity_type,
                "backfill_replay",
                "missing_table",
                1,
            );
            continue;
        };

        let tenant_blob_store = state.blob_store_for_tenant(tenant).ok();
        let replayed = recover_entity_state_from_store(
            tenant.as_str(),
            entity_type,
            entity_id,
            &table,
            &store,
            backend,
            &serde_json::json!({}),
            tenant_blob_store.as_ref(),
            true, // publication must fail closed when the journal tail is unreadable
        )
        .await;
        let replayed = match replayed {
            Ok(state) => state,
            Err(e) => {
                tracing::error!(
                    error = %e,
                    entity_type = %entity_type,
                    entity_id = %entity_id,
                    "field index backfill: snapshot-plus-tail recovery failed"
                );
                errors += 1;
                failed_types.insert(entity_type.clone());
                if let Err(error) = remove_projection_at_current_tail(
                    &query_plane,
                    &store,
                    tenant,
                    entity_type,
                    entity_id,
                )
                .await
                {
                    tracing::error!(
                        %error,
                        entity_type = %entity_type,
                        entity_id = %entity_id,
                        "field index backfill: failed to quarantine projection after recovery error"
                    );
                    errors += 1;
                    failed_types.insert(entity_type.clone());
                }
                crate::query_projection_metrics::record_backfill_entities(
                    tenant.as_str(),
                    entity_type,
                    "backfill_replay",
                    "error",
                    1,
                );
                tokio::task::yield_now().await;
                continue;
            }
        };

        if replayed.total_event_count == 0 {
            crate::query_projection_metrics::record_backfill_replay_events(
                tenant.as_str(),
                entity_type,
                "empty",
                replayed.total_event_count as u64,
            );
            crate::query_projection_metrics::record_backfill_entities(
                tenant.as_str(),
                entity_type,
                "backfill_replay",
                "empty",
                1,
            );
            tracing::debug!(
                entity_type = %entity_type,
                entity_id = %entity_id,
                "field index backfill: discovered stream recovered no events"
            );
            errors += 1;
            failed_types.insert(entity_type.clone());
        } else if replayed.status == "Deleted" {
            if let Err(e) = query_plane
                .remove_projection_through_sequence(
                    tenant.as_str(),
                    entity_type,
                    entity_id,
                    replayed.sequence_nr,
                )
                .await
            {
                tracing::debug!(
                    error = %e,
                    entity_type = %entity_type,
                    entity_id = %entity_id,
                    "field index backfill: failed to clear deleted projection"
                );
                errors += 1;
                failed_types.insert(entity_type.clone());
                crate::query_projection_metrics::record_backfill_replay_events(
                    tenant.as_str(),
                    entity_type,
                    "error",
                    replayed.total_event_count as u64,
                );
                crate::query_projection_metrics::record_backfill_entities(
                    tenant.as_str(),
                    entity_type,
                    "backfill_replay",
                    "error",
                    1,
                );
            } else {
                crate::query_projection_metrics::record_backfill_replay_events(
                    tenant.as_str(),
                    entity_type,
                    "deleted",
                    replayed.total_event_count as u64,
                );
                crate::query_projection_metrics::record_backfill_entities(
                    tenant.as_str(),
                    entity_type,
                    "backfill_replay",
                    "deleted_removed",
                    1,
                );
                crate::query_projection_metrics::record_update_applied_sequence(
                    tenant.as_str(),
                    entity_type,
                    "remove",
                    "backfill_replay",
                    replayed.sequence_nr,
                );
            }
        } else {
            match query_plane
                .upsert_projection(
                    tenant.as_str(),
                    entity_type,
                    entity_id,
                    &replayed.status,
                    &state.query_projection_fields(tenant, entity_type, &replayed.fields),
                    &state.query_projection_state(&replayed),
                    replayed.sequence_nr,
                )
                .await
            {
                Ok(()) => {
                    indexed += 1;
                    crate::query_projection_metrics::record_backfill_replay_events(
                        tenant.as_str(),
                        entity_type,
                        "ok",
                        replayed.total_event_count as u64,
                    );
                    crate::query_projection_metrics::record_backfill_entities(
                        tenant.as_str(),
                        entity_type,
                        "backfill_replay",
                        "ok",
                        1,
                    );
                    crate::query_projection_metrics::record_update_applied_sequence(
                        tenant.as_str(),
                        entity_type,
                        "upsert",
                        "backfill_replay",
                        replayed.sequence_nr,
                    );
                }
                Err(e) => {
                    tracing::debug!(
                        error = %e,
                        entity_type = %entity_type,
                        entity_id = %entity_id,
                        "field index backfill: replay upsert failed"
                    );
                    errors += 1;
                    failed_types.insert(entity_type.clone());
                    crate::query_projection_metrics::record_backfill_replay_events(
                        tenant.as_str(),
                        entity_type,
                        "error",
                        replayed.total_event_count as u64,
                    );
                    crate::query_projection_metrics::record_backfill_entities(
                        tenant.as_str(),
                        entity_type,
                        "backfill_replay",
                        "error",
                        1,
                    );
                }
            }
        }

        tokio::task::yield_now().await;
    }

    tracing::info!(
        tenant = %tenant,
        total,
        indexed,
        errors,
        "populated query projections from snapshot-plus-tail recovery"
    );
    crate::query_projection_metrics::record_backfill_duration(
        tenant.as_str(),
        "overall",
        overall_started_at.elapsed(),
    );
    if errors > 0 {
        return Err(PersistenceError::Storage(format!(
            "query projection backfill incomplete: {errors} errors across entity types [{}]",
            failed_types.into_iter().collect::<Vec<_>>().join(",")
        )));
    }
    Ok(())
}

async fn remove_projection_at_current_tail(
    query_plane: &std::sync::Arc<dyn crate::storage::QueryPlaneStore>,
    store: &crate::storage::BoxedEventStore,
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
) -> Result<(), PersistenceError> {
    let persistence_ids = [format!("{tenant}:{entity_type}:{entity_id}")];
    validate_latest_event_batch(&persistence_ids)?;
    let mut latest = store.read_latest_events(&persistence_ids).await?;
    let sequence_nr = latest
        .pop()
        .flatten()
        .ok_or_else(|| {
            PersistenceError::Storage(format!(
                "cannot quarantine projection without journal tail for {}",
                persistence_ids[0]
            ))
        })?
        .sequence_nr;
    query_plane
        .remove_projection_through_sequence(tenant.as_str(), entity_type, entity_id, sequence_nr)
        .await
}
