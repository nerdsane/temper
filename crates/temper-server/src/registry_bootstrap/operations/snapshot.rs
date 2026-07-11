//! Shared durable-quarantine snapshot projections.

use super::*;

type QuarantineSnapshotRow = (
    String,
    String,
    i64,
    Option<i64>,
    String,
    String,
    Option<i64>,
    Option<i64>,
    String,
);

pub(super) fn quarantine_upserts(
    report: &RegistryRestoreHealth,
) -> Vec<RegistryQuarantineUpsert<'_>> {
    report
        .quarantined_tenants
        .iter()
        .flat_map(|(tenant, quarantine)| {
            quarantine
                .entity_failures
                .iter()
                .map(move |(entity_type, failure)| RegistryQuarantineUpsert {
                    tenant,
                    entity_type,
                    spec_version: failure.spec_version,
                    constraint_version: failure.constraint_version,
                    reason: failure.reason.as_str(),
                    source_kind: failure.source_kind.as_str(),
                    source_line: failure.source_line,
                    source_column: failure.source_column,
                    detail: &failure.detail,
                })
        })
        .collect()
}

pub(super) fn report_snapshot(report: &RegistryRestoreHealth) -> BTreeSet<QuarantineSnapshotRow> {
    report
        .quarantined_tenants
        .iter()
        .flat_map(|(tenant, entry)| {
            entry
                .entity_failures
                .iter()
                .map(move |(entity_type, failure)| {
                    (
                        tenant.clone(),
                        entity_type.clone(),
                        failure.spec_version,
                        failure.constraint_version,
                        failure.reason.as_str().to_string(),
                        failure.source_kind.as_str().to_string(),
                        failure.source_line,
                        failure.source_column,
                        failure.detail.clone(),
                    )
                })
        })
        .collect()
}

pub(super) fn durable_snapshot(
    records: &[RegistryQuarantineRecord],
) -> BTreeSet<QuarantineSnapshotRow> {
    records
        .iter()
        .map(|record| {
            (
                record.tenant.clone(),
                record.entity_type.clone(),
                record.spec_version,
                record.constraint_version,
                record.reason.clone(),
                record.source_kind.clone(),
                record.source_line,
                record.source_column,
                record.detail.clone(),
            )
        })
        .collect()
}

pub(super) fn apply_durable_acknowledgments(
    report: &mut RegistryRestoreHealth,
    records: &[RegistryQuarantineRecord],
) {
    for record in records
        .iter()
        .filter(|record| record.acknowledged_at.is_some())
    {
        let Some(failure) = report
            .quarantined_tenants
            .get_mut(&record.tenant)
            .and_then(|quarantine| quarantine.entity_failures.get_mut(&record.entity_type))
        else {
            continue;
        };
        if failure.spec_version == record.spec_version
            && failure.constraint_version == record.constraint_version
        {
            failure.acknowledged = true;
        }
    }
}
