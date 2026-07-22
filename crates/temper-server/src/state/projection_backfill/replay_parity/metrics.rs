use std::time::Duration;

use temper_runtime::tenant::TenantId;

use crate::state::{QueryProjectionReplayParityDrift, QueryProjectionReplayParityReport};
use crate::storage::EntityCatalogRow;

const MAX_REPLAY_PARITY_DRIFT_EXAMPLES: usize = 25;

pub(super) struct ReplayParityExample<'a> {
    pub(super) entity_type: &'a str,
    pub(super) entity_id: &'a str,
    pub(super) drift_kind: &'a str,
    pub(super) sequence_direction: &'a str,
    pub(super) sequence_gap: u64,
    pub(super) catalog_sequence: Option<u64>,
    pub(super) authoritative_sequence: u64,
}

pub(super) fn replay_parity_drift_kind(
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

pub(super) fn replay_parity_sequence_gap(
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

pub(super) fn push_replay_parity_example(
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

#[expect(
    clippy::too_many_arguments,
    reason = "run-level projection parity metric mirrors the metric dimensions and counters"
)]
pub(super) fn record_replay_parity_run_summary(
    tenant: &TenantId,
    entity_type_filter: Option<&str>,
    source: &str,
    result: &str,
    checked: u64,
    drifted: u64,
    missing: u64,
    errors: u64,
    duration: Duration,
) {
    crate::query_projection_metrics::record_replay_parity_run(
        tenant.as_str(),
        entity_type_filter.unwrap_or("*"),
        source,
        result,
        checked,
        drifted,
        missing,
        errors,
        duration,
    );
}
