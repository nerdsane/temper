use libsql::Builder;

use super::runner::migrate;

const REMOTE_URL_ENV: &str = "TEMPER_HRANA_MIGRATION_TEST_URL";
const REMOTE_TOKEN_ENV: &str = "TEMPER_HRANA_MIGRATION_TEST_TOKEN";

#[tokio::test]
#[ignore = "requires an isolated Hrana endpoint via TEMPER_HRANA_MIGRATION_TEST_URL"]
async fn remote_final_ots_probe_is_atomic_and_side_effect_free() {
    let url = std::env::var(REMOTE_URL_ENV).expect("isolated Hrana test URL");
    let token = std::env::var(REMOTE_TOKEN_ENV).expect("isolated Hrana test token");
    assert!(url.starts_with("http"), "isolated Hrana endpoint URL");

    let initial_database = Builder::new_remote(url.clone(), token.clone())
        .build()
        .await
        .expect("build isolated remote database client");
    let setup = initial_database
        .connect()
        .expect("connect isolated remote database");
    migrate(&setup)
        .await
        .expect("migrate isolated remote database");
    setup
        .execute(
            "CREATE TABLE arn242_remote_probe_audit (
                trajectory_id TEXT NOT NULL
            )",
            (),
        )
        .await
        .expect("create remote probe audit table");
    setup
        .execute(
            "CREATE TRIGGER arn242_remote_probe_audit_trigger
             AFTER INSERT ON ots_trajectories
             WHEN NEW.trajectory_id GLOB '__temper_trigger_probe__-*'
             BEGIN
                 INSERT INTO arn242_remote_probe_audit (trajectory_id)
                 VALUES (NEW.trajectory_id);
             END",
            (),
        )
        .await
        .expect("create benign remote OTS audit trigger");

    let replay_database = Builder::new_remote(url, token)
        .build()
        .await
        .expect("build remote replay client");
    let replay = replay_database
        .connect()
        .expect("connect remote replay client");
    let reopened = migrate(&replay).await;
    let probe_rows = scalar_i64(
        &setup,
        "SELECT COUNT(*) FROM ots_trajectories
         WHERE trajectory_id GLOB '__temper_trigger_probe__-*'",
    )
    .await;
    let audit_rows = scalar_i64(&setup, "SELECT COUNT(*) FROM arn242_remote_probe_audit").await;
    let reopen_error = reopened.as_ref().err().map(ToString::to_string);

    assert!(
        reopened.is_ok() && probe_rows == 0 && audit_rows == 0,
        "remote head verification must succeed without durable probe effects: \
         reopen_error={reopen_error:?}, probe_rows={probe_rows}, audit_rows={audit_rows}"
    );
}

async fn scalar_i64(connection: &libsql::Connection, sql: &str) -> i64 {
    let mut rows = connection
        .query(sql, ())
        .await
        .expect("query remote scalar");
    rows.next()
        .await
        .expect("read remote scalar")
        .expect("remote scalar row")
        .get::<i64>(0)
        .expect("decode remote scalar")
}
