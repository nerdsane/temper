//! ADR-0155 declared-vector backfill: populate `entity_vector_index` for entities
//! that existed before the `[[vector]]` path was declared (or, on a write-behind
//! backend, that lag the index), and record the per-(tenant, entity_type) watermark.
//!
//! Mirrors the declared-key backfill (`key_index.rs`): authoritative enumeration
//! (registry types + `store.list_entity_ids_by_type`), strict state load, per-decl
//! vector parse, idempotent upsert, and a watermark only when every existing entity
//! was indexed or is definitively skippable.

use std::collections::BTreeSet;

use temper_runtime::tenant::TenantId;

use crate::ServerState;

use super::{EntityLoadOutcome, load_entity_current_fields, transition_table_for};

/// Backfill `entity_vector_index` for existing entities, then record the watermark.
///
/// Idempotent; entities written after the vector path was declared already maintain
/// their vectors at write time (co-commit on postgres/sim, write-behind on turso).
/// Runs as a cooperative background task off the boot path.
pub(in crate::state) async fn populate_vector_index_from_snapshots(
    state: &ServerState,
    tenant: &TenantId,
) {
    let Some((store, backend)) = state.event_journal() else {
        return;
    };

    // Types with a declared vector path, from the registry (os-app entities live here).
    let vectored_types: Vec<(String, Vec<temper_jit::table::types::DeclaredVector>)> = {
        let registry = state.registry.read().unwrap();
        registry
            .entity_types(tenant)
            .into_iter()
            .filter_map(|entity_type| {
                let table = registry.get_table(tenant, entity_type)?;
                if table.vectors.is_empty() {
                    None
                } else {
                    Some((entity_type.to_string(), table.vectors.clone()))
                }
            })
            .collect()
    };
    if vectored_types.is_empty() {
        return;
    }

    // The covered vector-path set per type (empty map on any failure — treat as
    // never-backfilled, which is safe: it re-indexes, never skips wrongly).
    let covered: std::collections::BTreeMap<String, String> = store
        .vector_index_backfilled_types(tenant.as_str())
        .await
        .unwrap_or_default()
        .into_iter()
        .collect();

    for (entity_type, vectors) in &vectored_types {
        let current_set = crate::vector_index::declared_vector_set_signature(vectors);
        // Already complete for the CURRENT declared vector-set: the write path keeps
        // the index whole (co-commit) or write-behind + this backfill did, so skip.
        if covered.get(entity_type).map(String::as_str) == Some(current_set.as_str()) {
            continue;
        }
        // A watermark covering a DIFFERENT set means a vector path was declared after
        // the first backfill; re-index every existing entity under all current paths.
        let force_full_reindex = covered.contains_key(entity_type);
        if force_full_reindex {
            tracing::info!(
                tenant = %tenant, entity_type = %entity_type,
                covered_set = covered.get(entity_type).map(String::as_str).unwrap_or(""),
                current_set = %current_set,
                "vector index backfill: declared vector-set changed — re-indexing every existing entity of this type (one-time)"
            );
        }

        let entity_ids = match store
            .list_entity_ids_by_type(tenant.as_str(), entity_type)
            .await
        {
            Ok(ids) => ids,
            Err(e) => {
                tracing::error!(
                    tenant = %tenant, entity_type = %entity_type, error = %e,
                    "vector index backfill: failed to enumerate entities; type not watermarked"
                );
                continue;
            }
        };

        // Resumability: on a first-time backfill, skip entities already indexed. On a
        // set change, re-index all (a newly declared path is not yet on them).
        let already_indexed: BTreeSet<String> = if force_full_reindex {
            BTreeSet::new()
        } else {
            match store
                .vectored_entity_ids_for_type(tenant.as_str(), entity_type)
                .await
            {
                Ok(ids) => ids.into_iter().collect(),
                Err(_) => BTreeSet::new(),
            }
        };

        let table = transition_table_for(state, tenant, entity_type);
        let blob_store = state.blob_store_for_tenant(tenant).ok();
        let total = entity_ids.len();
        let mut newly_indexed = 0usize;
        let mut already = 0usize;
        let mut skipped = 0usize;
        let mut failed = 0usize;

        for entity_id in &entity_ids {
            if already_indexed.contains(entity_id) {
                already += 1;
                continue;
            }
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
                EntityLoadOutcome::Fields(fields) => {
                    let Some(field_map) = fields.as_object() else {
                        skipped += 1;
                        continue;
                    };
                    let mut vector_rows = Vec::new();
                    for decl in vectors {
                        let Some(vector) = field_map
                            .get(&decl.property)
                            .and_then(|v| crate::vector_index::parse_vector_property(v, decl.dims))
                        else {
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
                    if vector_rows.is_empty() {
                        // No usable vector on this entity yet (unembedded) — not a
                        // failure; it is simply absent from the ranking until embedded.
                        skipped += 1;
                        continue;
                    }
                    match store
                        .backfill_entity_vectors(
                            tenant.as_str(),
                            entity_type,
                            entity_id,
                            &vector_rows,
                        )
                        .await
                    {
                        Ok(()) => newly_indexed += 1,
                        Err(e) => {
                            failed += 1;
                            tracing::warn!(
                                error = %e, entity_type = %entity_type, entity_id = %entity_id,
                                "vector index backfill: upsert failed"
                            );
                        }
                    }
                }
                EntityLoadOutcome::Skip => skipped += 1,
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

        // Watermark only if nothing failed — every existing entity was indexed or is
        // definitively skippable. Otherwise a later run resumes from the remainder.
        if failed == 0 {
            if let Some((store, _)) = state.event_journal()
                && let Err(e) = store
                    .mark_vector_index_backfilled(tenant.as_str(), entity_type, &current_set)
                    .await
            {
                tracing::error!(
                    tenant = %tenant, entity_type = %entity_type, error = %e,
                    "vector index backfill: failed to persist watermark"
                );
            }
            tracing::info!(
                tenant = %tenant, entity_type = %entity_type, vector_set = %current_set,
                total, newly_indexed, already, skipped,
                "entity_vector_index backfill complete; type watermarked"
            );
        } else {
            tracing::warn!(
                tenant = %tenant, entity_type = %entity_type,
                total, newly_indexed, already, skipped, failed,
                "vector index backfill: {failed} entities unresolved; type NOT watermarked (will resume next run)"
            );
        }
    }
}
