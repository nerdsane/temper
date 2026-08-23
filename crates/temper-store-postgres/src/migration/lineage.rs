use std::collections::BTreeMap;

use sqlx::migrate::{Migration, Migrator};

use super::{
    FIXED_UPSTREAM_MIGRATOR, FORK_MIGRATOR, HISTORICAL_CONVERGENCE_MIGRATOR,
    LEGACY_UPSTREAM_MIGRATOR, SHARED_MIGRATOR,
};

const COMMON_LAST_VERSION: i64 = 11;
const FIRST_DIVERGENT_VERSION: i64 = 12;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum MigrationLineage {
    Fork,
    LegacyUpstream,
    FixedUpstream,
}

#[derive(Clone, Debug)]
pub(super) struct AppliedMigrationRow {
    pub(super) version: i64,
    pub(super) checksum: Vec<u8>,
    pub(super) success: bool,
}

pub(super) fn classify_migration_lineage(
    applied: &[AppliedMigrationRow],
) -> Result<MigrationLineage, String> {
    let applied_by_version: BTreeMap<_, _> = applied
        .iter()
        .map(|migration| (migration.version, migration))
        .collect();
    if applied_by_version.len() != applied.len() {
        return Err("Postgres migration history contains duplicate versions".to_string());
    }
    if let Some(failed) = applied.iter().find(|migration| !migration.success) {
        return Err(format!(
            "Postgres migration history contains failed version {}",
            failed.version
        ));
    }

    let common = migrations_in_range(&FORK_MIGRATOR, 1, COMMON_LAST_VERSION);
    validate_prefix(&applied_by_version, &common, "common")?;
    if applied
        .iter()
        .any(|migration| migration.version > COMMON_LAST_VERSION)
        && !stream_is_complete(&applied_by_version, &common)
    {
        return Err(
            "Postgres divergent migration history exists before the common stream is complete"
                .to_string(),
        );
    }

    let lineage = classify_divergent_stream(&applied_by_version)?;
    let pre_shared = pre_shared_migrations(lineage)?;
    let lineage_name = lineage_name(lineage);
    validate_prefix(&applied_by_version, &pre_shared, lineage_name)?;

    let shared = migrations_in_range(&SHARED_MIGRATOR, 17, i64::MAX);
    validate_prefix(&applied_by_version, &shared, "shared")?;
    if shared
        .iter()
        .any(|migration| applied_by_version.contains_key(&migration.version))
        && !stream_is_complete(&applied_by_version, &pre_shared)
    {
        return Err(format!(
            "Postgres shared migration history exists before the {lineage_name} stream is complete"
        ));
    }

    for applied_migration in applied {
        let known = common
            .iter()
            .chain(pre_shared.iter())
            .chain(shared.iter())
            .any(|migration| migration.version == applied_migration.version);
        if !known {
            return Err(format!(
                "Postgres migration history contains unknown version {}",
                applied_migration.version
            ));
        }
    }

    Ok(lineage)
}

