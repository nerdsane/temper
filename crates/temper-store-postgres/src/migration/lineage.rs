use std::collections::BTreeMap;

use sqlx::migrate::{Migration, Migrator};

use super::{CONVERGENCE_MIGRATOR, FORK_MIGRATOR, UPSTREAM_MIGRATOR};

const COMMON_LAST_VERSION: i64 = 11;
const FORK_LAST_VERSION: i64 = 13;
const UPSTREAM_LAST_VERSION: i64 = 15;
const CONVERGENCE_FIRST_VERSION: i64 = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum MigrationLineage {
    Fork,
    Upstream,
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

    validate_prefix(
        &applied_by_version,
        migrations_in_range(&FORK_MIGRATOR, 1, COMMON_LAST_VERSION),
        "common",
    )?;
    if applied
        .iter()
        .any(|migration| migration.version > COMMON_LAST_VERSION)
        && !legacy_is_complete(&applied_by_version, &FORK_MIGRATOR, COMMON_LAST_VERSION)
    {
        return Err(
            "Postgres divergent migration history exists before the common stream is complete"
                .to_string(),
        );
    }

    let lineage = match applied_by_version.get(&(COMMON_LAST_VERSION + 1)) {
        None => MigrationLineage::Fork,
        Some(migration)
            if checksum_matches(
                migration,
                migration_at(&FORK_MIGRATOR, COMMON_LAST_VERSION + 1)?,
            ) =>
        {
            MigrationLineage::Fork
        }
        Some(migration)
            if checksum_matches(
                migration,
                migration_at(&UPSTREAM_MIGRATOR, COMMON_LAST_VERSION + 1)?,
            ) =>
        {
            MigrationLineage::Upstream
        }
        Some(_) => {
            return Err(format!(
                "Postgres migration version {} has an unknown lineage checksum",
                COMMON_LAST_VERSION + 1
            ));
        }
    };

    let (legacy_migrator, legacy_last, lineage_name) = match lineage {
        MigrationLineage::Fork => (&FORK_MIGRATOR, FORK_LAST_VERSION, "fork"),
        MigrationLineage::Upstream => (&UPSTREAM_MIGRATOR, UPSTREAM_LAST_VERSION, "upstream"),
    };
    validate_prefix(
        &applied_by_version,
        migrations_in_range(legacy_migrator, COMMON_LAST_VERSION + 1, legacy_last),
        lineage_name,
    )?;

    for version in (COMMON_LAST_VERSION + 1)..=UPSTREAM_LAST_VERSION {
        if version > legacy_last && applied_by_version.contains_key(&version) {
            return Err(format!(
                "Postgres migration history mixes {lineage_name} lineage with version {version}"
            ));
        }
    }

    let convergence =
        migrations_in_range(&CONVERGENCE_MIGRATOR, CONVERGENCE_FIRST_VERSION, i64::MAX);
    validate_prefix(&applied_by_version, convergence.clone(), "convergence")?;
    if convergence
        .iter()
        .any(|migration| applied_by_version.contains_key(&migration.version))
        && !legacy_is_complete(&applied_by_version, legacy_migrator, legacy_last)
    {
        return Err(format!(
            "Postgres convergence history exists before the {lineage_name} legacy stream is complete"
        ));
    }

    for applied_migration in applied {
        let known = (1..=COMMON_LAST_VERSION).contains(&applied_migration.version)
            || ((COMMON_LAST_VERSION + 1)..=legacy_last).contains(&applied_migration.version)
            || convergence
                .iter()
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

fn validate_prefix(
    applied: &BTreeMap<i64, &AppliedMigrationRow>,
    expected: Vec<&Migration>,
    stream_name: &str,
) -> Result<(), String> {
    let highest_applied = expected
        .iter()
        .filter(|migration| applied.contains_key(&migration.version))
        .map(|migration| migration.version)
        .max();
    let Some(highest_applied) = highest_applied else {
        return Ok(());
    };

    for migration in expected
        .into_iter()
        .take_while(|migration| migration.version <= highest_applied)
    {
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

fn legacy_is_complete(
    applied: &BTreeMap<i64, &AppliedMigrationRow>,
    migrator: &'static Migrator,
    last_version: i64,
) -> bool {
    migrations_in_range(migrator, 1, last_version)
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
