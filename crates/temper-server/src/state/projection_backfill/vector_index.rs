//! ADR-0155 declared-vector backfill: populate `entity_vector_index` for entities
//! that existed before the `[[vector]]` path was declared (or, on a write-behind
//! backend, that lag the index), and record the per-(tenant, entity_type) watermark.
//!
//! Mirrors the declared-key backfill (`key_index.rs`): authoritative enumeration
//! unions durable and projected IDs, strict state load, per-decl vector parse, exact
//! replacement, and a watermark only when every discovered entity was reconciled.

use std::collections::BTreeSet;

use temper_runtime::tenant::TenantId;

use crate::ServerState;

use super::{EntityLoadOutcome, load_entity_current_fields, transition_table_for};

/// Retire vector-index authority for types whose current spec has no vectors.
///
/// This is deliberately narrower than exact reconciliation. Generation changes use
/// it before registry mutation so a failed rebuild for a still-installed declaration
/// cannot prevent a corrective spec from removing that declaration.
pub(in crate::state) async fn retire_removed_vector_index_watermarks(
    state: &ServerState,
    tenant: &TenantId,
) -> bool {
    let Some((store, _)) = state.event_journal() else {
        return true;
    };
    if !store.has_durable_vector_backfill_watermark() {
        return true;
    }

    let current_vectored_types = state
        .governed_entity_types_for(tenant)
        .into_iter()
        .filter(|entity_type| !state.declared_vectors_for(tenant, entity_type).is_empty())
        .collect::<BTreeSet<_>>();

    let known_watermarks = match store.vector_index_backfilled_types(tenant.as_str()).await {
        Ok(watermarks) => watermarks,
        Err(e) => {
            tracing::error!(
                tenant = %tenant, error = %e,
                "vector index backfill: durable watermark unreadable; retirement not started"
            );
            return false;
        }
    };
    let empty_vector_set = crate::vector_index::declared_vector_set_signature(&[]);
    let mut succeeded = true;
    for (entity_type, _) in known_watermarks {
        if current_vectored_types.contains(&entity_type) {
            continue;
        }
        let _retirement_fence = match store
            .acquire_projection_reconciliation_fence(tenant.as_str(), &entity_type)
            .await
        {
            Ok(fence) => fence,
            Err(e) => {
                succeeded = false;
                tracing::error!(
                    tenant = %tenant, entity_type = %entity_type, error = %e,
                    "vector index backfill: failed to fence removed-declaration watermark retirement"
                );
                continue;
            }
        };
        let still_covered = match store.vector_index_backfilled_types(tenant.as_str()).await {
            Ok(watermarks) => watermarks
                .into_iter()
                .any(|(covered_type, _)| covered_type == entity_type),
            Err(e) => {
                succeeded = false;
                tracing::error!(
                    tenant = %tenant, entity_type = %entity_type, error = %e,
                    "vector index backfill: removed-declaration watermark re-read failed"
                );
                continue;
            }
        };
        if !still_covered {
            continue;
        }
        if let Err(e) = store
            .mark_vector_index_backfilled(tenant.as_str(), &entity_type, &empty_vector_set)
            .await
        {
            succeeded = false;
            tracing::error!(
                tenant = %tenant, entity_type = %entity_type, error = %e,
                "vector index backfill: failed to retire removed-declaration watermark"
            );
        }
    }

    succeeded
}

