use std::collections::BTreeMap;
use std::time::Instant;

use temper_runtime::tenant::TenantId;

use crate::entity_actor::{EntityRecoveryContext, recover_entity_state_from_stable_sources};
use crate::state::{QueryProjectionReplayParityReport, ServerState};
use crate::storage::{CatalogRowsLoad, load_catalog_rows_by_id};

mod metrics;

use metrics::{
    ReplayParityExample, push_replay_parity_example, record_replay_parity_run_summary,
    replay_parity_drift_kind, replay_parity_sequence_gap,
};

pub(in crate::state) async fn verify_query_projection_replay_parity(
    state: &ServerState,
    tenant: &TenantId,
    entity_type_filter: Option<&str>,
    entity_limit: Option<usize>,
    source: &str,
) -> Result<QueryProjectionReplayParityReport, String> {
    let run_started_at = Instant::now(); // determinism-ok: production-only parity verifier duration metric
    let Some((store, backend)) = state.event_journal() else {
        record_replay_parity_run_summary(
            tenant,
            entity_type_filter,
            source,
            "error",
            0,
            0,
            0,
            1,
            run_started_at.elapsed(),
        );
        return Err("event journal is not configured".to_string());
    };
    let Some(query_plane) = state.query_plane_store() else {
        record_replay_parity_run_summary(
            tenant,
            entity_type_filter,
            source,
            "error",
            0,
            0,
            0,
            1,
            run_started_at.elapsed(),
        );
        return Err("query-plane store is not configured".to_string());
    };

    let mut entities = if let Some(entity_limit) = entity_limit {
        store
            .list_entity_ids_limited(tenant.as_str(), entity_type_filter, entity_limit)
            .await
            .map_err(|error| format!("list bounded persisted entity ids failed: {error}"))?
    } else {
        let mut entities = store
            .list_entity_ids(tenant.as_str())
            .await
            .map_err(|error| format!("list persisted entity ids failed: {error}"))?;
        if let Some(entity_type_filter) = entity_type_filter {
            entities.retain(|(entity_type, _)| entity_type == entity_type_filter);
        }
        entities
    };
    entities.sort();
    let mut by_type = BTreeMap::<String, Vec<String>>::new();
    for (entity_type, entity_id) in entities {
        by_type.entry(entity_type).or_default().push(entity_id);
    }

    let mut report = QueryProjectionReplayParityReport {
        tenant: tenant.as_str().to_string(),
        entity_type: entity_type_filter.map(str::to_string),
        entity_limit: entity_limit.map(|limit| limit as u64),
        ..Default::default()
    };

    for (entity_type, entity_ids) in by_type {
        let Some(table) = super::transition_table_for(state, tenant, &entity_type) else {
            for entity_id in entity_ids {
                let started_at = Instant::now(); // determinism-ok: production-only parity verifier duration metric
                report.checked += 1;
                report.errors += 1;
                push_replay_parity_example(
                    &mut report,
                    ReplayParityExample {
                        entity_type: &entity_type,
                        entity_id: &entity_id,
                        drift_kind: "missing_table",
                        sequence_direction: "unknown",
                        sequence_gap: 0,
                        catalog_sequence: None,
                        authoritative_sequence: 0,
                    },
                );
                crate::query_projection_metrics::record_replay_parity_check(
                    tenant.as_str(),
                    &entity_type,
                    "error",
                    "missing_table",
                    "unknown",
                    0,
                    started_at.elapsed(),
                );
            }
            continue;
        };

        let catalog_rows =
            match load_catalog_rows_by_id(&query_plane, tenant.as_str(), &entity_type, &entity_ids)
                .await
            {
                Ok(CatalogRowsLoad::Available(rows)) => rows,
                Ok(CatalogRowsLoad::Unsupported) => {
                    for entity_id in entity_ids {
                        let started_at = Instant::now(); // determinism-ok: production-only parity verifier duration metric
                        report.checked += 1;
                        report.errors += 1;
                        push_replay_parity_example(
                            &mut report,
                            ReplayParityExample {
                                entity_type: &entity_type,
                                entity_id: &entity_id,
                                drift_kind: "catalog_unavailable",
                                sequence_direction: "unknown",
                                sequence_gap: 0,
                                catalog_sequence: None,
                                authoritative_sequence: 0,
                            },
                        );
                        crate::query_projection_metrics::record_replay_parity_check(
                            tenant.as_str(),
                            &entity_type,
                            "error",
                            "catalog_unavailable",
                            "unknown",
                            0,
                            started_at.elapsed(),
                        );
                    }
                    continue;
                }
                Err(error) => {
                    for entity_id in entity_ids {
                        let started_at = Instant::now(); // determinism-ok: production-only parity verifier duration metric
                        report.checked += 1;
                        report.errors += 1;
                        push_replay_parity_example(
                            &mut report,
                            ReplayParityExample {
                                entity_type: &entity_type,
                                entity_id: &entity_id,
                                drift_kind: "catalog_error",
                                sequence_direction: "unknown",
                                sequence_gap: 0,
                                catalog_sequence: None,
                                authoritative_sequence: 0,
                            },
                        );
                        crate::query_projection_metrics::record_replay_parity_check(
                            tenant.as_str(),
                            &entity_type,
                            "error",
                            "catalog_error",
                            "unknown",
                            0,
                            started_at.elapsed(),
                        );
                    }
                    tracing::warn!(
                        tenant = %tenant,
                        entity_type = %entity_type,
                        error = %error,
                        "query projection replay parity could not load catalog rows"
                    );
                    continue;
                }
            };

        for entity_id in entity_ids {
            let started_at = Instant::now(); // determinism-ok: production-only parity verifier duration metric
            report.checked += 1;
            let tenant_blob_store = state.blob_store_for_tenant(tenant).ok();
            let replayed = recover_entity_state_from_stable_sources(EntityRecoveryContext {
                tenant: tenant.as_str(),
                entity_type: &entity_type,
                entity_id: &entity_id,
                table: &table,
                store: &store,
                backend,
                initial_fields: &serde_json::json!({}),
                blob_store: tenant_blob_store.as_ref(),
            })
            .await;
            let (replayed, authoritative_sequence) = match replayed {
                Ok(source) => {
                    let authoritative_sequence = source.durable_sequence();
                    let Some(state) = source.state else {
                        report.errors += 1;
                        push_replay_parity_example(
                            &mut report,
                            ReplayParityExample {
                                entity_type: &entity_type,
                                entity_id: &entity_id,
                                drift_kind: "replay_absent",
                                sequence_direction: "unknown",
                                sequence_gap: 0,
                                catalog_sequence: catalog_rows
                                    .get(&entity_id)
                                    .map(|catalog| catalog.sequence_nr),
                                authoritative_sequence,
                            },
                        );
                        continue;
                    };
                    (state, authoritative_sequence)
                }
                Err(error) => {
                    report.errors += 1;
                    push_replay_parity_example(
                        &mut report,
                        ReplayParityExample {
                            entity_type: &entity_type,
                            entity_id: &entity_id,
                            drift_kind: "replay_error",
                            sequence_direction: "unknown",
                            sequence_gap: 0,
                            catalog_sequence: catalog_rows
                                .get(&entity_id)
                                .map(|catalog| catalog.sequence_nr),
                            authoritative_sequence: 0,
                        },
                    );
                    crate::query_projection_metrics::record_replay_parity_check(
                        tenant.as_str(),
                        &entity_type,
                        "error",
                        "replay_error",
                        "unknown",
                        0,
                        started_at.elapsed(),
                    );
                    tracing::warn!(
                        tenant = %tenant,
                        entity_type = %entity_type,
                        entity_id = %entity_id,
                        error = %error,
                        "query projection replay parity could not replay entity"
                    );
                    continue;
                }
            };
            let catalog = catalog_rows.get(&entity_id);

            if replayed.status == "Deleted" {
                if let Some(catalog) = catalog {
                    report.drifted += 1;
                    let (sequence_direction, sequence_gap) = replay_parity_sequence_gap(
                        Some(catalog.sequence_nr),
                        authoritative_sequence,
                    );
                    push_replay_parity_example(
                        &mut report,
                        ReplayParityExample {
                            entity_type: &entity_type,
                            entity_id: &entity_id,
                            drift_kind: "deleted_present",
                            sequence_direction,
                            sequence_gap,
                            catalog_sequence: Some(catalog.sequence_nr),
                            authoritative_sequence,
                        },
                    );
                    crate::query_projection_metrics::record_replay_parity_check(
                        tenant.as_str(),
                        &entity_type,
                        "drift",
                        "deleted_present",
                        sequence_direction,
                        sequence_gap,
                        started_at.elapsed(),
                    );
                } else {
                    report.deleted_absent += 1;
                    crate::query_projection_metrics::record_replay_parity_check(
                        tenant.as_str(),
                        &entity_type,
                        "match",
                        "deleted_absent",
                        "catalog_absent",
                        0,
                        started_at.elapsed(),
                    );
                }
                continue;
            }

            let Some(catalog) = catalog else {
                report.missing += 1;
                report.drifted += 1;
                let (sequence_direction, sequence_gap) =
                    replay_parity_sequence_gap(None, authoritative_sequence);
                push_replay_parity_example(
                    &mut report,
                    ReplayParityExample {
                        entity_type: &entity_type,
                        entity_id: &entity_id,
                        drift_kind: "missing_catalog",
                        sequence_direction,
                        sequence_gap,
                        catalog_sequence: None,
                        authoritative_sequence,
                    },
                );
                crate::query_projection_metrics::record_replay_parity_check(
                    tenant.as_str(),
                    &entity_type,
                    "drift",
                    "missing_catalog",
                    sequence_direction,
                    sequence_gap,
                    started_at.elapsed(),
                );
                continue;
            };

            let projected_fields =
                state.query_projection_fields(tenant, &entity_type, &replayed.fields);
            let projected_state = state.query_projection_state(&replayed);
            let drift_kind = replay_parity_drift_kind(
                catalog,
                &replayed.status,
                &projected_fields,
                &projected_state,
                authoritative_sequence,
            );
            let (sequence_direction, sequence_gap) =
                replay_parity_sequence_gap(Some(catalog.sequence_nr), authoritative_sequence);
            if drift_kind == "none" {
                report.matched += 1;
                crate::query_projection_metrics::record_replay_parity_check(
                    tenant.as_str(),
                    &entity_type,
                    "match",
                    "none",
                    sequence_direction,
                    sequence_gap,
                    started_at.elapsed(),
                );
            } else {
                report.drifted += 1;
                push_replay_parity_example(
                    &mut report,
                    ReplayParityExample {
                        entity_type: &entity_type,
                        entity_id: &entity_id,
                        drift_kind,
                        sequence_direction,
                        sequence_gap,
                        catalog_sequence: Some(catalog.sequence_nr),
                        authoritative_sequence,
                    },
                );
                crate::query_projection_metrics::record_replay_parity_check(
                    tenant.as_str(),
                    &entity_type,
                    "drift",
                    drift_kind,
                    sequence_direction,
                    sequence_gap,
                    started_at.elapsed(),
                );
            }
        }
    }

    tracing::info!(
        tenant = %tenant,
        entity_type = entity_type_filter.unwrap_or("*"),
        entity_limit = entity_limit.unwrap_or(usize::MAX),
        checked = report.checked,
        matched = report.matched,
        drifted = report.drifted,
        missing = report.missing,
        deleted_absent = report.deleted_absent,
        errors = report.errors,
        "query projection replay parity verification complete"
    );
    let result = if report.errors > 0 {
        "error"
    } else if report.drifted > 0 || report.missing > 0 {
        "drift"
    } else {
        "clean"
    };
    record_replay_parity_run_summary(
        tenant,
        entity_type_filter,
        source,
        result,
        report.checked,
        report.drifted,
        report.missing,
        report.errors,
        run_started_at.elapsed(),
    );
    Ok(report)
}
