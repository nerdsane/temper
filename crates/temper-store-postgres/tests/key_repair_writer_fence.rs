//! Regression for derived durable writers racing exact key-index repair.

use futures::future::{Either, select};
use temper_runtime::persistence::{
    EntityKeyRow, EventMetadata, EventStore, IndexReconciliation, KeyIndexBackfillFence,
    PersistenceEnvelope, PersistenceError,
};
use temper_runtime::scheduler::{sim_now, sim_uuid};
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

#[test]
fn snapshot_only_writer_invalidates_inflight_key_coverage() {
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
        let store = PostgresEventStore::new(pool);
        let tenant = format!("arn238-snapshot-coverage-{}", sim_uuid());
        let signature = "v4:path";
        let revision = store
            .begin_key_index_backfill(&tenant, "Doc", signature)
            .await
            .expect("begin key repair");

        store
            .save_snapshot(&format!("{tenant}:Doc:snapshot-only"), 1, b"snapshot-only")
            .await
            .expect("write newly enumerated snapshot owner");

        assert!(
            !store
                .mark_key_index_backfilled_if_revision(&tenant, "Doc", signature, revision)
                .await
                .expect("conditionally publish coverage"),
            "a snapshot-only owner created after enumeration must reject publication"
        );
    });
}

#[test]
fn catalog_only_writer_invalidates_inflight_key_coverage() {
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
        let store = PostgresEventStore::new(pool);
        let tenant = format!("arn238-catalog-coverage-{}", sim_uuid());
        let signature = "v4:path";
        let revision = store
            .begin_key_index_backfill(&tenant, "Doc", signature)
            .await
            .expect("begin key repair");

        store
            .upsert_query_projection(
                &tenant,
                "Doc",
                "catalog-only",
                "Ready",
                &serde_json::json!({"WorkspaceId": "ws", "Path": "/catalog-only"}),
                1,
            )
            .await
            .expect("write newly enumerated catalog owner");

        assert!(
            !store
                .mark_key_index_backfilled_if_revision(&tenant, "Doc", signature, revision)
                .await
                .expect("conditionally publish coverage"),
            "a catalog-only owner created after enumeration must reject publication"
        );
    });
}

#[test]
fn journal_source_fence_rejects_equal_sequence_snapshot_repair() {
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
        let store = PostgresEventStore::new(pool);
        let tenant = format!("arn238-source-fence-{}", sim_uuid());
        let entity_type = "Doc";
        let entity_id = "source-aba";
        let persistence_id = format!("{tenant}:{entity_type}:{entity_id}");
        let repair_signature = "v4:path";
        let snapshot_key = EntityKeyRow {
            key_name: "path".to_string(),
            key_hash: format!("snapshot-path-{}", sim_uuid()),
        };
        let journal_key = EntityKeyRow {
            key_name: "path".to_string(),
            key_hash: format!("journal-path-{}", sim_uuid()),
        };

        store
            .save_snapshot(&persistence_id, 1, b"snapshot-only")
            .await
            .expect("seed snapshot-only generation");
        let repair_revision = store
            .begin_key_index_backfill(&tenant, entity_type, repair_signature)
            .await
            .expect("begin snapshot-derived repair");

        let timestamp = sim_now();
        store
            .append_with_index_rows(
                &persistence_id,
                0,
                &[PersistenceEnvelope {
                    sequence_nr: 1,
                    event_type: "Create".to_string(),
                    payload: serde_json::json!({}),
                    metadata: EventMetadata {
                        event_id: sim_uuid(),
                        causation_id: sim_uuid(),
                        correlation_id: sim_uuid(),
                        timestamp,
                        actor_id: persistence_id.clone(),
                    },
                }],
                std::slice::from_ref(&journal_key),
                &[],
                IndexReconciliation {
                    keys: true,
                    key_set_signature: Some(repair_signature.to_string()),
                    vectors: false,
                },
            )
            .await
            .expect("replace snapshot-only source with equal-sequence journal state");

        let stale = store
            .backfill_entity_keys(
                &tenant,
                entity_type,
                entity_id,
                1,
                KeyIndexBackfillFence {
                    key_set_signature: repair_signature,
                    contract_revision: repair_revision,
                    expected_journal_sequence: 0,
                    expected_entity_live: true,
                },
                std::slice::from_ref(&snapshot_key),
            )
            .await;
        assert!(matches!(
            stale,
            Err(PersistenceError::JournalBoundaryChanged {
                expected: 0,
                actual: 1,
            })
        ));
        assert_eq!(
            store
                .lookup_by_key(&tenant, entity_type, "path", &journal_key.key_hash)
                .await
                .expect("lookup current journal ownership"),
            Some(entity_id.to_string())
        );
        assert_eq!(
            store
                .lookup_by_key(&tenant, entity_type, "path", &snapshot_key.key_hash)
                .await
                .expect("lookup rejected snapshot ownership"),
            None
        );
    });
}
