use crate::{PostgresEventStore, PostgresSchema, migration::run_migrations_in_schema};
use temper_runtime::persistence::{EventMetadata, EventStore, PersistenceEnvelope};

#[test]
fn boot_comparison_uses_the_selected_postgres_schema_and_only_lagging_rows() {
    let Some(url) = temper_test_resource::env_var("DATABASE_URL") else {
        return;
    };
    sqlx::test_block_on(async {
        let pool = sqlx::PgPool::connect(&url).await.unwrap();
        let schema = PostgresSchema::new(format!("repair.\"{}", uuid::Uuid::new_v4())).unwrap();
        run_migrations_in_schema(&pool, &schema).await.unwrap();
        let store = PostgresEventStore::with_schema(pool.clone(), schema.clone());
        let event = || PersistenceEnvelope {
            sequence_nr: 0,
            event_type: "Touch".to_string(),
            payload: serde_json::json!({}),
            metadata: EventMetadata {
                event_id: uuid::Uuid::new_v4(),
                causation_id: uuid::Uuid::new_v4(),
                correlation_id: uuid::Uuid::new_v4(),
                timestamp: chrono::Utc::now(),
                actor_id: "repair-test".to_string(),
            },
        };
        for tenant in ["installed-app", "another-app"] {
            for (ty, id, projected, seq) in [
                ("Order", "current", 2, 2),
                ("Order", "behind", 1, 3),
                ("Other", "missing", 0, 1),
                ("Order", "snapshot", 1, 1),
            ] {
                let pid = format!("{tenant}:{ty}:{id}");
                let events: Vec<_> = (0..seq).map(|_| event()).collect();
                store.append(&pid, 0, &events).await.unwrap();
                if projected != 0 {
                    store
                        .upsert_query_projection_with_state(
                            tenant,
                            ty,
                            id,
                            "Reviewing",
                            &serde_json::json!({}),
                            &serde_json::json!({}),
                            projected,
                        )
                        .await
                        .unwrap();
                }
            }
            store
                .save_snapshot(&format!("{tenant}:Order:snapshot"), 5, b"{}")
                .await
                .unwrap();
        }
        let gaps = store
            .lagging_query_projections("installed-app")
            .await
            .unwrap();
        let observed: Vec<_> = gaps
            .iter()
            .map(|g| {
                (
                    g.journal.entity_type.as_str(),
                    g.journal.entity_id.as_str(),
                    g.projection.as_ref().map(|p| p.sequence_nr),
                    g.journal.sequence_nr,
                )
            })
            .collect();
        assert_eq!(
            observed,
            vec![
                ("Order", "behind", Some(1), 3),
                ("Order", "snapshot", Some(1), 5),
                ("Other", "missing", None, 1)
            ]
        );
        assert_eq!(
            store
                .load_journal_heads("installed-app")
                .await
                .unwrap()
                .len(),
            4
        );
        assert_eq!(
            store
                .query_projection_checkpoints("installed-app")
                .await
                .unwrap()
                .len(),
            3
        );
        assert_eq!(
            store
                .latest_sequence_nr("installed-app:Order:snapshot")
                .await
                .unwrap(),
            5
        );
        assert_eq!(
            store
                .latest_sequence_nr("installed-app:Order:behind")
                .await
                .unwrap(),
            3
        );
        assert_eq!(
            store
                .latest_sequence_nr("installed-app:Order:absent")
                .await
                .unwrap(),
            0
        );
        sqlx::query(&format!(
            "DROP SCHEMA {} CASCADE",
            schema.prefix().trim_end_matches('.')
        ))
        .execute(&pool)
        .await
        .unwrap();
    });
}

/// Rows of `schema`'s events table that scans and index reads have handed out
/// so far, as the statistics system counts them. The pool has one connection,
/// so every statement the store ran is this backend's, and forcing its flush
/// first makes them all visible.
async fn events_rows_read(pool: &sqlx::PgPool, schema: &PostgresSchema) -> i64 {
    sqlx::query("SELECT pg_stat_force_next_flush()")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query_scalar(
        "SELECT (COALESCE(t.seq_tup_read, 0) + COALESCE((SELECT SUM(i.idx_tup_read)
             FROM pg_stat_user_indexes i WHERE i.relid = t.relid), 0))::bigint
         FROM pg_stat_user_tables t WHERE t.relid = to_regclass($1)",
    )
    .bind(format!("{}events", schema.prefix()))
    .fetch_one(pool)
    .await
    .unwrap()
}

/// Journal rows a restart's comparison reads for `records` records of
/// `events` events each, every one committed with its catalog row.
async fn current_tenant_comparison_journal_reads(url: &str, records: usize, events: usize) -> i64 {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(url)
        .await
        .unwrap();
    let schema = PostgresSchema::new(format!("history.{}", uuid::Uuid::new_v4())).unwrap();
    run_migrations_in_schema(&pool, &schema).await.unwrap();
    let store = PostgresEventStore::with_schema(pool.clone(), schema.clone());
    let fields = serde_json::json!({ "title": "current" });
    let state = serde_json::json!({});
    for n in 0..records {
        let batch: Vec<_> = (0..events)
            .map(|_| PersistenceEnvelope {
                sequence_nr: 0,
                event_type: "Touch".to_string(),
                payload: serde_json::json!({}),
                metadata: EventMetadata {
                    event_id: uuid::Uuid::new_v4(),
                    causation_id: uuid::Uuid::new_v4(),
                    correlation_id: uuid::Uuid::new_v4(),
                    timestamp: chrono::Utc::now(),
                    actor_id: "repair-test".to_string(),
                },
            })
            .collect();
        store
            .append_with_rows(
                &format!("acme:Doc:doc-{n}"),
                0,
                &batch,
                temper_runtime::persistence::CommitRows {
                    projection: Some(temper_runtime::persistence::ProjectionRow::Upsert {
                        status: "Open",
                        fields: &fields,
                        state: &state,
                    }),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
    }
    let before = events_rows_read(&pool, &schema).await;
    let lagging = store.lagging_query_projections("acme").await.unwrap();
    let read = events_rows_read(&pool, &schema).await - before;
    assert!(lagging.is_empty(), "every record is current: {lagging:?}");
    sqlx::query(&format!(
        "DROP SCHEMA {} CASCADE",
        schema.prefix().trim_end_matches('.')
    ))
    .execute(&pool)
    .await
    .unwrap();
    read
}

/// A busy record gains an event every few seconds, and a restart must not pay
/// for each of them again. With every projection committed with its events,
/// records 400 events long cost the comparison no more journal reads than
/// records one event long.
#[test]
fn boot_comparison_journal_reads_do_not_grow_with_history() {
    let Some(url) = temper_test_resource::env_var("DATABASE_URL") else {
        return;
    };
    sqlx::test_block_on(async {
        let short = current_tenant_comparison_journal_reads(&url, 50, 1).await;
        let long = current_tenant_comparison_journal_reads(&url, 50, 400).await;
        assert!(
            long <= short,
            "the comparison read {long} journal rows for 50 current records of 400 events \
             each, against {short} for one event each: it reads the tenant's history"
        );
    });
}