fn classify_divergent_stream(
    applied: &BTreeMap<i64, &AppliedMigrationRow>,
) -> Result<MigrationLineage, String> {
    let Some(version_twelve) = applied.get(&FIRST_DIVERGENT_VERSION) else {
        return Ok(MigrationLineage::Fork);
    };

    if checksum_matches(
        version_twelve,
        migration_at(&LEGACY_UPSTREAM_MIGRATOR, FIRST_DIVERGENT_VERSION)?,
    ) {
        return Ok(MigrationLineage::LegacyUpstream);
    }
    if !checksum_matches(
        version_twelve,
        migration_at(&FORK_MIGRATOR, FIRST_DIVERGENT_VERSION)?,
    ) {
        return Err(format!(
            "Postgres migration version {FIRST_DIVERGENT_VERSION} has an unknown lineage checksum"
        ));
    }

    let Some(version_thirteen) = applied.get(&(FIRST_DIVERGENT_VERSION + 1)) else {
        // Fork and corrected upstream share 0012. Until 0013 records a
        // distinct identity, resume on the canonical fork stream; the shared
        // migration produces the same final schema either way.
        return Ok(MigrationLineage::Fork);
    };
    if checksum_matches(
        version_thirteen,
        migration_at(&FORK_MIGRATOR, FIRST_DIVERGENT_VERSION + 1)?,
    ) {
        return Ok(MigrationLineage::Fork);
    }
    if checksum_matches(
        version_thirteen,
        migration_at(&FIXED_UPSTREAM_MIGRATOR, FIRST_DIVERGENT_VERSION + 1)?,
    ) {
        return Ok(MigrationLineage::FixedUpstream);
    }
    Err(format!(
        "Postgres migration version {} has an unknown lineage checksum",
        FIRST_DIVERGENT_VERSION + 1
    ))
}

fn pre_shared_migrations(lineage: MigrationLineage) -> Result<Vec<&'static Migration>, String> {
    let mut migrations = match lineage {
        MigrationLineage::Fork => migrations_in_range(&FORK_MIGRATOR, 12, 13),
        MigrationLineage::LegacyUpstream => migrations_in_range(&LEGACY_UPSTREAM_MIGRATOR, 12, 15),
        MigrationLineage::FixedUpstream => {
            let mut fixed = vec![migration_at(&FORK_MIGRATOR, 12)?];
            fixed.extend(migrations_in_range(&FIXED_UPSTREAM_MIGRATOR, 13, 16));
            fixed
        }
    };
    if lineage != MigrationLineage::FixedUpstream {
        migrations.extend(migrations_in_range(
            &HISTORICAL_CONVERGENCE_MIGRATOR,
            16,
            16,
        ));
    }
    Ok(migrations)
}

fn lineage_name(lineage: MigrationLineage) -> &'static str {
    match lineage {
        MigrationLineage::Fork => "fork",
        MigrationLineage::LegacyUpstream => "legacy upstream",
        MigrationLineage::FixedUpstream => "fixed upstream",
    }
}

fn validate_prefix(
    applied: &BTreeMap<i64, &AppliedMigrationRow>,
    expected: &[&Migration],
    stream_name: &str,
) -> Result<(), String> {
    let highest_applied_index = expected
        .iter()
        .rposition(|migration| applied.contains_key(&migration.version));
    let Some(highest_applied_index) = highest_applied_index else {
        return Ok(());
    };

    for migration in &expected[..=highest_applied_index] {
        let Some(applied_migration) = applied.get(&migration.version) else {
            return Err(format!(
                "Postgres {stream_name} migration history has a gap at version {}",
                migration.version
            ));
        };
        if !checksum_matches(applied_migration, migration) {
            return Err(format!(
                "Postgres {stream_name} migration version {} has an unexpected checksum",
                migration.version
            ));
        }
    }
    Ok(())
}

fn stream_is_complete(
    applied: &BTreeMap<i64, &AppliedMigrationRow>,
    expected: &[&Migration],
) -> bool {
    expected
        .iter()
        .all(|migration| applied.contains_key(&migration.version))
}

fn migrations_in_range(
    migrator: &'static Migrator,
    first: i64,
    last: i64,
) -> Vec<&'static Migration> {
    migrator
        .iter()
        .filter(|migration| (first..=last).contains(&migration.version))
        .collect()
}

pub(super) fn migration_at(
    migrator: &'static Migrator,
    version: i64,
) -> Result<&'static Migration, String> {
    migrator
        .iter()
        .find(|migration| migration.version == version)
        .ok_or_else(|| format!("embedded migration stream is missing version {version}"))
}

fn checksum_matches(applied: &AppliedMigrationRow, expected: &Migration) -> bool {
    applied.checksum.as_slice() == expected.checksum.as_ref()
}
