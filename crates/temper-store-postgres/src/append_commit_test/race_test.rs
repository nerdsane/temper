//! An append that loses the race for the record's catalog row.
//!
//! A test transaction does what a background refresh does (rewrites the
//! catalog row and its field index) and stays open, so the append's `locked`
//! waits on it. When it commits, the append's snapshot is older than the row
//! it locked: the run writes nothing and the append runs once more while it
//! holds the lock. The retry seam counts the runs.

use sqlx::{Postgres, Transaction};
use tokio::sync::oneshot;

use super::*;
use crate::append_commit::retry_seam::{self, Seam};
use crate::platform::{json_hash, scalar_index_fields};

/// Rewrite the record's catalog row and field index the way a refresh does,
/// adding a `legacy` field the next move does not carry, and hold the
/// transaction open.
async fn rival_refresh(
    pool: &PgPool,
    store: &PostgresEventStore,
    label: &str,
) -> Transaction<'static, Postgres> {
    let mut rival = pool.begin().await.unwrap();
    sqlx::query(&store.schema().qualify_sql(
        "UPDATE {schema}entity_catalog \
         SET fields = fields || jsonb_build_object('legacy', $1::text), projection_hash = $1 \
         WHERE tenant = 't' AND entity_type = 'Panel' AND entity_id = 'p1'",
    ))
    .bind(label)
    .execute(&mut *rival)
    .await
    .unwrap();
    sqlx::query(&store.schema().qualify_sql(
        "INSERT INTO {schema}entity_field_index \
         (tenant, entity_type, entity_id, field_name, field_value, status) \
         VALUES ('t', 'Panel', 'p1', 'legacy', $1, 'Open') \
         ON CONFLICT (tenant, entity_type, entity_id, field_name) \
         DO UPDATE SET field_value = EXCLUDED.field_value",
    ))
    .bind(label)
    .execute(&mut *rival)
    .await
    .unwrap();
    rival
}

/// Wait until an append statement in `schema` is blocked on a lock. The
/// match is on the statement's opening, which pg_stat_activity always keeps.
async fn wait_for_blocked_append(pool: &PgPool, schema: &PostgresSchema) {
    let waiting = format!(
        "SELECT COUNT(*)::bigint FROM pg_stat_activity \
         WHERE wait_event_type = 'Lock' AND query LIKE 'WITH locked AS (%{}%'",
        schema.prefix().trim_end_matches('.').trim_matches('"')
    );
    for _ in 0..500 {
        let waiters: i64 = sqlx::query_scalar(&waiting).fetch_one(pool).await.unwrap();
        if waiters > 0 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("the append never waited on the rival's lock");
}

/// The record's rows follow its journal at `head`: the catalog row is at the
/// head with the move's fields and their hash, the field index holds exactly
/// the catalog's scalar fields (the rival's `legacy` row is gone), and no
/// pending gap mark is left.
async fn assert_rows_follow_journal(
    store: &PostgresEventStore,
    pool: &PgPool,
    head: i64,
    fields: &serde_json::Value,
) {
    let journal_head: i64 = sqlx::query_scalar(&store.schema().qualify_sql(
        "SELECT MAX(sequence_nr) FROM {schema}events WHERE tenant = 't' AND entity_id = 'p1'",
    ))
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(journal_head, head);
    let (sequence_nr, hash, catalog_fields): (i64, String, serde_json::Value) =
        sqlx::query_as(&store.schema().qualify_sql(
            "SELECT sequence_nr, projection_hash, fields FROM {schema}entity_catalog \
             WHERE tenant = 't' AND entity_type = 'Panel' AND entity_id = 'p1'",
        ))
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(
        (sequence_nr, hash.as_str(), &catalog_fields),
        (head, json_hash(fields).as_str(), fields),
        "the catalog row is the move's, at the journal head"
    );
    let want: Vec<(String, String)> = scalar_index_fields(&catalog_fields).0.into_iter().collect();
    assert_eq!(
        index_rows(store, pool).await,
        want,
        "the field index holds the catalog's fields and nothing the rival wrote"
    );
    let pending: i64 = sqlx::query_scalar(&store.schema().qualify_sql(
        "SELECT COUNT(*)::bigint FROM {schema}entity_projection_delta \
         WHERE tenant = 't' AND NOT terminal",
    ))
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(pending, 0, "a projected move leaves no pending mark");
}

async fn first_move(store: &PostgresEventStore) {
    let first = Rows::at(1, serde_json::json!({"Id": "p1", "app": "a1"}));
    store
        .append_with_rows(
            PID,
            0,
            &[envelope("Created", serde_json::json!({}))],
            first.commit(),
        )
        .await
        .unwrap();
}

/// An append on its own loses the race once, runs exactly once more inside a
/// transaction that takes the catalog row lock first, and lands with every
/// row following the journal.
#[test]
fn a_raced_append_runs_once_more_under_the_lock_and_lands() {
    let Some(url) = temper_test_resource::env_var("DATABASE_URL") else {
        return;
    };
    sqlx::test_block_on(async {
        let (pool, schema, store) = isolated_store(&url).await;
        first_move(&store).await;
        retry_seam::install(schema.prefix(), Seam::default());
        let rival = rival_refresh(&pool, &store, "refreshed").await;

        let racing = store.clone();
        let writer = tokio::spawn(async move {
            let rows = Rows::at(2, serde_json::json!({"Id": "p1", "app": "racer"}));
            racing
                .append_with_rows(
                    PID,
                    1,
                    &[envelope("Moved", serde_json::json!({}))],
                    rows.commit(),
                )
                .await
        });
        wait_for_blocked_append(&pool, &schema).await;
        rival.commit().await.unwrap();

        assert_eq!(writer.await.unwrap().unwrap(), 2, "the append lands");
        assert_eq!(
            retry_seam::runs(schema.prefix()),
            2,
            "one raced run and exactly one more"
        );
        assert_rows_follow_journal(
            &store,
            &pool,
            2,
            &serde_json::json!({"Id": "p1", "app": "racer"}),
        )
        .await;
        let timers: Vec<(String, i64)> = sqlx::query_as(&store.schema().qualify_sql(
            "SELECT arm_id, sequence_nr FROM {schema}state_timers WHERE tenant = 't'",
        ))
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            timers,
            vec![("Open-2".to_string(), 2)],
            "the move's timer row"
        );
        drop_schema(&pool, &schema).await;
    });
}

