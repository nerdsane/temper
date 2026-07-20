//! PostgreSQL regression for snapshot baseline changes after coverage publication.

use temper_runtime::persistence::{EventMetadata, EventStore, PersistenceEnvelope};
use temper_runtime::scheduler::{sim_now, sim_uuid};
use temper_store_postgres::PostgresEventStore;

#[test]
fn older_snapshot_writer_cannot_regress_current_generation_or_coverage() {
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
        let tenant = format!("arn238-snapshot-writer-race-{}", sim_uuid());
        let entity_type = "Doc";
        let persistence_id = format!("{tenant}:{entity_type}:delayed-older");
        let signature = "v4:path";
        store
            .save_snapshot(&persistence_id, 10, b"newer")
            .await
            .expect("commit newer snapshot writer");
        let revision = store
            .begin_key_index_backfill(&tenant, entity_type, signature)
            .await
            .expect("begin coverage epoch");
        assert!(
            store
                .mark_key_index_backfilled_if_revision(&tenant, entity_type, signature, revision,)
                .await
                .expect("publish coverage")
        );

        store
            .save_snapshot(&persistence_id, 5, b"delayed-older")
            .await
            .expect("complete delayed older snapshot writer");

        assert_eq!(
            store
                .load_snapshot(&persistence_id)
                .await
                .expect("load current snapshot"),
            Some((10, b"newer".to_vec())),
            "a delayed writer must not replace a newer authoritative snapshot"
        );
        assert_eq!(
            store
                .key_index_reconciliation_revision(&tenant, entity_type)
                .await
                .expect("read unchanged coverage epoch"),
            revision,
            "an ignored older snapshot must not invalidate coverage"
        );
        assert_eq!(
            store
                .key_index_backfilled_types(&tenant)
                .await
                .expect("read preserved coverage watermark"),
            vec![(entity_type.to_string(), signature.to_string())]
        );
    });
}

#[test]
fn same_sequence_snapshot_rewrite_invalidates_published_key_coverage() {
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
        let tenant = format!("arn238-snapshot-published-{}", sim_uuid());
        let entity_type = "Doc";
        let persistence_id = format!("{tenant}:{entity_type}:snapshot-rewrite");
        let signature = "v4:path";
        let timestamp = sim_now();
        store
            .append(
                &persistence_id,
                0,
                &[PersistenceEnvelope {
                    sequence_nr: 0,
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
            )
            .await
            .expect("seed journal high-water");
        store
            .save_snapshot(&persistence_id, 1, b"before")
            .await
            .expect("seed captured snapshot baseline");
        let revision = store
            .begin_key_index_backfill(&tenant, entity_type, signature)
            .await
            .expect("begin coverage epoch");
        assert!(
            store
                .mark_key_index_backfilled_if_revision(&tenant, entity_type, signature, revision,)
                .await
                .expect("publish coverage")
        );

        store
            .save_snapshot(&persistence_id, 1, b"before")
            .await
            .expect("repeat identical snapshot write");
        assert_eq!(
            store
                .key_index_reconciliation_revision(&tenant, entity_type)
                .await
                .expect("read unchanged snapshot coverage epoch"),
            revision,
            "identical snapshot bytes and sequence must not churn the coverage epoch"
        );
        assert_eq!(
            store
                .key_index_backfilled_types(&tenant)
                .await
                .expect("read preserved snapshot coverage watermark"),
            vec![(entity_type.to_string(), signature.to_string())],
            "identical snapshot writes must preserve published coverage"
        );

        store
            .upsert_query_projection(
                &tenant,
                entity_type,
                "snapshot-rewrite",
                "Ready",
                &serde_json::json!({"Path": "/journal-dominated"}),
                1,
            )
            .await
            .expect("write catalog projection represented by the journal");
        assert_eq!(
            store
                .key_index_reconciliation_revision(&tenant, entity_type)
                .await
                .expect("read unchanged catalog coverage epoch"),
            revision,
            "a catalog projection at the journal high-water must reuse the append fence"
        );
        assert_eq!(
            store
                .key_index_backfilled_types(&tenant)
                .await
                .expect("read preserved catalog coverage watermark"),
            vec![(entity_type.to_string(), signature.to_string())],
            "journal-dominated catalog writes must preserve published coverage"
        );

        store
            .save_snapshot(&persistence_id, 1, b"after")
            .await
            .expect("rewrite snapshot bytes at the journal high-water");

        assert!(
            store
                .key_index_reconciliation_revision(&tenant, entity_type)
                .await
                .expect("read current coverage epoch")
                > revision,
            "changed snapshot baseline bytes must invalidate the published coverage epoch"
        );
        assert!(
            store
                .key_index_backfilled_types(&tenant)
                .await
                .expect("read coverage watermarks")
                .is_empty(),
            "stale ownership rows must not remain authoritative after a snapshot rewrite"
        );
    });
}

