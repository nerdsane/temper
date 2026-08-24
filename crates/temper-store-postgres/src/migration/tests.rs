use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use super::{
    AppliedMigrationRow, FIXED_UPSTREAM_MIGRATOR, FORK_MIGRATOR, HISTORICAL_CONVERGENCE_MIGRATOR,
    LEGACY_UPSTREAM_MIGRATOR, MigrationLineage, SHARED_MIGRATOR, classify_migration_lineage,
    migration_at,
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

fn fixed_upstream_history(versions: std::ops::RangeInclusive<i64>) -> Vec<AppliedMigrationRow> {
    let mut history = Vec::new();
    for version in versions {
        let migrator = if version == 12 {
            &FORK_MIGRATOR
        } else {
            &FIXED_UPSTREAM_MIGRATOR
        };
        history.extend(applied(migrator, &[version]));
    }
    history
}

#[test]
fn embedded_migration_streams_preserve_every_published_boundary() {
    assert_eq!(
        FORK_MIGRATOR
            .iter()
            .map(|migration| migration.version)
            .collect::<Vec<_>>(),
        (1..=13).collect::<Vec<_>>()
    );
    assert_eq!(
        LEGACY_UPSTREAM_MIGRATOR
            .iter()
            .map(|migration| migration.version)
            .collect::<Vec<_>>(),
        vec![12, 13, 14, 15]
    );
    assert_eq!(
        FIXED_UPSTREAM_MIGRATOR
            .iter()
            .map(|migration| migration.version)
            .collect::<Vec<_>>(),
        vec![13, 14, 15, 16]
    );
    assert_eq!(
        HISTORICAL_CONVERGENCE_MIGRATOR
            .iter()
            .map(|migration| migration.version)
            .collect::<Vec<_>>(),
        vec![16]
    );
    assert_eq!(
        SHARED_MIGRATOR
            .iter()
            .map(|migration| migration.version)
            .collect::<Vec<_>>(),
        vec![17, 18]
    );
    assert_ne!(
        migration_at(&FORK_MIGRATOR, 12).unwrap().checksum,
        migration_at(&LEGACY_UPSTREAM_MIGRATOR, 12)
            .unwrap()
            .checksum,
        "legacy upstream must remain distinguishable at 0012"
    );
    assert_ne!(
        migration_at(&FORK_MIGRATOR, 13).unwrap().checksum,
        migration_at(&FIXED_UPSTREAM_MIGRATOR, 13).unwrap().checksum,
        "fixed upstream must remain distinguishable at 0013"
    );
    assert_ne!(
        migration_at(&HISTORICAL_CONVERGENCE_MIGRATOR, 16)
            .unwrap()
            .checksum,
        migration_at(&FIXED_UPSTREAM_MIGRATOR, 16).unwrap().checksum,
        "the two immutable 0016 identities must remain distinguishable"
    );
}

#[test]
fn every_active_migrator_has_unique_normalized_numeric_versions() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for relative in [
        "migrations",
        "migrations-upstream",
        "migrations-upstream-fixed",
        "migrations-convergence",
        "migrations-shared",
    ] {
        let mut versions = BTreeMap::new();
        for entry in fs::read_dir(root.join(relative)).expect("migration directory") {
            let name = entry
                .expect("migration directory entry")
                .file_name()
                .to_string_lossy()
                .into_owned();
            if !name.ends_with(".sql") {
                continue;
            }
            let prefix: String = name.chars().take_while(char::is_ascii_digit).collect();
            let version = prefix
                .parse::<i64>()
                .unwrap_or_else(|_| panic!("migration lacks numeric prefix: {relative}/{name}"));
            if let Some(previous) = versions.insert(version, name.clone()) {
                panic!(
                    "duplicate normalized migration version {version} in {relative}: \
                     {previous} and {name}"
                );
            }
        }
    }
}

#[test]
fn migration_lineage_classifies_complete_histories() {
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
    fork.extend(applied(&HISTORICAL_CONVERGENCE_MIGRATOR, &[16]));
    fork.extend(applied(&SHARED_MIGRATOR, &[17]));
    assert_eq!(
        classify_migration_lineage(&fork).unwrap(),
        MigrationLineage::Fork
    );

    let mut legacy_upstream = common_history();
    legacy_upstream.extend(applied(&LEGACY_UPSTREAM_MIGRATOR, &[12, 13, 14, 15]));
    legacy_upstream.extend(applied(&HISTORICAL_CONVERGENCE_MIGRATOR, &[16]));
    legacy_upstream.extend(applied(&SHARED_MIGRATOR, &[17]));
    assert_eq!(
        classify_migration_lineage(&legacy_upstream).unwrap(),
        MigrationLineage::LegacyUpstream
    );

    let mut fixed_upstream = common_history();
    fixed_upstream.extend(fixed_upstream_history(12..=16));
    fixed_upstream.extend(applied(&SHARED_MIGRATOR, &[17]));
    assert_eq!(
        classify_migration_lineage(&fixed_upstream).unwrap(),
        MigrationLineage::FixedUpstream
    );
}

#[test]
fn migration_lineage_accepts_every_distinguishable_interrupted_stream() {
    let mut ambiguous_twelve = common_history();
    ambiguous_twelve.extend(applied(&FORK_MIGRATOR, &[12]));
    assert_eq!(
        classify_migration_lineage(&ambiguous_twelve).unwrap(),
        MigrationLineage::Fork,
        "0012 is shared by fork and fixed upstream, so the canonical fork path resumes it"
    );

    let mut fork = ambiguous_twelve.clone();
    fork.extend(applied(&FORK_MIGRATOR, &[13]));
    assert_eq!(
        classify_migration_lineage(&fork).unwrap(),
        MigrationLineage::Fork
    );

    let mut legacy_upstream = common_history();
    legacy_upstream.extend(applied(&LEGACY_UPSTREAM_MIGRATOR, &[12, 13]));
    assert_eq!(
        classify_migration_lineage(&legacy_upstream).unwrap(),
        MigrationLineage::LegacyUpstream
    );

    let mut fixed_upstream = common_history();
    fixed_upstream.extend(fixed_upstream_history(12..=14));
    assert_eq!(
        classify_migration_lineage(&fixed_upstream).unwrap(),
        MigrationLineage::FixedUpstream
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
    mixed.extend(applied(&LEGACY_UPSTREAM_MIGRATOR, &[12]));
    mixed.extend(applied(&FORK_MIGRATOR, &[13]));
    assert!(
        classify_migration_lineage(&mixed)
            .unwrap_err()
            .contains("unexpected checksum")
    );

    let mut wrong_sixteen = common_history();
    wrong_sixteen.extend(applied(&FORK_MIGRATOR, &[12, 13]));
    wrong_sixteen.extend(applied(&FIXED_UPSTREAM_MIGRATOR, &[16]));
    assert!(
        classify_migration_lineage(&wrong_sixteen)
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
fn migration_lineage_rejects_shared_before_selected_stream_completion() {
    let mut history = common_history();
    history.extend(applied(&FORK_MIGRATOR, &[12]));
    history.extend(applied(&SHARED_MIGRATOR, &[17]));
    assert!(
        classify_migration_lineage(&history)
            .unwrap_err()
            .contains("stream is complete")
    );
}

#[test]
fn migration_lineage_requires_complete_common_history_before_divergence() {
    let legacy_twelve = applied(&LEGACY_UPSTREAM_MIGRATOR, &[12]);
    assert!(
        classify_migration_lineage(&legacy_twelve)
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