/// In a batch, the raced run's lock stays with the batch's transaction, so
/// the second run is the next statement and lands.
#[test]
fn a_raced_batch_append_runs_once_more_and_lands() {
    let Some(url) = temper_test_resource::env_var("DATABASE_URL") else {
        return;
    };
    sqlx::test_block_on(async {
        let (pool, schema, store) = isolated_store(&url).await;
        first_move(&store).await;
        retry_seam::install(schema.prefix(), Seam::default());
        let rival = rival_refresh(&pool, &store, "refreshed").await;

        let racing = store.clone();
        let writer =
            tokio::spawn(
                async move { racing.append_batch(&[batched(PID, 1, kept("racer"))]).await },
            );
        wait_for_blocked_append(&pool, &schema).await;
        rival.commit().await.unwrap();

        let written = writer.await.unwrap().unwrap();
        assert_eq!(written[0].sequence_nr, 2, "the batch lands");
        assert_eq!(
            retry_seam::runs(schema.prefix()),
            2,
            "one raced run and exactly one more"
        );
        assert_rows_follow_journal(&store, &pool, 2, &serde_json::json!({"app": "racer"})).await;
        drop_schema(&pool, &schema).await;
    });
}

/// While the retry holds the lock its run cannot race. The seam leaves the
/// retry's lock statement out and lets a second rival take the row first, so
/// the second run races for real: the append returns the storage error,
/// writes nothing, and does not run a third time.
#[test]
fn a_second_race_returns_an_error_and_writes_nothing() {
    let Some(url) = temper_test_resource::env_var("DATABASE_URL") else {
        return;
    };
    sqlx::test_block_on(async {
        let (pool, schema, store) = isolated_store(&url).await;
        first_move(&store).await;
        let (retrying, retry_started) = oneshot::channel();
        let (go, retry_goes) = oneshot::channel();
        retry_seam::install(
            schema.prefix(),
            Seam {
                skip_retry_lock: true,
                before_retry: Some((retrying, retry_goes)),
                ..Seam::default()
            },
        );
        let first_rival = rival_refresh(&pool, &store, "first").await;

        let racing = store.clone();
        let writer = tokio::spawn(async move {
            let rows = Rows::at(2, serde_json::json!({"Id": "p1", "app": "racer"}));
            racing
                .append_with_rows(
                    PID,
                    1,
                    &[envelope("Moved", serde_json::json!({}))],
                    rows.commit(),
                )
                .await
        });
        wait_for_blocked_append(&pool, &schema).await;
        first_rival.commit().await.unwrap();

        retry_started.await.unwrap();
        let second_rival = rival_refresh(&pool, &store, "second").await;
        go.send(()).unwrap();
        wait_for_blocked_append(&pool, &schema).await;
        let before = snapshot(&pool, &schema).await;
        second_rival.commit().await.unwrap();

        let refused = writer.await.unwrap();
        assert!(
            matches!(&refused, Err(PersistenceError::Storage(message))
                if message.contains("changed while this append held its lock")),
            "{refused:?}"
        );
        assert_eq!(retry_seam::runs(schema.prefix()), 2, "no third run");
        let after = snapshot(&pool, &schema).await;
        for table in TABLES {
            if table != "entity_catalog" && table != "entity_field_index" {
                assert_eq!(
                    after[table], before[table],
                    "the append wrote no {table} row"
                );
            }
        }
        let (sequence_nr, hash): (i64, String) = sqlx::query_as(&store.schema().qualify_sql(
            "SELECT sequence_nr, projection_hash FROM {schema}entity_catalog \
             WHERE tenant = 't' AND entity_id = 'p1'",
        ))
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            (sequence_nr, hash.as_str()),
            (1, "second"),
            "the catalog row is the second rival's, not the append's"
        );
        assert_eq!(
            index_rows(&store, &pool).await,
            vec![
                ("Id".to_string(), "p1".to_string()),
                ("app".to_string(), "a1".to_string()),
                ("legacy".to_string(), "second".to_string()),
            ],
            "the field index is the second rival's, not the append's"
        );
        drop_schema(&pool, &schema).await;
    });
}