#[test]
fn delayed_snapshot_splits_durable_tail_and_equal_rewrite_preserves_topology() {
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
        let tenant = format!("arn238-delayed-segment-snapshot-{}", sim_uuid());
        let entity_type = "Doc";
        let entity_id = "delayed-segment-snapshot";
        let persistence_id = format!("{tenant}:{entity_type}:{entity_id}");
        let timestamp = sim_now();
        let events = (1..=10)
            .map(|sequence| PersistenceEnvelope {
                sequence_nr: sequence,
                event_type: "Updated".to_string(),
                payload: serde_json::json!({"sequence": sequence}),
                metadata: EventMetadata {
                    event_id: sim_uuid(),
                    causation_id: sim_uuid(),
                    correlation_id: sim_uuid(),
                    timestamp,
                    actor_id: persistence_id.clone(),
                },
            })
            .collect::<Vec<_>>();
        store
            .append(&persistence_id, 0, &events)
            .await
            .expect("append durable tail before delayed snapshot");

        store
            .save_snapshot(&persistence_id, 5, b"snapshot-a")
            .await
            .expect("save delayed snapshot boundary");

        let segment_rows = || async {
            sqlx::query_as::<_, (i64, i64, Option<i64>, Option<i64>, i64, bool)>(
                "SELECT segment_index, start_sequence_nr, end_sequence_nr, \
                        snapshot_sequence, event_count, sealed_at IS NOT NULL \
                 FROM event_segments \
                 WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 \
                 ORDER BY segment_index",
            )
            .bind(&tenant)
            .bind(entity_type)
            .bind(entity_id)
            .fetch_all(&pool)
            .await
            .expect("read event segment topology")
        };
        let event_rows = || async {
            sqlx::query_as::<_, (i64, i64)>(
                "SELECT sequence_nr, segment_index FROM events \
                 WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 \
                 ORDER BY sequence_nr",
            )
            .bind(&tenant)
            .bind(entity_type)
            .bind(entity_id)
            .fetch_all(&pool)
            .await
            .expect("read event segment assignments")
        };
        let segment_versions = || async {
            sqlx::query_as::<_, (i64, String)>(
                "SELECT segment_index, xmin::text FROM event_segments \
                 WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 \
                 ORDER BY segment_index",
            )
            .bind(&tenant)
            .bind(entity_type)
            .bind(entity_id)
            .fetch_all(&pool)
            .await
            .expect("read event segment row versions")
        };

        let expected_segments = vec![
            (0, 1, Some(5), Some(5), 5, true),
            (1, 6, Some(10), None, 5, false),
        ];
        assert_eq!(
            segment_rows().await,
            expected_segments,
            "a delayed snapshot must retain the already-durable tail in the open successor"
        );
        let expected_events = (1..=10)
            .map(|sequence| (sequence, if sequence <= 5 { 0 } else { 1 }))
            .collect::<Vec<_>>();
        assert_eq!(
            event_rows().await,
            expected_events,
            "events after the delayed boundary must move to the successor segment"
        );

        let segments_before_rewrite = segment_rows().await;
        let events_before_rewrite = event_rows().await;
        let segment_versions_before_rewrite = segment_versions().await;
        store
            .save_snapshot(&persistence_id, 5, b"snapshot-b")
            .await
            .expect("replace same-sequence snapshot bytes");
        assert_eq!(segment_rows().await, segments_before_rewrite);
        assert_eq!(event_rows().await, events_before_rewrite);
        assert_eq!(
            segment_versions().await,
            segment_versions_before_rewrite,
            "same-sequence source replacement must not rewrite segment rows"
        );
        assert_eq!(
            store
                .load_snapshot(&persistence_id)
                .await
                .expect("load replaced snapshot"),
            Some((5, b"snapshot-b".to_vec()))
        );
    });
}
