//! ADR-0171 sequence-monotonic vector-index reconciliation.
//!
//! Every repair enumerates durable journal streams (including deleted entities),
//! rebuilds current rows from a strict replay, and carries that replay's journal
//! sequence into a store-level compare-and-reconcile transaction. Completion is
//! watermarked only after every stream converges durably.

use std::collections::{BTreeMap, BTreeSet};

use temper_runtime::tenant::TenantId;

use crate::ServerState;

use super::{EntityLoadOutcome, load_entity_current_fields, transition_table_for};

fn vector_backfill_work_types(
    current_vectors: &BTreeMap<String, Vec<temper_jit::table::types::DeclaredVector>>,
    covered: &BTreeMap<String, String>,
    reconciliation_types: &BTreeSet<String>,
) -> BTreeSet<String> {
    let mut work_types: BTreeSet<String> = current_vectors
        .iter()
        .filter(|(_, vectors)| !vectors.is_empty())
        .map(|(entity_type, _)| entity_type.clone())
        .collect();
    work_types.extend(covered.keys().cloned());
    work_types.extend(reconciliation_types.iter().cloned());
    work_types
}

/// Backfill `entity_vector_index` for existing entities, then record the watermark.
///
/// Idempotent and safe alongside live writes: a rebuild observed at sequence N is
/// ignored by the store after a live append advances that entity's fence to N+1.
pub(in crate::state) async fn populate_vector_index_from_snapshots(
    state: &ServerState,
    tenant: &TenantId,
) {
    // Acquire before reading declarations. A second local invocation therefore
    // cannot snapshot an older table and later allocate a newer durable generation
    // after a hot swap. The store token remains the authoritative crash/process
    // boundary (ADR-0171).
    let _reconciliation_guard = state.vector_reconciliation_lock.lock().await;
    let Some((store, backend)) = state.event_journal() else {
        return;
    };

    let covered: BTreeMap<String, String> = match store
        .vector_index_backfilled_types(tenant.as_str())
        .await
    {
        Ok(types) => types.into_iter().collect(),
        Err(error) => {
            tracing::error!(
                tenant = %tenant,
                error = %error,
                "vector index backfill: failed to load durable watermarks; reconciliation aborted"
            );
            return;
        }
    };
    let reconciliation_types: BTreeSet<String> = match store
        .vector_reconciliation_entity_types(tenant.as_str())
        .await
    {
        Ok(types) => types.into_iter().collect(),
        Err(error) => {
            tracing::error!(
                tenant = %tenant,
                error = %error,
                "vector index backfill: failed to load durable reconciliation types; reconciliation aborted"
            );
            return;
        }
    };

    // Keep empty vector declarations in this map. A type that was previously
    // watermarked but now declares none must still run once to purge retained rows.
    let current_vectors: BTreeMap<String, Vec<temper_jit::table::types::DeclaredVector>> = {
        let registry = state.registry.read().unwrap();
        registry
            .entity_types(tenant)
            .into_iter()
            .filter_map(|entity_type| {
                registry
                    .get_table(tenant, entity_type)
                    .map(|table| (entity_type.to_string(), table.vectors.clone()))
            })
            .collect()
    };

    let work_types = vector_backfill_work_types(&current_vectors, &covered, &reconciliation_types);

    for entity_type in work_types {
        let vectors = current_vectors
            .get(&entity_type)
            .cloned()
            .unwrap_or_default();
        let current_set = crate::vector_index::declared_vector_set_signature(&vectors);
        if covered.get(&entity_type).map(String::as_str) == Some(current_set.as_str()) {
            continue;
        }
        if let Some(previous_set) = covered.get(&entity_type) {
            tracing::info!(
                tenant = %tenant,
                entity_type = %entity_type,
                covered_set = %previous_set,
                current_set = %current_set,
                "vector index backfill: reconciliation signature changed; rebuilding every durable stream"
            );
        }

        let reconciliation_generation = match store
            .begin_vector_index_reconciliation(tenant.as_str(), &entity_type, &current_set)
            .await
        {
            Ok(generation) => generation,
            Err(error) => {
                tracing::error!(
                    tenant = %tenant,
                    entity_type = %entity_type,
                    vector_set = %current_set,
                    error = %error,
                    "vector index backfill: failed to begin durable reconciliation generation"
                );
                continue;
            }
        };

        let entity_ids = match store
            .list_vector_repair_entity_ids(tenant.as_str(), &entity_type)
            .await
        {
            Ok(ids) => ids,
            Err(error) => {
                tracing::error!(
                    tenant = %tenant,
                    entity_type = %entity_type,
                    error = %error,
                    "vector index backfill: failed to enumerate durable streams; type not watermarked"
                );
                continue;
            }
        };

        let table = transition_table_for(state, tenant, &entity_type);
        let blob_store = state.blob_store_for_tenant(tenant).ok();
        let total = entity_ids.len();
        let mut indexed = 0usize;
        let mut empty = 0usize;
        let mut failed = 0usize;

        for entity_id in &entity_ids {
            match load_entity_current_fields(
                tenant,
                &entity_type,
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
                    let vector_rows =
                        crate::vector_index::rows_for_entity_state(&vectors, "Active", &fields);

                    match store
                        .backfill_entity_vectors(
                            tenant.as_str(),
                            &entity_type,
                            entity_id,
                            reconciliation_generation,
                            sequence_nr,
                            &vector_rows,
                        )
                        .await
                    {
                        Ok(()) if vector_rows.is_empty() => empty += 1,
                        Ok(()) => indexed += 1,
                        Err(error) => {
                            failed += 1;
                            tracing::warn!(
                                error = %error,
                                entity_type = %entity_type,
                                entity_id = %entity_id,
                                sequence_nr,
                                "vector index backfill: reconciliation failed"
                            );
                        }
                    }
                }
                EntityLoadOutcome::Skip { sequence_nr } => {
                    if let Err(error) = store
                        .backfill_entity_vectors(
                            tenant.as_str(),
                            &entity_type,
                            entity_id,
                            reconciliation_generation,
                            sequence_nr,
                            &[],
                        )
                        .await
                    {
                        failed += 1;
                        tracing::warn!(
                            error = %error,
                            entity_type = %entity_type,
                            entity_id = %entity_id,
                            sequence_nr,
                            "vector index backfill: purge reconciliation failed"
                        );
                    } else {
                        empty += 1;
                    }
                }
                EntityLoadOutcome::LoadFailed => {
                    failed += 1;
                    tracing::warn!(
                        entity_type = %entity_type,
                        entity_id = %entity_id,
                        "vector index backfill: durable stream could not be loaded; type will not be watermarked"
                    );
                }
            }
            tokio::task::yield_now().await;
        }

        if failed != 0 {
            tracing::warn!(
                tenant = %tenant,
                entity_type = %entity_type,
                total,
                indexed,
                empty,
                failed,
                "vector index backfill incomplete; type not watermarked"
            );
            continue;
        }

        match store
            .mark_vector_index_backfilled(
                tenant.as_str(),
                &entity_type,
                reconciliation_generation,
                &current_set,
            )
            .await
        {
            Ok(()) => tracing::info!(
                tenant = %tenant,
                entity_type = %entity_type,
                vector_set = %current_set,
                total,
                indexed,
                empty,
                "entity_vector_index reconciliation complete; type watermarked"
            ),
            Err(error) => tracing::error!(
                tenant = %tenant,
                entity_type = %entity_type,
                error = %error,
                "vector index backfill converged but watermark persistence failed"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn previously_watermarked_empty_vector_type_remains_in_work_set() {
        let current_vectors = BTreeMap::from([("Item".to_string(), Vec::new())]);
        let covered = BTreeMap::from([
            (
                "Item".to_string(),
                "v2|embed:vector:model:2:cosine".to_string(),
            ),
            ("Legacy".to_string(), "v1|embed".to_string()),
        ]);

        assert_eq!(
            vector_backfill_work_types(&current_vectors, &covered, &BTreeSet::new()),
            BTreeSet::from(["Item".to_string(), "Legacy".to_string()])
        );
    }

    #[test]
    fn interrupted_empty_reconciliation_remains_in_work_set_without_a_watermark() {
        let current_vectors = BTreeMap::from([("Item".to_string(), Vec::new())]);
        let covered = BTreeMap::new();
        let reconciliation_types = BTreeSet::from(["Item".to_string()]);

        assert_eq!(
            vector_backfill_work_types(&current_vectors, &covered, &reconciliation_types),
            BTreeSet::from(["Item".to_string()])
        );
    }
}
