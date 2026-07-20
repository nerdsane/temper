//! ADR-0153 declared-key backfill: key `entity_key_index` for pre-existing entities
//! and record the per-(tenant, entity_type) watermark, so keyed hits and misses can
//! be authoritative (retiring #324's full-type scan — the 413, ARN-68).

use std::collections::BTreeSet;

use temper_runtime::tenant::TenantId;

use crate::ServerState;

use super::{EntityLoadOutcome, load_entity_current_fields, transition_table_for};

enum KeyRepairDisposition {
    Reconcile {
        fields: Option<serde_json::Value>,
        sequence_nr: u64,
        journal_sequence: u64,
        snapshot: Option<crate::entity_actor::CapturedEntitySnapshot>,
    },
    Unresolved,
}

fn loaded_sequence_nr(outcome: &EntityLoadOutcome) -> Option<u64> {
    match outcome {
        EntityLoadOutcome::Fields { sequence_nr, .. }
        | EntityLoadOutcome::Deleted { sequence_nr, .. }
        | EntityLoadOutcome::Missing { sequence_nr, .. } => Some(*sequence_nr),
        EntityLoadOutcome::LoadFailed => None,
    }
}

fn loaded_journal_sequence(outcome: &EntityLoadOutcome) -> Option<u64> {
    match outcome {
        EntityLoadOutcome::Fields {
            journal_sequence, ..
        }
        | EntityLoadOutcome::Deleted {
            journal_sequence, ..
        }
        | EntityLoadOutcome::Missing {
            journal_sequence, ..
        } => Some(*journal_sequence),
        EntityLoadOutcome::LoadFailed => None,
    }
}

fn loaded_snapshot(
    outcome: &EntityLoadOutcome,
) -> Option<&crate::entity_actor::CapturedEntitySnapshot> {
    match outcome {
        EntityLoadOutcome::Fields { snapshot, .. }
        | EntityLoadOutcome::Deleted { snapshot, .. }
        | EntityLoadOutcome::Missing { snapshot, .. } => snapshot.as_ref(),
        EntityLoadOutcome::LoadFailed => None,
    }
}

