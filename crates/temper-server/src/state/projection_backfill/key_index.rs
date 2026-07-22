//! ADR-0153 declared-key backfill: key `entity_key_index` for pre-existing entities
//! and record the per-(tenant, entity_type) watermark, so keyed hits and misses can
//! be authoritative (retiring #324's full-type scan — the 413, ARN-68).

use temper_runtime::tenant::TenantId;

use crate::ServerState;

use super::{EntityLoadOutcome, load_entity_current_fields};

mod activation;

use activation::prepare_key_index_coverage_for_tables;
pub(in crate::state) use activation::{
    PreparedKeyIndexCoverage, prepare_key_index_coverage_for_activation,
    publish_prepared_key_index_coverage,
};

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

fn reconciliation_sequence(
    outcome: &EntityLoadOutcome,
    catalog_sequence: Option<u64>,
) -> Option<u64> {
    let journal_sequence = loaded_journal_sequence(outcome)?;
    if journal_sequence > 0 {
        return Some(journal_sequence);
    }
    if let Some(snapshot) = loaded_snapshot(outcome) {
        return Some(snapshot.sequence_nr);
    }
    catalog_sequence.or_else(|| loaded_sequence_nr(outcome))
}

/// Backfill current registry tables and publish each successful watermark.
pub(in crate::state) async fn populate_key_index_from_snapshots(
    state: &ServerState,
    tenant: &TenantId,
) {
    let tables = {
        let registry = state.registry.read().expect("registry lock poisoned");
        registry
            .entity_types(tenant)
            .into_iter()
            .filter_map(|entity_type| {
                registry
                    .get_table(tenant, entity_type)
                    .map(|table| (entity_type.to_string(), table.as_ref().clone()))
            })
            .collect::<Vec<_>>()
    };
    for table in tables {
        match prepare_key_index_coverage_for_tables(state, tenant, std::slice::from_ref(&table))
            .await
        {
            Ok(prepared) => {
                if let Err(error) =
                    publish_prepared_key_index_coverage(state, tenant, &prepared).await
                {
                    tracing::warn!(tenant = %tenant, error = %error, "key index backfill did not publish coverage");
                }
            }
            Err(error) => tracing::warn!(
                tenant = %tenant,
                entity_type = %table.0,
                error = %error,
                "key index backfill did not prepare coverage"
            ),
        }
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "key coverage preparation captures one exact tenant/type generation"
)]
async fn prepare_key_index_type(
    state: &ServerState,
    tenant: &TenantId,
    store: &crate::storage::BoxedEventStore,
    backend: crate::storage::BackendLabel,
    entity_type: &str,
    table: &temper_jit::TransitionTable,
    preparing_activation: bool,
    retained_revision: Option<u64>,
) -> Result<Option<PreparedKeyIndexCoverage>, String> {
    let keys = &table.keys;
    let current_key_set = crate::key_index::declared_key_set_signature(keys);
    if let Some(revision) = retained_revision {
        return Ok(Some(PreparedKeyIndexCoverage {
            entity_type: entity_type.to_string(),
            key_set: current_key_set,
            revision,
            total: 0,
            newly_keyed: 0,
            skipped: 0,
        }));
    }
    let covered = if preparing_activation {
        None
    } else {
        state
            .key_index_backfill_covered_key_set(tenant, entity_type)
            .await
    };
    if covered.as_deref() == Some(current_key_set.as_str()) {
        return Ok(None);
    }
    tracing::info!(
        tenant = %tenant,
        entity_type,
        covered_key_set = covered.as_deref().unwrap_or(""),
        current_key_set = %current_key_set,
        "key index backfill: reconciling every durable entity"
    );

    let repair_revision = store
        .begin_key_index_backfill(tenant.as_str(), entity_type, &current_key_set)
        .await
        .map_err(|error| {
            format!("failed to establish key repair contract for {tenant}:{entity_type}: {error}")
        })?;
    let blob_store = state.blob_store_for_tenant(tenant).ok();
    let query_plane = state.query_plane_store();
    let scan_boundary = store
        .key_reconciliation_boundary(tenant.as_str(), entity_type)
        .await
        .map_err(|error| {
            format!("failed to capture key repair boundary for {tenant}:{entity_type}: {error}")
        })?;
    const KEY_REPAIR_PAGE_BUDGET: usize = 256;
    let mut cursor = None;
    let mut total = 0usize;
    let mut newly_keyed = 0usize;
    let mut skipped = 0usize;
    const FAILED_ENTITY_SAMPLE_BUDGET: usize = 8;
    let mut failed_count = 0usize;
    let mut failed_sample = Vec::with_capacity(FAILED_ENTITY_SAMPLE_BUDGET);

    while let Some(scan_boundary) = scan_boundary.as_deref() {
        let page = store
            .list_key_reconciliation_page(
                tenant.as_str(),
                entity_type,
                cursor.as_deref(),
                scan_boundary,
                KEY_REPAIR_PAGE_BUDGET,
            )
            .await
            .map_err(|error| {
                format!(
                    "failed to enumerate bounded key repair page for {tenant}:{entity_type}: {error}"
                )
            })?;
        if page.is_empty() {
            break;
        }
        assert!(
            page.len() <= KEY_REPAIR_PAGE_BUDGET,
            "key repair store exceeded the requested page budget"
        );
        let next_cursor = page
            .last()
            .expect("non-empty key repair page has a last entity")
            .entity_id
            .clone();
        total = total.checked_add(page.len()).ok_or_else(|| {
            format!("key repair entity count overflow for {tenant}:{entity_type}")
        })?;

        for candidate in page {
            let entity_id = candidate.entity_id;
            let is_live = candidate.is_live;
            let loaded = load_entity_current_fields(
                tenant,
                entity_type,
                &entity_id,
                Some(table),
                store,
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
                        let catalog_row = if let Some(query_plane) = query_plane.as_ref() {
                            match query_plane
                                .load_entity_catalog_rows(
                                    tenant.as_str(),
                                    entity_type,
                                    std::slice::from_ref(&entity_id),
                                )
                                .await
                            {
                                Ok(Some(rows)) => {
                                    Ok(rows.into_iter().find(|row| row.entity_id == entity_id))
                                }
                                Ok(None) => Ok(None),
                                Err(error) => Err(error),
                            }
                        } else {
                            Ok(None)
                        };
                        match catalog_row {
                            Err(error) => {
                                tracing::warn!(error = %error, entity_type, entity_id, "key repair catalog fallback failed");
                                KeyRepairDisposition::Unresolved
                            }
                            Ok(row) if !is_live => match reconciliation_sequence(
                                &outcome,
                                row.as_ref().map(|catalog| catalog.sequence_nr),
                            ) {
                                Some(sequence_nr) => KeyRepairDisposition::Reconcile {
                                    fields: None,
                                    sequence_nr,
                                    journal_sequence: observed_journal_sequence,
                                    snapshot: captured_snapshot,
                                },
                                None => KeyRepairDisposition::Unresolved,
                            },
                            Ok(row) => {
                                let sequence_nr = reconciliation_sequence(
                                    &outcome,
                                    row.as_ref().map(|catalog| catalog.sequence_nr),
                                )
                                .expect("loaded outcome has a durable sequence");
                                match outcome {
                                    EntityLoadOutcome::Deleted {
                                        journal_sequence,
                                        snapshot,
                                        ..
                                    } => KeyRepairDisposition::Reconcile {
                                        fields: None,
                                        sequence_nr,
                                        journal_sequence,
                                        snapshot,
                                    },
                                    _ => match row {
                                        Some(row) if row.status == "Deleted" => {
                                            KeyRepairDisposition::Reconcile {
                                                fields: None,
                                                sequence_nr,
                                                journal_sequence: observed_journal_sequence,
                                                snapshot: captured_snapshot,
                                            }
                                        }
                                        Some(row) => KeyRepairDisposition::Reconcile {
                                            fields: Some(row.fields),
                                            sequence_nr,
                                            journal_sequence: observed_journal_sequence,
                                            snapshot: captured_snapshot,
                                        },
                                        None => KeyRepairDisposition::Unresolved,
                                    },
                                }
                            }
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
                    match store
                        .backfill_entity_keys(
                            tenant.as_str(),
                            entity_type,
                            &entity_id,
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
                        Err(error) => {
                            tracing::warn!(error = %error, entity_type, entity_id, "key repair write lost its source fence");
                            failed_count += 1;
                            if failed_sample.len() < FAILED_ENTITY_SAMPLE_BUDGET {
                                failed_sample.push(entity_id.clone());
                            }
                        }
                    }
                }
                KeyRepairDisposition::Unresolved => {
                    failed_count += 1;
                    if failed_sample.len() < FAILED_ENTITY_SAMPLE_BUDGET {
                        failed_sample.push(entity_id.clone());
                    }
                }
            }
            tokio::task::yield_now().await;
        }
        cursor = Some(next_cursor);
    }

    if failed_count > 0 {
        return Err(format!(
            "key repair left {} unresolved entities for {tenant}:{entity_type}: {}",
            failed_count,
            failed_sample.join(",")
        ));
    }
    Ok(Some(PreparedKeyIndexCoverage {
        entity_type: entity_type.to_string(),
        key_set: current_key_set,
        revision: repair_revision,
        total,
        newly_keyed,
        skipped,
    }))
}
