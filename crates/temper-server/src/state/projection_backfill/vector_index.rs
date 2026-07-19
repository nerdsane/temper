//! ADR-0181 sequence-monotonic vector-index reconciliation.
//!
//! Every repair enumerates durable journal streams (including deleted entities),
//! rebuilds current rows from a strict replay, and carries that replay's journal
//! sequence into a store-level compare-and-reconcile transaction. Completion is
//! watermarked only after every stream converges durably.

use std::collections::{BTreeMap, BTreeSet};

use temper_runtime::persistence::PersistenceError;
use temper_runtime::tenant::TenantId;

use crate::{ServerState, storage::BoxedEventStore};

use super::{EntityLoadOutcome, load_entity_current_fields};

fn vector_backfill_work_types(
    current_types: &BTreeSet<String>,
    covered: &BTreeMap<String, String>,
    reconciliation_types: &BTreeSet<String>,
) -> BTreeSet<String> {
    let mut work_types = current_types.clone();
    work_types.extend(covered.keys().cloned());
    work_types.extend(reconciliation_types.iter().cloned());
    work_types
}

async fn durable_stream_sequence(
    store: &BoxedEventStore,
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
) -> Result<u64, PersistenceError> {
    let persistence_id = format!("{tenant}:{entity_type}:{entity_id}");
    let snapshot_sequence = store
        .load_snapshot(&persistence_id)
        .await?
        .map(|(sequence_nr, _)| sequence_nr)
        .unwrap_or(0);
    let events = store
        .read_events(&persistence_id, snapshot_sequence)
        .await?;
    Ok(events
        .last()
        .map(|event| event.sequence_nr)
        .unwrap_or(snapshot_sequence))
}