/// Backfill `entity_key_index` for existing entities, then record the watermark.
///
/// Enumeration is authoritative: keyed types come from the registry and their entity
/// ids from `store.list_entity_ids_for_key_reconciliation`. This repair-specific
/// enumeration unions journal streams, snapshots, query-plane catalog/field rows,
/// and key-index-only phantoms. It must NOT read `state.entity_index`, which is
/// populated only when an actor spawns (lazy) and is therefore near-empty at boot —
/// the original bug that left ~0 of N entities keyed.
///
/// Robustness at scale (tenants hold 10k–100k+ entities of a keyed type):
/// - **Repairing**: every incomplete type is fully replayed. Merely having a key row
///   is not proof that the row is current, so a partial or pre-ADR-0171 index is never
///   trusted as a resumability checkpoint.
/// - **Sound**: a type is watermarked only if EVERY existing entity was either keyed
///   or is definitively skippable (deleted/phantom). One entity that exists but
///   cannot be loaded fails the type — it is not watermarked, and keyed queries keep
///   scanning (correct, just not bounded) until the data issue is resolved. The
///   failing `entity_id`s are logged so the integrity problem is visible.
/// - **Cooperative**: yields between entities and runs as a background task, so the
///   heavy types (e.g. SessionEntry) do not block boot or the smaller, higher-value
///   types. Types are processed in the registry's sorted order.
///
/// Idempotent; entities written after the key was declared already co-commit their
/// keys at write time.
pub(in crate::state) async fn populate_key_index_from_snapshots(
    state: &ServerState,
    tenant: &TenantId,
) {
    let Some((store, backend)) = state.event_journal() else {
        return;
    };
    if !store.supports_authoritative_key_index() {
        tracing::debug!(
            tenant = %tenant,
            ?backend,
            "key index backfill skipped: backend does not maintain authoritative declared keys"
        );
        return;
    }

    // Keyed entity types from the registry — the authoritative record of what is
    // installed for this tenant (os-app entities live here, not in transition_tables).
    let keyed_types: Vec<(String, Vec<temper_jit::table::types::DeclaredKey>)> = {
        let registry = state.registry.read().unwrap();
        registry
            .entity_types(tenant)
            .into_iter()
            .filter_map(|entity_type| {
                let table = registry.get_table(tenant, entity_type)?;
                if table.keys.is_empty() {
                    None
                } else {
                    Some((entity_type.to_string(), table.keys.clone()))
                }
            })
            .collect()
    };

    for (entity_type, keys) in &keyed_types {
        let current_key_set = crate::key_index::declared_key_set_signature(keys);
        let covered = state
            .key_index_backfill_covered_key_set(tenant, entity_type)
            .await;
        // Already complete for the CURRENT declared key-set: the co-committed write path
        // keeps the index whole, so skip the re-scan.
        if covered.as_deref() == Some(current_key_set.as_str()) {
            continue;
        }
        // No matching watermark means the index is not authoritative. This includes
        // first boot, an interrupted prior pass, and a derivation-contract upgrade.
        // Existing rows can therefore be stale or incomplete and must never be used
        // to skip replay. Reconcile every entity before recording the v3 signature.
        tracing::info!(
            tenant = %tenant, entity_type = %entity_type,
            covered_key_set = covered.as_deref().unwrap_or(""),
            current_key_set = %current_key_set,
            "key index backfill: incomplete declared-key signature — reconciling every existing entity"
        );

        // Establish the target contract BEFORE replay starts. Any live writer still
        // using a different spec signature now advances the revision, so the final
        // compare-and-set cannot certify rows repaired across that mixed contract.
        let repair_revision = match store
            .begin_key_index_backfill(tenant.as_str(), entity_type, &current_key_set)
            .await
        {
            Ok(revision) => revision,
            Err(e) => {
                tracing::error!(
                    tenant = %tenant, entity_type = %entity_type, error = %e,
                    "key index backfill: failed to establish target contract; type not watermarked"
                );
                continue;
            }
        };

        let entity_ids = match store
            .list_entity_ids_for_key_reconciliation(tenant.as_str(), entity_type)
            .await
        {
            Ok(ids) => ids,
            Err(e) => {
                tracing::error!(
                    tenant = %tenant, entity_type = %entity_type, error = %e,
                    "key index backfill: failed to enumerate entities; type not watermarked"
                );
                continue;
            }
        };
        let live_entity_ids: BTreeSet<String> = match store
            .list_entity_ids_by_type(tenant.as_str(), entity_type)
            .await
        {
            Ok(ids) => ids.into_iter().collect(),
            Err(e) => {
                tracing::error!(
                    tenant = %tenant, entity_type = %entity_type, error = %e,
                    "key index backfill: failed to classify live entities; type not watermarked"
                );
                continue;
            }
        };

        let table = transition_table_for(state, tenant, entity_type);
        let blob_store = state.blob_store_for_tenant(tenant).ok();
        let query_plane = state.query_plane_store();
        let total = entity_ids.len();
        let mut newly_keyed = 0usize;
        let mut skipped = 0usize;
        let mut failed = 0usize;

        for entity_id in &entity_ids {
            let is_live = live_entity_ids.contains(entity_id);
            let loaded = load_entity_current_fields(
                tenant,
                entity_type,
                entity_id,
                table.as_ref(),
                &store,
                backend,
                blob_store.as_ref(),
            )
            .await;
            let disposition = match loaded {
                EntityLoadOutcome::Fields {
                    fields,
                    sequence_nr,
                    journal_sequence,
                    snapshot,
                } if is_live => KeyRepairDisposition::Reconcile {
                    fields: Some(fields),
                    sequence_nr,
                    journal_sequence,
                    snapshot,
                },
                EntityLoadOutcome::LoadFailed => KeyRepairDisposition::Unresolved,
                EntityLoadOutcome::Missing {
                    journal_sequence, ..
                } if journal_sequence > 0 => KeyRepairDisposition::Unresolved,
                outcome => match loaded_journal_sequence(&outcome) {
                    None => KeyRepairDisposition::Unresolved,
                    Some(observed_journal_sequence) => {
                        let captured_snapshot = loaded_snapshot(&outcome).cloned();
                        // ADR-0077 migrations can leave the query-plane catalog as the
                        // only durable representation. Use it only when replay did not
                        // produce current fields; the sequence fence below rejects a
                        // catalog row that trails a newer event or snapshot.
                        let catalog_row = if let Some(query_plane) = query_plane.as_ref() {
                            match query_plane
                                .load_entity_catalog_rows(
                                    tenant.as_str(),
                                    entity_type,
                                    std::slice::from_ref(entity_id),
                                )
                                .await
                            {
                                Ok(Some(rows)) => {
                                    Ok(rows.into_iter().find(|row| row.entity_id == *entity_id))
                                }
                                Ok(None) => Ok(None),
                                Err(e) => Err(e),
                            }
                        } else {
                            Ok(None)
                        };

                        match catalog_row {
                            Err(e) => {
                                tracing::warn!(
                                    error = %e, entity_type = %entity_type, entity_id = %entity_id,
                                    "key index backfill: catalog fallback failed"
                                );
                                KeyRepairDisposition::Unresolved
                            }
                            Ok(row) if !is_live => {
                                // Live enumeration is tombstone-aware. A deleted journal
                                // therefore outranks a later stale snapshot or catalog
                                // row. Fence the purge at the newest derived sequence so
                                // the exact repair cannot race a still-newer write.
                                match loaded_sequence_nr(&outcome)
                                    .max(row.as_ref().map(|catalog| catalog.sequence_nr))
                                {
                                    Some(sequence_nr) => KeyRepairDisposition::Reconcile {
                                        fields: None,
                                        sequence_nr,
                                        journal_sequence: observed_journal_sequence,
                                        snapshot: captured_snapshot,
                                    },
                                    None => KeyRepairDisposition::Unresolved,
                                }
                            }
                            Ok(row) => match outcome {
                                EntityLoadOutcome::Deleted {
                                    sequence_nr,
                                    journal_sequence,
                                    snapshot,
                                } => KeyRepairDisposition::Reconcile {
                                    fields: None,
                                    sequence_nr: sequence_nr.max(
                                        row.as_ref()
                                            .map(|catalog| catalog.sequence_nr)
                                            .unwrap_or(0),
                                    ),
                                    journal_sequence,
                                    snapshot,
                                },
                                _ => match row {
                                    Some(row) if row.status == "Deleted" => {
                                        KeyRepairDisposition::Reconcile {
                                            fields: None,
                                            sequence_nr: row.sequence_nr,
                                            journal_sequence: observed_journal_sequence,
                                            snapshot: captured_snapshot,
                                        }
                                    }
                                    Some(row) => KeyRepairDisposition::Reconcile {
                                        fields: Some(row.fields),
                                        sequence_nr: row.sequence_nr,
                                        journal_sequence: observed_journal_sequence,
                                        snapshot: captured_snapshot,
                                    },
                                    None => {
                                        tracing::warn!(
                                            entity_type = %entity_type, entity_id = %entity_id,
                                            "key index backfill: durable entity has no reconstructable journal, snapshot, or catalog state"
                                        );
                                        KeyRepairDisposition::Unresolved
                                    }
                                },
                            },
                        }
                    }
                },
            };

            match disposition {
                KeyRepairDisposition::Reconcile {
                    fields,
                    sequence_nr,
                    journal_sequence,
                    snapshot,
                } => {
                    let mut key_rows = Vec::new();
                    if let Some(field_map) = fields.as_ref().and_then(serde_json::Value::as_object)
                    {
                        for key in keys {
                            if let Some(hash) = crate::key_index::canonical_key_hash(
                                &key.name,
                                &key.properties,
                                field_map,
                            ) {
                                key_rows.push(temper_runtime::persistence::EntityKeyRow {
                                    key_name: key.name.clone(),
                                    key_hash: hash,
                                });
                            }
                        }
                    }
                    // Exact reconciliation is required even when no current row is
                    // resolvable (all-null/non-scalar): an empty set purges any stale
                    // ownership left by the previous derivation contract.
                    match store
                        .backfill_entity_keys(
                            tenant.as_str(),
                            entity_type,
                            entity_id,
                            sequence_nr,
                            temper_runtime::persistence::KeyIndexBackfillFence {
                                key_set_signature: &current_key_set,
                                contract_revision: repair_revision,
                                expected_journal_sequence: journal_sequence,
                                expected_entity_live: is_live,
                                expected_snapshot: snapshot.as_ref().map(|snapshot| {
                                    temper_runtime::persistence::SnapshotBackfillFence {
                                        sequence_nr: snapshot.sequence_nr,
                                        state: &snapshot.state,
                                    }
                                }),
                            },
                            &key_rows,
                        )
                        .await
                    {
                        Ok(()) if key_rows.is_empty() => skipped += 1,
                        Ok(()) => newly_keyed += 1,
                        Err(e) => {
                            failed += 1;
                            tracing::warn!(
                                error = %e, entity_type = %entity_type, entity_id = %entity_id,
                                "key index backfill: upsert failed"
                            );
                        }
                    }
                }
                KeyRepairDisposition::Unresolved => {
                    failed += 1;
                    tracing::warn!(
                        entity_type = %entity_type, entity_id = %entity_id,
                        "key index backfill: existing entity could not be loaded; type will NOT be watermarked (data integrity issue)"
                    );
                }
            }
            tokio::task::yield_now().await;
        }

        // Watermark only if nothing failed — every existing entity was keyed or is
        // definitively skippable. Otherwise keyed queries keep scanning (sound), and a
        // later boot retries the full type from authoritative state.
        if failed == 0 {
            match state
                .mark_key_index_backfilled_if_revision(
                    tenant,
                    entity_type,
                    &current_key_set,
                    repair_revision,
                )
                .await
            {
                Ok(true) => tracing::info!(
                    tenant = %tenant, entity_type = %entity_type, key_set = %current_key_set,
                    total, newly_keyed, skipped, repair_revision,
                    "entity_key_index backfill complete; type watermarked"
                ),
                Ok(false) => tracing::warn!(
                    tenant = %tenant, entity_type = %entity_type, key_set = %current_key_set,
                    repair_revision,
                    "key index backfill: live key contract changed during repair; type NOT watermarked"
                ),
                Err(e) => tracing::error!(
                    tenant = %tenant, entity_type = %entity_type, error = %e,
                    "key index backfill: failed to publish fenced watermark"
                ),
            }
        } else {
            tracing::warn!(
                tenant = %tenant, entity_type = %entity_type,
                total, newly_keyed, skipped, failed,
                "key index backfill: {failed} entities unresolved; type NOT watermarked (keyed queries keep scanning; will retry the full type next run)"
            );
        }
    }
}