/// Backfill `entity_vector_index` for existing entities, then record the watermark.
///
/// Idempotent; entities written after the vector path was declared already maintain
/// their vectors at write time (co-commit on postgres/sim, write-behind on turso).
/// Runs as a cooperative background task off the boot path.
pub(in crate::state) async fn populate_vector_index_from_snapshots(
    state: &ServerState,
    tenant: &TenantId,
) -> bool {
    let mut succeeded = retire_removed_vector_index_watermarks(state, tenant).await;
    let Some((store, backend)) = state.event_journal() else {
        return succeeded;
    };

    // Registry-installed and compatibility-table types share declaration lookup,
    // so both must be eligible to earn the same durable vector authority.
    let vectored_types: Vec<(String, Vec<temper_jit::table::types::DeclaredVector>)> = state
        .governed_entity_types_for(tenant)
        .into_iter()
        .filter_map(|entity_type| {
            let vectors = state.declared_vectors_for(tenant, &entity_type);
            (!vectors.is_empty()).then_some((entity_type, vectors))
        })
        .collect();

    let durable_watermark = store.has_durable_vector_backfill_watermark();

    for (entity_type, vectors) in &vectored_types {
        let current_set = crate::vector_index::declared_vector_set_signature(vectors);
        let _reconciliation_fence = match store
            .acquire_projection_reconciliation_fence(tenant.as_str(), entity_type)
            .await
        {
            Ok(fence) => fence,
            Err(e) => {
                succeeded = false;
                tracing::error!(
                    tenant = %tenant, entity_type = %entity_type, error = %e,
                    "vector index backfill: failed to acquire reconciliation fence"
                );
                continue;
            }
        };
        // The fence makes this the definitive current watermark read. An error is
        // not equivalent to absence: abort before mutating projection rows.
        let covered = if durable_watermark {
            match store.vector_index_backfilled_types(tenant.as_str()).await {
                Ok(types) => types.into_iter().find_map(|(covered_type, vector_set)| {
                    (covered_type == *entity_type).then_some(vector_set)
                }),
                Err(e) => {
                    succeeded = false;
                    tracing::error!(
                        tenant = %tenant, entity_type = %entity_type, error = %e,
                        "vector index backfill: durable watermark unreadable; reconciliation not started"
                    );
                    continue;
                }
            }
        } else {
            // Write-behind backends cannot let an old completion marker suppress
            // repair after an exhausted live update. They reconcile every startup;
            // per-entity journal-sequence checks prevent stale replay from winning.
            None
        };
        // Already complete for the CURRENT declared vector-set: the write path keeps
        // the index whole (co-commit) or write-behind + this backfill did, so skip.
        if covered.as_deref() == Some(current_set.as_str()) {
            continue;
        }
        // Any missing/different watermark requires a full exact reconciliation. The
        // signature version makes this happen once after upgrading from binaries that
        // could leave tombstone rows behind.
        if covered.is_some() {
            tracing::info!(
                tenant = %tenant, entity_type = %entity_type,
                covered_set = covered.as_deref().unwrap_or(""),
                current_set = %current_set,
                "vector index backfill: signature changed — exactly reconciling every durable or projected entity (one-time)"
            );
        }

        let mut entity_ids: BTreeSet<String> = match store
            .list_entity_ids_by_type(tenant.as_str(), entity_type)
            .await
        {
            Ok(ids) => ids.into_iter().collect(),
            Err(e) => {
                succeeded = false;
                tracing::error!(
                    tenant = %tenant, entity_type = %entity_type, error = %e,
                    "vector index backfill: failed to enumerate entities; type not watermarked"
                );
                continue;
            }
        };
        let projected_ids = match store
            .vectored_entity_ids_for_type(tenant.as_str(), entity_type)
            .await
        {
            Ok(ids) => ids,
            Err(e) => {
                succeeded = false;
                tracing::error!(
                    tenant = %tenant, entity_type = %entity_type, error = %e,
                    "vector index backfill: failed to enumerate existing projection rows; type not watermarked"
                );
                continue;
            }
        };
        entity_ids.extend(projected_ids);

        let table = transition_table_for(state, tenant, entity_type);
        let blob_store = state.blob_store_for_tenant(tenant).ok();
        let total = entity_ids.len();
        let mut reconciled = 0usize;
        let mut skipped = 0usize;
        let mut failed = 0usize;

        for entity_id in &entity_ids {
            match load_entity_current_fields(
                tenant,
                entity_type,
                entity_id,
                table.as_ref(),
                &store,
                backend,
                blob_store.as_ref(),
            )
            .await
            {
                EntityLoadOutcome::Fields {
                    fields,
                    sequence_nr,
                } => {
                    let mut vector_rows = Vec::new();
                    if let Some(field_map) = fields.as_object() {
                        for decl in vectors {
                            let Some(vector) = field_map.get(&decl.property).and_then(|v| {
                                crate::vector_index::parse_vector_property(v, decl.dims)
                            }) else {
                                continue;
                            };
                            let Some(model_tag) = field_map
                                .get(&decl.model_property)
                                .and_then(|v| v.as_str())
                                .filter(|tag| !tag.is_empty())
                            else {
                                continue;
                            };
                            vector_rows.push(temper_runtime::persistence::EntityVectorRow {
                                decl_name: decl.name.clone(),
                                model_tag: model_tag.to_string(),
                                vector,
                            });
                        }
                    }
                    let has_rows = !vector_rows.is_empty();
                    match store
                        .backfill_entity_vectors(
                            tenant.as_str(),
                            entity_type,
                            entity_id,
                            sequence_nr,
                            &vector_rows,
                        )
                        .await
                    {
                        Ok(()) if has_rows => reconciled += 1,
                        Ok(()) => skipped += 1,
                        Err(e) => {
                            failed += 1;
                            tracing::warn!(
                                error = %e, entity_type = %entity_type, entity_id = %entity_id,
                                "vector index backfill: upsert failed"
                            );
                        }
                    }
                }
                EntityLoadOutcome::Skip { sequence_nr } => {
                    // A deleted (or phantom) entity must hold no vector rows — purge
                    // any it still has so a soft-deleted entity is never ranked
                    // (reconcile with an empty row set). Harmless when there is nothing
                    // to purge.
                    if let Err(e) = store
                        .backfill_entity_vectors(
                            tenant.as_str(),
                            entity_type,
                            entity_id,
                            sequence_nr,
                            &[],
                        )
                        .await
                    {
                        failed += 1;
                        tracing::warn!(
                            error = %e, entity_type = %entity_type, entity_id = %entity_id,
                            "vector index backfill: purge of deleted/phantom entity failed"
                        );
                    } else {
                        skipped += 1;
                    }
                }
                EntityLoadOutcome::LoadFailed => {
                    failed += 1;
                    tracing::warn!(
                        entity_type = %entity_type, entity_id = %entity_id,
                        "vector index backfill: existing entity could not be loaded; type will NOT be watermarked"
                    );
                }
            }
            tokio::task::yield_now().await;
        }

        // Watermark only if nothing failed — every durable or projected entity was
        // exactly reconciled. Otherwise a later run retries the full exact pass.
        if failed == 0
            && durable_watermark
            && let Err(e) = store
                .mark_vector_index_backfilled(tenant.as_str(), entity_type, &current_set)
                .await
        {
            failed += 1;
            tracing::error!(
                tenant = %tenant, entity_type = %entity_type, error = %e,
                "vector index backfill: failed to persist watermark"
            );
        }
        if failed == 0 {
            tracing::info!(
                tenant = %tenant, entity_type = %entity_type, vector_set = %current_set,
                total, reconciled, skipped, durable_watermark,
                "entity_vector_index backfill complete"
            );
        } else {
            succeeded = false;
            tracing::warn!(
                tenant = %tenant, entity_type = %entity_type,
                total, reconciled, skipped, failed,
                "vector index backfill: {failed} entities unresolved; type NOT watermarked (will retry exact reconciliation)"
            );
        }
    }

    succeeded
}