/// Backfill `entity_vector_index` for existing entities, then record the watermark.
///
/// Idempotent and safe alongside live writes: a rebuild observed at sequence N is
/// ignored by the store after a live append advances that entity's fence to N+1.
pub(in crate::state) async fn populate_vector_index_from_snapshots(
    state: &ServerState,
    tenant: &TenantId,
) {
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

    // The work set needs only type names. Declarations themselves are snapshotted
    // later under the short snapshot+generation critical section.
    let (mut current_types, uses_legacy_tables): (BTreeSet<String>, bool) = {
        let registry = state
            .registry
            .read()
            .expect("spec registry lock poisoned while listing vector declarations");
        (
            registry
                .entity_types(tenant)
                .into_iter()
                .map(ToString::to_string)
                .collect(),
            registry.get_tenant(tenant).is_none(),
        )
    };
    if uses_legacy_tables {
        current_types.extend(state.transition_tables.keys().cloned());
    }

    let work_types = vector_backfill_work_types(&current_types, &covered, &reconciliation_types);

    for entity_type in work_types {
        // Serialize only declaration snapshot + durable generation allocation.
        // Replaying journals and writing rows happens after this guard is released,
        // so unrelated tenants and entity types are not blocked by a long rebuild.
        let reconciliation_guard = state.vector_reconciliation_lock.lock().await;
        let (table, declaration_revision, declaration_fingerprint) = {
            let registry = state
                .registry
                .read()
                .expect("spec registry lock poisoned during vector reconciliation");
            if let Some(config) = registry.get_tenant(tenant) {
                if let Some(spec) = config.entities.get(&entity_type) {
                    let table = spec.table();
                    let fingerprint = table
                        .spec_declaration_fingerprint
                        .clone()
                        .unwrap_or_else(|| temper_store_turso::spec_content_hash(&spec.ioa_source));
                    (Some(table), config.revision, fingerprint)
                } else {
                    (None, config.revision, "absent:v1".to_string())
                }
            } else if let Some(table) = state.transition_tables.get(&entity_type).cloned() {
                let fingerprint = table
                    .spec_declaration_fingerprint
                    .clone()
                    .unwrap_or_else(|| "absent:v1".to_string());
                (Some(table), 1, fingerprint)
            } else {
                (None, 1, "absent:v1".to_string())
            }
        };
        let vectors = table
            .as_deref()
            .map(|table| table.vectors.clone())
            .unwrap_or_default();
        if vectors.is_empty()
            && !covered.contains_key(&entity_type)
            && !reconciliation_types.contains(&entity_type)
        {
            continue;
        }
        let current_set = crate::vector_index::declared_vector_set_signature(&vectors);
        if let Some(previous_set) = covered.get(&entity_type)
            && previous_set != &current_set
        {
            tracing::info!(
                tenant = %tenant,
                entity_type = %entity_type,
                covered_set = %previous_set,
                current_set = %current_set,
                "vector index backfill: reconciliation signature changed; rebuilding every durable stream"
            );
        }

        let reconciliation_generation = match store
            .begin_vector_index_reconciliation(
                tenant.as_str(),
                &entity_type,
                &current_set,
                declaration_revision,
                &declaration_fingerprint,
            )
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

        // A cached watermark cannot be trusted before the declaration barrier:
        // spec persistence may have withdrawn it after the initial tenant-wide
        // read. Re-read after `begin` while coordinators are serialized. An exact
        // retry keeps the watermark; a new declaration generation removes it.
        let already_complete = match store.vector_index_backfilled_types(tenant.as_str()).await {
            Ok(types) => types.into_iter().any(|(completed_type, completed_set)| {
                completed_type == entity_type && completed_set == current_set
            }),
            Err(error) => {
                tracing::error!(
                    tenant = %tenant,
                    entity_type = %entity_type,
                    error = %error,
                    "vector index backfill: failed to revalidate completion after declaration barrier"
                );
                continue;
            }
        };
        drop(reconciliation_guard);
        if already_complete {
            continue;
        }

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

        let blob_store = state.blob_store_for_tenant(tenant).ok();
        let total = entity_ids.len();
        let mut indexed = 0usize;
        let mut empty = 0usize;
        let mut failed = 0usize;

        for entity_id in &entity_ids {
            if table.is_none() {
                match durable_stream_sequence(&store, tenant, &entity_type, entity_id).await {
                    Ok(sequence_nr) => {
                        match store
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
                            Ok(()) => empty += 1,
                            Err(error) => {
                                failed += 1;
                                tracing::warn!(
                                    error = %error,
                                    entity_type = %entity_type,
                                    entity_id = %entity_id,
                                    sequence_nr,
                                    "vector index backfill: absent-declaration purge failed"
                                );
                            }
                        }
                    }
                    Err(error) => {
                        failed += 1;
                        tracing::warn!(
                            error = %error,
                            entity_type = %entity_type,
                            entity_id = %entity_id,
                            "vector index backfill: absent-declaration stream sequence could not be loaded"
                        );
                    }
                }
                tokio::task::yield_now().await;
                continue;
            }
            match load_entity_current_fields(
                tenant,
                &entity_type,
                entity_id,
                table.as_deref(),
                &store,
                backend,
                blob_store.as_ref(),
            )
            .await
            {
                EntityLoadOutcome::Fields {
                    fields,
                    status,
                    sequence_nr,
                } => {
                    let vector_rows =
                        crate::vector_index::rows_for_entity_state(&vectors, &status, &fields);

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
        let current_types = BTreeSet::from(["Item".to_string()]);
        let covered = BTreeMap::from([
            (
                "Item".to_string(),
                "v2|embed:vector:model:2:cosine".to_string(),
            ),
            ("Legacy".to_string(), "v1|embed".to_string()),
        ]);

        assert_eq!(
            vector_backfill_work_types(&current_types, &covered, &BTreeSet::new()),
            BTreeSet::from(["Item".to_string(), "Legacy".to_string()])
        );
    }

    #[test]
    fn interrupted_empty_reconciliation_remains_in_work_set_without_a_watermark() {
        let current_types = BTreeSet::from(["Item".to_string()]);
        let covered = BTreeMap::new();
        let reconciliation_types = BTreeSet::from(["Item".to_string()]);

        assert_eq!(
            vector_backfill_work_types(&current_types, &covered, &reconciliation_types),
            BTreeSet::from(["Item".to_string()])
        );
    }
}
