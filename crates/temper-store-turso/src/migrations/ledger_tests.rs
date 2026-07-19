use libsql::Builder;

use super::catalog::MIGRATIONS;
use super::runner::migrate;

#[tokio::test]
async fn compact_equivalent_ledger_definition_is_accepted() {
    let directory = tempfile::tempdir().expect("temporary database directory");
    let database = Builder::new_local(directory.path().join("compact-ledger.db"))
        .build()
        .await
        .expect("build compact-ledger database");
    let connection = database.connect().expect("connect compact-ledger database");
    connection
        .execute(
            "CREATE TABLE temper_schema_migrations(version INTEGER PRIMARY KEY CHECK(version>0),name TEXT NOT NULL UNIQUE,checksum TEXT NOT NULL CHECK(length(checksum)=64),applied_at TEXT NOT NULL DEFAULT(datetime('now')))",
            (),
        )
        .await
        .expect("create semantically identical compact ledger");

    migrate(&connection)
        .await
        .expect("formatting must not make an identical ledger incompatible");

    let mut rows = connection
        .query("SELECT COUNT(*) FROM temper_schema_migrations", ())
        .await
        .expect("query ledger count");
    let count = rows
        .next()
        .await
        .expect("read ledger count")
        .expect("ledger count row")
        .get::<i64>(0)
        .expect("decode ledger count");
    assert_eq!(count, MIGRATIONS.len() as i64);
}

#[tokio::test]
async fn merged_constraint_tokens_do_not_satisfy_ledger_contract() {
    let directory = tempfile::tempdir().expect("temporary database directory");
    let database = Builder::new_local(directory.path().join("weak-ledger.db"))
        .build()
        .await
        .expect("build weak-ledger database");
    let connection = database.connect().expect("connect weak-ledger database");
    connection
        .execute(
            "CREATE TABLE temper_schema_migrations(
                version INTEGER PRIMARYKEY CHECK(version>0),
                name TEXTNOT NULL UNIQUE,
                checksum TEXTNOT NULL CHECK(length(checksum)=64),
                applied_at TEXTNOT NULL DEFAULT(datetime('now'))
            )",
            (),
        )
        .await
        .expect("create structurally weaker ledger");

    let error = migrate(&connection)
        .await
        .expect_err("merged SQL words must not satisfy separate constraint tokens");
    assert!(error.to_string().contains("incompatible schema"), "{error}");
    let mut columns = connection
        .query("PRAGMA table_xinfo(temper_schema_migrations)", ())
        .await
        .expect("inspect weak ledger columns");
    while let Some(row) = columns.next().await.expect("read weak ledger column") {
        assert_eq!(row.get::<i64>(3).expect("not-null flag"), 0);
        assert_eq!(row.get::<i64>(5).expect("primary-key position"), 0);
    }
}

#[tokio::test]
async fn ignored_ledger_insert_prevents_schema_commit_and_readiness() {
    let directory = tempfile::tempdir().expect("temporary database directory");
    let database = Builder::new_local(directory.path().join("ignored-ledger-insert.db"))
        .build()
        .await
        .expect("build ignored-ledger-insert database");
    let connection = database
        .connect()
        .expect("connect ignored-ledger-insert database");
    connection
        .execute(
            "CREATE TABLE temper_schema_migrations(
                version INTEGER PRIMARY KEY CHECK(version>0),
                name TEXT NOT NULL UNIQUE,
                checksum TEXT NOT NULL CHECK(length(checksum)=64),
                applied_at TEXT NOT NULL DEFAULT(datetime('now'))
            )",
            (),
        )
        .await
        .expect("create exact ledger");
    connection
        .execute(
            "CREATE TRIGGER ignore_migration_ledger_insert
             BEFORE INSERT ON temper_schema_migrations
             BEGIN SELECT RAISE(IGNORE); END",
            (),
        )
        .await
        .expect("create ledger insert trigger");

    let error = migrate(&connection)
        .await
        .expect_err("a migration without a retained ledger row must not reach readiness");
    assert!(error.to_string().contains("migration ledger"), "{error}");
    assert_eq!(
        scalar_i64(&connection, "SELECT COUNT(*) FROM temper_schema_migrations").await,
        0
    );
    assert_eq!(
        scalar_i64(
            &connection,
            "SELECT COUNT(*) FROM sqlite_schema WHERE type = 'table' AND name = 'events'"
        )
        .await,
        0
    );
}

#[tokio::test]
async fn differently_cased_ledger_trigger_owner_is_rejected_before_migration() {
    let directory = tempfile::tempdir().expect("temporary database directory");
    let database = Builder::new_local(directory.path().join("case-folded-ledger-trigger.db"))
        .build()
        .await
        .expect("build case-folded-ledger-trigger database");
    let connection = database
        .connect()
        .expect("connect case-folded-ledger-trigger database");
    connection
        .execute(super::ledger::CREATE_MIGRATION_LEDGER, ())
        .await
        .expect("create exact migration ledger");
    connection
        .execute(
            "CREATE TRIGGER case_folded_ledger_audit
             AFTER INSERT ON TEMPER_SCHEMA_MIGRATIONS
             BEGIN SELECT 1; END",
            (),
        )
        .await
        .expect("create benign trigger with differently cased ledger owner");

    let error = migrate(&connection)
        .await
        .expect_err("every trigger on the migration ledger must be rejected");
    let diagnostic = error.to_string();
    assert!(diagnostic.contains("unsupported trigger"), "{diagnostic}");
    assert!(
        diagnostic.contains("case_folded_ledger_audit"),
        "{diagnostic}"
    );
    assert_eq!(
        scalar_i64(&connection, "SELECT COUNT(*) FROM temper_schema_migrations").await,
        0
    );
    assert_eq!(
        scalar_i64(
            &connection,
            "SELECT COUNT(*) FROM sqlite_schema WHERE type = 'table' AND name = 'events'"
        )
        .await,
        0
    );
}

async fn scalar_i64(connection: &libsql::Connection, sql: &str) -> i64 {
    let mut rows = connection
        .query(sql, ())
        .await
        .expect("query integer scalar");
    rows.next()
        .await
        .expect("read integer scalar")
        .expect("integer scalar row")
        .get::<i64>(0)
        .expect("decode integer scalar")
}
