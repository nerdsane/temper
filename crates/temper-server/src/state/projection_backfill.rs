use std::collections::BTreeMap;
use std::time::Instant;

use temper_runtime::tenant::TenantId;

use crate::entity_actor::{EntityRecoveryContext, recover_entity_state_from_stable_sources};

use super::ServerState;

mod entity_load;
mod key_index;
mod replay_parity;
mod vector_index;

use super::source_fenced_projection::repair_projection_from_stable_source;
use entity_load::{EntityLoadOutcome, load_entity_current_fields};
pub(super) use key_index::{
    PreparedKeyIndexCoverage, populate_key_index_from_snapshots,
    prepare_key_index_coverage_for_activation, publish_prepared_key_index_coverage,
};
pub(super) use replay_parity::verify_query_projection_replay_parity;
pub(super) use vector_index::populate_vector_index_from_snapshots;

const MAX_FIELD_BACKFILL_SOURCE_ATTEMPTS: usize = 3;

enum FieldBackfillOutcome {
    Upserted { event_count: u64, sequence_nr: u64 },
    Removed { event_count: u64, sequence_nr: u64 },
    Empty { event_count: u64 },
}

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

/// Backfill the broad `entity_field_index` (every field of every entity) so the native
/// AND-equality candidate pushdown can bound any non-keyed point lookup (e.g. `Path eq
/// '/souls' and WorkspaceId eq …`) instead of full-scanning and 413ing at tenant scale
/// (ARN-68). Enumerates authoritatively (registry types + `store.list_entity_ids_by_type`),
/// loads each entity's current state from its snapshot (or event replay), and upserts the
/// query projection. Idempotent (re-runs converge; it re-processes every entity — there
/// is no watermark/skip like the key-index backfill has); runs as a background task off
/// the boot path. This is the generic counterpart to the declared-key backfill — covers ALL
/// types, where the key backfill covers only declared-key shapes (incl. null components).
pub(super) async fn populate_field_index_from_snapshots(state: &ServerState, tenant: &TenantId) {
    let overall_started_at = Instant::now(); // determinism-ok: production-only backfill duration metric
    let Some((store, backend)) = state.event_journal() else {
        return;
    };
    if state.query_plane_store().is_none() {
        return;
    }

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
        for entity_type in &entity_types {
            match store
                .list_entity_ids_by_type(tenant.as_str(), entity_type)
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
                }
            }
        }
        result
    };

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
    let mut errors = 0usize;
    let needs_replay = entities;

    tracing::info!(
        tenant = %tenant,
        total,
        needs_replay = needs_replay.len(),
        "field index backfill starting stable snapshot/journal recovery"
    );

    for (entity_type, entity_id) in &needs_replay {
        let Some(table) = transition_table_for(state, tenant, entity_type) else {
            tracing::debug!(
                entity_type = %entity_type,
                entity_id = %entity_id,
                "field index backfill: no transition table available for replay"
            );
            errors += 1;
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
        let persistence_id = format!("{tenant}:{entity_type}:{entity_id}");
        let mut outcome = None;
        let mut last_event_count = 0;
        for attempt in 1..=MAX_FIELD_BACKFILL_SOURCE_ATTEMPTS {
            let source = match recover_entity_state_from_stable_sources(EntityRecoveryContext {
                tenant: tenant.as_str(),
                entity_type,
                entity_id,
                table: &table,
                store: &store,
                backend,
                initial_fields: &serde_json::json!({}),
                blob_store: tenant_blob_store.as_ref(),
            })
            .await
            {
                Ok(source) => source,
                Err(error) => {
                    tracing::debug!(
                        error = %error,
                        attempt,
                        entity_type = %entity_type,
                        entity_id = %entity_id,
                        "field index backfill: stable recovery failed"
                    );
                    continue;
                }
            };
            let authoritative_sequence = source.durable_sequence();
            let Some(replayed) = source.state.as_ref() else {
                outcome = Some(FieldBackfillOutcome::Empty { event_count: 0 });
                break;
            };
            last_event_count = replayed.total_event_count as u64;
            if replayed.total_event_count == 0
                && source.snapshot.is_none()
                && authoritative_sequence == 0
            {
                outcome = Some(FieldBackfillOutcome::Empty {
                    event_count: last_event_count,
                });
                break;
            }

            match repair_projection_from_stable_source(
                state,
                tenant,
                entity_type,
                entity_id,
                &store,
                &persistence_id,
                &source,
            )
            .await
            {
                Ok(true) if replayed.status == "Deleted" => {
                    outcome = Some(FieldBackfillOutcome::Removed {
                        event_count: last_event_count,
                        sequence_nr: authoritative_sequence,
                    });
                    break;
                }
                Ok(true) => {
                    outcome = Some(FieldBackfillOutcome::Upserted {
                        event_count: last_event_count,
                        sequence_nr: authoritative_sequence,
                    });
                    break;
                }
                Ok(false) => tracing::debug!(
                    attempt,
                    entity_type = %entity_type,
                    entity_id = %entity_id,
                    "field index backfill: durable source changed during projection repair; retrying"
                ),
                Err(error) => tracing::debug!(
                    error = %error,
                    attempt,
                    entity_type = %entity_type,
                    entity_id = %entity_id,
                    "field index backfill: source-fenced projection repair failed; retrying"
                ),
            }
        }

        match outcome {
            Some(FieldBackfillOutcome::Upserted {
                event_count,
                sequence_nr,
            }) => {
                indexed += 1;
                crate::query_projection_metrics::record_backfill_replay_events(
                    tenant.as_str(),
                    entity_type,
                    "ok",
                    event_count,
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
                    sequence_nr,
                );
            }
            Some(FieldBackfillOutcome::Removed {
                event_count,
                sequence_nr,
            }) => {
                crate::query_projection_metrics::record_backfill_replay_events(
                    tenant.as_str(),
                    entity_type,
                    "deleted",
                    event_count,
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
                    sequence_nr,
                );
            }
            Some(FieldBackfillOutcome::Empty { event_count }) => {
                errors += 1;
                crate::query_projection_metrics::record_backfill_replay_events(
                    tenant.as_str(),
                    entity_type,
                    "empty",
                    event_count,
                );
                crate::query_projection_metrics::record_backfill_entities(
                    tenant.as_str(),
                    entity_type,
                    "backfill_replay",
                    "empty",
                    1,
                );
            }
            None => {
                errors += 1;
                crate::query_projection_metrics::record_backfill_replay_events(
                    tenant.as_str(),
                    entity_type,
                    "error",
                    last_event_count,
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

        tokio::task::yield_now().await;
    }

    tracing::info!(
        tenant = %tenant,
        total,
        indexed,
        errors,
        "populated query projections from stable snapshot/journal recovery"
    );
    crate::query_projection_metrics::record_backfill_duration(
        tenant.as_str(),
        "overall",
        overall_started_at.elapsed(),
    );
}
