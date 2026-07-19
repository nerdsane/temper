use libsql::params;

use super::{ledger_count, scalar_i64, schema_kind, temporary_connection};
use crate::migrations::catalog::{MIGRATIONS, Migration, MigrationStep};
use crate::migrations::runner::{migrate, migrate_catalog};
use crate::store::ots::PERSIST_OTS_TRAJECTORY_SQL;

const OTS_PROBE_SENTINEL_STEPS: &[MigrationStep] = &[MigrationStep::Sql(
    "CREATE TABLE ots_probe_migration_sentinel (id INTEGER PRIMARY KEY)",
)];

#[tokio::test]
async fn persisted_replacement_trigger_prevents_readiness_and_rolls_back_migration() {
    let (_directory, connection) = temporary_connection("ots-persist-replacement-trigger").await;
    migrate(&connection).await.expect("install current catalog");
    let trajectory_id = "existing-persisted-trajectory";
    connection
        .execute(
            PERSIST_OTS_TRAJECTORY_SQL,
            params![
                trajectory_id,
                "tenant-a",
                "agent-a",
                "session-before",
                "outcome-before",
                1_i64,
                "{\"stage\":\"before\"}",
            ],
        )
        .await
        .expect("seed an existing trajectory through production SQL");
    connection
        .execute(
            "CREATE TRIGGER reject_persisted_replacement
             BEFORE INSERT ON ots_trajectories
             WHEN NEW.persistence_status = 'persisted'
              AND EXISTS (
                  SELECT 1 FROM ots_trajectories
                  WHERE trajectory_id = NEW.trajectory_id
              )
             BEGIN SELECT RAISE(FAIL, 'persisted replacement blocked'); END",
            (),
        )
        .await
        .expect("create replacement-only OTS trigger");

    let runtime_error = connection
        .execute(
            PERSIST_OTS_TRAJECTORY_SQL,
            params![
                trajectory_id,
                "tenant-b",
                "agent-b",
                "session-after",
                "outcome-after",
                2_i64,
                "{\"stage\":\"after\"}",
            ],
        )
        .await
        .expect_err("the trigger must reproduce the production replacement failure");
    assert!(
        runtime_error
            .to_string()
            .contains("persisted replacement blocked"),
        "{runtime_error}"
    );

    let mut catalog = MIGRATIONS.to_vec();
    catalog.push(Migration {
        version: 8,
        name: "ots-probe-transaction-sentinel",
        steps: OTS_PROBE_SENTINEL_STEPS,
    });
    let error = migrate_catalog(&connection, &catalog)
        .await
        .expect_err("readiness must probe replacement through production persist SQL");
    let diagnostic = error.to_string();
    assert!(diagnostic.contains("migration 8"), "{diagnostic}");
    assert!(
        diagnostic.contains("reject_persisted_replacement"),
        "{diagnostic}"
    );
    assert!(
        diagnostic.contains("probe OTS persisted replacement"),
        "{diagnostic}"
    );
    assert_eq!(
        schema_kind(&connection, "ots_probe_migration_sentinel").await,
        None,
        "the active migration must roll back when the readiness probe fails"
    );
    assert_eq!(
        schema_kind(&connection, "reject_persisted_replacement")
            .await
            .as_deref(),
        Some("trigger")
    );
    assert_eq!(ledger_count(&connection).await, MIGRATIONS.len() as i64);
    assert_eq!(
        scalar_i64(
            &connection,
            "SELECT turn_count FROM ots_trajectories
             WHERE trajectory_id = 'existing-persisted-trajectory'",
        )
        .await,
        1,
        "the rejected runtime replacement and failed probe must preserve the row"
    );
}
