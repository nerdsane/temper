use super::{
    AppliedMigrationRow, CONVERGENCE_MIGRATOR, FORK_MIGRATOR, MigrationLineage, UPSTREAM_MIGRATOR,
    classify_migration_lineage, migration_at,
};

fn applied(
    migrator: &'static sqlx::migrate::Migrator,
    versions: &[i64],
) -> Vec<AppliedMigrationRow> {
    versions
        .iter()
        .map(|version| {
            let migration = migration_at(migrator, *version).expect("fixture migration");
            AppliedMigrationRow {
                version: *version,
                checksum: migration.checksum.to_vec(),
                success: true,
            }
        })
        .collect()
}

fn common_history() -> Vec<AppliedMigrationRow> {
    applied(&FORK_MIGRATOR, &(1..=11).collect::<Vec<_>>())
}

#[test]
fn embedded_migration_streams_keep_reserved_versions_and_distinct_lineages() {
    assert_eq!(
        FORK_MIGRATOR
            .iter()
            .map(|migration| migration.version)
            .collect::<Vec<_>>(),
        (1..=13).collect::<Vec<_>>()
    );
    assert_eq!(
        UPSTREAM_MIGRATOR
            .iter()
            .map(|migration| migration.version)
            .collect::<Vec<_>>(),
        vec![12, 13, 14, 15]
    );
    assert_eq!(
        CONVERGENCE_MIGRATOR
            .iter()
            .map(|migration| migration.version)
            .collect::<Vec<_>>(),
        vec![16]
    );
    assert_ne!(
        migration_at(&FORK_MIGRATOR, 12).unwrap().checksum,
        migration_at(&UPSTREAM_MIGRATOR, 12).unwrap().checksum,
        "the lineage classifier requires distinct divergent checksums"
    );
}

#[test]
fn migration_lineage_classifies_fresh_fork_and_upstream_histories() {
    assert_eq!(
        classify_migration_lineage(&[]).unwrap(),
        MigrationLineage::Fork
    );
    assert_eq!(
        classify_migration_lineage(&common_history()).unwrap(),
        MigrationLineage::Fork
    );

    let mut fork = common_history();
    fork.extend(applied(&FORK_MIGRATOR, &[12, 13]));
    fork.extend(applied(&CONVERGENCE_MIGRATOR, &[16]));
    assert_eq!(
        classify_migration_lineage(&fork).unwrap(),
        MigrationLineage::Fork
    );

    let mut upstream = common_history();
    upstream.extend(applied(&UPSTREAM_MIGRATOR, &[12, 13, 14, 15]));
    upstream.extend(applied(&CONVERGENCE_MIGRATOR, &[16]));
    assert_eq!(
        classify_migration_lineage(&upstream).unwrap(),
        MigrationLineage::Upstream
    );
}

#[test]
fn migration_lineage_accepts_interrupted_legacy_prefixes() {
    let mut fork = common_history();
    fork.extend(applied(&FORK_MIGRATOR, &[12]));
    assert_eq!(
        classify_migration_lineage(&fork).unwrap(),
        MigrationLineage::Fork
    );

    let mut upstream = common_history();
    upstream.extend(applied(&UPSTREAM_MIGRATOR, &[12, 13]));
    assert_eq!(
        classify_migration_lineage(&upstream).unwrap(),
        MigrationLineage::Upstream
    );
}

#[test]
fn migration_lineage_rejects_unknown_mixed_gapped_and_failed_histories() {
    let mut unknown = common_history();
    unknown.push(AppliedMigrationRow {
        version: 12,
        checksum: vec![0; 48],
        success: true,
    });
    assert!(
        classify_migration_lineage(&unknown)
            .unwrap_err()
            .contains("unknown lineage checksum")
    );

    let mut mixed = common_history();
    mixed.extend(applied(&FORK_MIGRATOR, &[12]));
    mixed.extend(applied(&UPSTREAM_MIGRATOR, &[13]));
    assert!(
        classify_migration_lineage(&mixed)
            .unwrap_err()
            .contains("unexpected checksum")
    );

    let mut gapped = applied(&FORK_MIGRATOR, &[1, 3]);
    assert!(
        classify_migration_lineage(&gapped)
            .unwrap_err()
            .contains("gap at version 2")
    );
    gapped[0].success = false;
    assert!(
        classify_migration_lineage(&gapped)
            .unwrap_err()
            .contains("failed version 1")
    );
}

#[test]
fn migration_lineage_rejects_convergence_before_legacy_completion() {
    let mut history = common_history();
    history.extend(applied(&FORK_MIGRATOR, &[12]));
    history.extend(applied(&CONVERGENCE_MIGRATOR, &[16]));
    assert!(
        classify_migration_lineage(&history)
            .unwrap_err()
            .contains("legacy stream is complete")
    );
}

#[test]
fn migration_lineage_requires_complete_common_history_before_divergence() {
    let upstream_twelve = applied(&UPSTREAM_MIGRATOR, &[12]);
    assert!(
        classify_migration_lineage(&upstream_twelve)
            .unwrap_err()
            .contains("before the common stream is complete")
    );

    let mut partial_common = applied(&FORK_MIGRATOR, &[1, 2, 3, 4, 5]);
    partial_common.extend(applied(&FORK_MIGRATOR, &[12]));
    assert!(
        classify_migration_lineage(&partial_common)
            .unwrap_err()
            .contains("before the common stream is complete")
    );
}

#[path = "tests/postgres.rs"]
mod postgres;

#[path = "tests/schema.rs"]
mod schema;
