use std::collections::BTreeMap;
use std::time::Instant;

use temper_runtime::tenant::TenantId;

use crate::entity_actor::recover_entity_state_from_store;
use crate::state::{
    QueryProjectionReplayParityDrift, QueryProjectionReplayParityReport, ServerState,
};
use crate::storage::EntityCatalogRow;

const MAX_REPLAY_PARITY_DRIFT_EXAMPLES: usize = 25;

struct ReplayParityExample<'a> {
    entity_type: &'a str,
    entity_id: &'a str,
    drift_kind: &'a str,
    sequence_direction: &'a str,
    sequence_gap: u64,
    catalog_sequence: Option<u64>,
    authoritative_sequence: u64,
}

fn replay_parity_drift_kind(
    catalog: &EntityCatalogRow,
    authoritative_status: &str,
    authoritative_fields: &serde_json::Value,
    authoritative_state: &serde_json::Value,
    authoritative_sequence: u64,
) -> &'static str {
    let status_drift = catalog.status != authoritative_status;
    let fields_drift = catalog.fields != *authoritative_fields;
    let state_drift = catalog
        .state
        .as_ref()
        .is_some_and(|catalog_state| catalog_state != authoritative_state);
    let sequence_drift = catalog.sequence_nr != authoritative_sequence;
    match (status_drift, fields_drift, state_drift, sequence_drift) {
        (false, false, false, false) => "none",
        (true, false, false, false) => "status",
        (false, true, false, false) => "fields",
        (false, false, true, false) => "state",
        (false, false, false, true) => "sequence",
        _ => "multiple",
    }
}

fn replay_parity_sequence_gap(
    catalog_sequence: Option<u64>,
    authoritative_sequence: u64,
) -> (&'static str, u64) {
    let Some(catalog_sequence) = catalog_sequence else {
        return ("catalog_missing", authoritative_sequence);
    };
    match catalog_sequence.cmp(&authoritative_sequence) {
        std::cmp::Ordering::Less => ("catalog_behind", authoritative_sequence - catalog_sequence),
        std::cmp::Ordering::Equal => ("equal", 0),
        std::cmp::Ordering::Greater => ("catalog_ahead", catalog_sequence - authoritative_sequence),
    }
}

fn push_replay_parity_example(
    report: &mut QueryProjectionReplayParityReport,
    example: ReplayParityExample<'_>,
) {
    if report.drift_examples.len() >= MAX_REPLAY_PARITY_DRIFT_EXAMPLES {
        return;
    }
    report
        .drift_examples
        .push(QueryProjectionReplayParityDrift {
            entity_type: example.entity_type.to_string(),
            entity_id: example.entity_id.to_string(),
            drift_kind: example.drift_kind.to_string(),
            sequence_direction: example.sequence_direction.to_string(),
            sequence_gap: example.sequence_gap,
            catalog_sequence: example.catalog_sequence,
            authoritative_sequence: example.authoritative_sequence,
        });
}

pub(in crate::state) async fn verify_query_projection_replay_parity(
    state: &ServerState,
    tenant: &TenantId,
) -> Result<QueryProjectionReplayParityReport, String> {
    let Some((store, backend)) = state.event_journal() else {
        return Err("event journal is not configured".to_string());
    };
    let Some(query_plane) = state.query_plane_store() else {
        return Err("query-plane store is not configured".to_string());
    };

    let entities = store
        .list_entity_ids(tenant.as_str())
        .await
        .map_err(|error| format!("list persisted entity ids failed: {error}"))?;
    let mut by_type = BTreeMap::<String, Vec<String>>::new();
    for (entity_type, entity_id) in entities {
        by_type.entry(entity_type).or_default().push(entity_id);
    }

    let mut report = QueryProjectionReplayParityReport {
        tenant: tenant.as_str().to_string(),
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

        let catalog_rows = match query_plane
            .load_entity_catalog_rows(tenant.as_str(), &entity_type, &entity_ids)
            .await
        {
            Ok(Some(rows)) => rows
                .into_iter()
                .map(|row| (row.entity_id.clone(), row))
                .collect::<BTreeMap<_, _>>(),
            Ok(None) => {
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
            let replayed = recover_entity_state_from_store(
                tenant.as_str(),
                &entity_type,
                &entity_id,
                &table,
                &store,
                backend,
                &serde_json::json!({}),
                tenant_blob_store.as_ref(),
            )
            .await;
            let catalog = catalog_rows.get(&entity_id);

            if replayed.status == "Deleted" {
                if let Some(catalog) = catalog {
                    report.drifted += 1;
                    let (sequence_direction, sequence_gap) =
                        replay_parity_sequence_gap(Some(catalog.sequence_nr), replayed.sequence_nr);
                    push_replay_parity_example(
                        &mut report,
                        ReplayParityExample {
                            entity_type: &entity_type,
                            entity_id: &entity_id,
                            drift_kind: "deleted_present",
                            sequence_direction,
                            sequence_gap,
                            catalog_sequence: Some(catalog.sequence_nr),
                            authoritative_sequence: replayed.sequence_nr,
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
                    replay_parity_sequence_gap(None, replayed.sequence_nr);
                push_replay_parity_example(
                    &mut report,
                    ReplayParityExample {
                        entity_type: &entity_type,
                        entity_id: &entity_id,
                        drift_kind: "missing_catalog",
                        sequence_direction,
                        sequence_gap,
                        catalog_sequence: None,
                        authoritative_sequence: replayed.sequence_nr,
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
                replayed.sequence_nr,
            );
            let (sequence_direction, sequence_gap) =
                replay_parity_sequence_gap(Some(catalog.sequence_nr), replayed.sequence_nr);
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
                        authoritative_sequence: replayed.sequence_nr,
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
        checked = report.checked,
        matched = report.matched,
        drifted = report.drifted,
        missing = report.missing,
        deleted_absent = report.deleted_absent,
        errors = report.errors,
        "query projection replay parity verification complete"
    );
    Ok(report)
}
