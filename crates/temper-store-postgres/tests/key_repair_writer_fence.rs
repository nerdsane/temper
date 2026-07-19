//! Regression for derived durable writers racing exact key-index repair.

use futures::future::{Either, select};
use temper_runtime::persistence::EventStore;
use temper_runtime::scheduler::sim_uuid;
use temper_store_postgres::PostgresEventStore;

async fn hold_stream_fence<'a>(
    pool: &'a sqlx::PgPool,
    persistence_id: &str,
) -> sqlx::Transaction<'a, sqlx::Postgres> {
    let mut transaction = pool.begin().await.expect("begin stream-fence holder");
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(persistence_id)
        .execute(&mut *transaction)
        .await
        .expect("hold exact-repair stream fence");
    transaction
}

#[test]
fn snapshot_writer_waits_for_exact_repair_stream_fence() {
    let database_url = match std::env::var("DATABASE_URL") {
        Ok(url) => url,
        Err(_) => return,
    };
    sqlx::test_block_on(async {
        let pool = sqlx::PgPool::connect(&database_url)
            .await
            .expect("connect to Postgres");
        temper_store_postgres::migration::run_migrations(&pool)
            .await
            .expect("run Postgres migrations");
        let store = PostgresEventStore::new(pool.clone());
        let tenant = format!("arn238-snapshot-fence-{}", sim_uuid());
        let persistence_id = format!("{tenant}:Doc:snapshot-only");
        let blocker = hold_stream_fence(&pool, &persistence_id).await;

        let write = Box::pin(store.save_snapshot(&persistence_id, 1, b"snapshot-only"));
        let deadline = Box::pin(sqlx::query("SELECT pg_sleep(1)").execute(&pool));
        let write = match select(write, deadline).await {
            Either::Left((result, _)) => panic!(
                "snapshot writer crossed the exact-repair stream fence before release: {result:?}"
            ),
            Either::Right((deadline, write)) => {
                deadline.expect("snapshot fence deadline query");
                write
            }
        };

        blocker.commit().await.expect("release stream fence");
        write
            .await
            .expect("snapshot writer resumes after fence release");
    });
}

#[test]
fn catalog_writer_waits_for_exact_repair_stream_fence() {
    let database_url = match std::env::var("DATABASE_URL") {
        Ok(url) => url,
        Err(_) => return,
    };
    sqlx::test_block_on(async {
        let pool = sqlx::PgPool::connect(&database_url)
            .await
            .expect("connect to Postgres");
        temper_store_postgres::migration::run_migrations(&pool)
            .await
            .expect("run Postgres migrations");
        let store = PostgresEventStore::new(pool.clone());
        let tenant = format!("arn238-catalog-fence-{}", sim_uuid());
        let persistence_id = format!("{tenant}:Doc:catalog-only");
        let blocker = hold_stream_fence(&pool, &persistence_id).await;

        let fields = serde_json::json!({"WorkspaceId": "ws", "Path": "/catalog-only"});
        let write = Box::pin(store.upsert_query_projection(
            &tenant,
            "Doc",
            "catalog-only",
            "Ready",
            &fields,
            1,
        ));
        let deadline = Box::pin(sqlx::query("SELECT pg_sleep(1)").execute(&pool));
        let write = match select(write, deadline).await {
            Either::Left((result, _)) => panic!(
                "catalog writer crossed the exact-repair stream fence before release: {result:?}"
            ),
            Either::Right((deadline, write)) => {
                deadline.expect("catalog fence deadline query");
                write
            }
        };

        blocker.commit().await.expect("release stream fence");
        write
            .await
            .expect("catalog writer resumes after fence release");
    });
}
