//! Regression for derived durable writers racing exact key-index repair.

use futures::future::{Either, select};
use temper_runtime::persistence::{
    EntityKeyRow, EventMetadata, EventStore, IndexReconciliation, KeyIndexBackfillFence,
    PersistenceEnvelope, PersistenceError, SnapshotBackfillFence, SnapshotSourceFence,
    encode_activated_key_contract,
};
use temper_runtime::scheduler::{sim_now, sim_uuid};
use temper_store_postgres::PostgresEventStore;

type SegmentRow = (i64, i64, Option<i64>, Option<i64>, i64, bool, String);

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
fn lower_authoritative_snapshot_outranks_stale_catalog_during_key_backfill() {
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
        let tenant = format!("arn238-snapshot-catalog-precedence-{}", sim_uuid());
        let entity_type = "Doc";
        let entity_id = "snapshot-authority";
        let persistence_id = format!("{tenant}:{entity_type}:{entity_id}");
        let signature = "v4:path";
        let snapshot = br#"{"fields":{"Path":"/snapshot"}}"#;
        let key = EntityKeyRow {
            key_name: "path".to_string(),
            key_hash: format!("snapshot-owner-{}", sim_uuid()),
        };

        store
            .save_snapshot(&persistence_id, 5, snapshot)
            .await
            .expect("seed authoritative snapshot generation");
        store
            .upsert_query_projection(
                &tenant,
                entity_type,
                entity_id,
                "Live",
                &serde_json::json!({"Path": "/stale-catalog"}),
                10,
            )
            .await
            .expect("seed newer compatibility catalog projection");
        let revision = store
            .begin_key_index_backfill(&tenant, entity_type, signature)
            .await
            .expect("begin exact key repair");

        store
            .backfill_entity_keys(
                &tenant,
                entity_type,
                entity_id,
                5,
                KeyIndexBackfillFence {
                    key_set_signature: signature,
                    contract_revision: revision,
                    expected_journal_sequence: 0,
                    expected_entity_live: true,
                    expected_snapshot: Some(SnapshotBackfillFence {
                        sequence_nr: 5,
                        state: snapshot,
                    }),
                },
                std::slice::from_ref(&key),
            )
            .await
            .expect("snapshot generation must fence key repair ahead of stale catalog sequence");
        assert!(
            store
                .mark_key_index_backfilled_if_revision(&tenant, entity_type, signature, revision,)
                .await
                .expect("publish snapshot-derived coverage"),
            "stable snapshot-derived ownership must publish coverage"
        );
        assert_eq!(
            store
                .lookup_by_key(&tenant, entity_type, "path", &key.key_hash)
                .await
                .expect("lookup snapshot-derived key owner"),
            Some(entity_id.to_string())
        );
        assert_eq!(
            store
                .key_index_backfilled_types(&tenant)
                .await
                .expect("load published coverage"),
            vec![(entity_type.to_string(), signature.to_string())]
        );
    });
}

#[test]
fn stale_writer_epoch_cannot_resurrect_a_claim_after_a_none_a_activation() {
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
        let tenant = format!("arn238-activation-epoch-{}", sim_uuid());
        let entity_type = "Doc";
        let entity_id = "delayed-writer";
        let persistence_id = format!("{tenant}:{entity_type}:{entity_id}");
        let signature_a = "v3|4:path[4:Path]";
        let signature_none = "v3";
        let row = EntityKeyRow {
            key_name: "path".to_string(),
            key_hash: "released".to_string(),
        };
        let envelope = |event_type: &str| PersistenceEnvelope {
            sequence_nr: 0,
            event_type: event_type.to_string(),
            payload: serde_json::json!({}),
            metadata: EventMetadata {
                event_id: sim_uuid(),
                causation_id: sim_uuid(),
                correlation_id: sim_uuid(),
                timestamp: sim_now(),
                actor_id: persistence_id.clone(),
            },
        };

        let old_epoch = store
            .activate_key_index_contract(&tenant, entity_type, signature_a, false)
            .await
            .expect("activate original A contract");
        store
            .mark_key_index_backfilled(&tenant, entity_type, signature_a)
            .await
            .expect("publish original A readiness");
        let old_contract = encode_activated_key_contract(signature_a, old_epoch);
        store
            .append_with_index_rows(
                &persistence_id,
                0,
                &[envelope("Created")],
                std::slice::from_ref(&row),
                &[],
                IndexReconciliation {
                    keys: true,
                    key_set_signature: Some(old_contract.clone()),
                    ..IndexReconciliation::default()
                },
            )
            .await
            .expect("seed A owner");
        store
            .activate_key_index_contract(&tenant, entity_type, signature_none, true)
            .await
            .expect("activate empty contract and purge rows");
        let current_epoch = store
            .activate_key_index_contract(&tenant, entity_type, signature_a, false)
            .await
            .expect("reactivate A at a new epoch");

        let error = store
            .append_with_index_rows(
                &persistence_id,
                1,
                &[envelope("DelayedOldWriter")],
                std::slice::from_ref(&row),
                &[],
                IndexReconciliation {
                    keys: true,
                    key_set_signature: Some(old_contract),
                    ..IndexReconciliation::default()
                },
            )
            .await
            .expect_err("old A epoch must be rejected after A is reactivated");
        assert!(matches!(
            error,
            PersistenceError::KeyContractActivationStale {
                activated_epoch,
                attempted_epoch: Some(attempted_epoch),
            } if activated_epoch == current_epoch && attempted_epoch == old_epoch
        ));
        assert_eq!(
            store
                .read_events(&persistence_id, 0)
                .await
                .expect("read journal")
                .len(),
            1,
            "rejected writer must not advance the journal"
        );
        assert_eq!(
            store
                .lookup_by_key(&tenant, entity_type, "path", &row.key_hash)
                .await
                .expect("read key owner"),
            None,
            "rejected writer must not resurrect the purged row"
        );
    });
}

#[test]
fn unreconciled_writer_between_entity_repair_and_publication_closes_readiness() {
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
        let tenant = format!("arn238-unreconciled-race-{}", sim_uuid());
        let entity_type = "Doc";
        let entity_id = "writer-during-repair";
        let persistence_id = format!("{tenant}:{entity_type}:{entity_id}");
        let key_set = "v3|4:path[4:Path]";
        let key = EntityKeyRow {
            key_name: "path".to_string(),
            key_hash: "writer-during-repair".to_string(),
        };
        let envelope = |event_type: &str| PersistenceEnvelope {
            sequence_nr: 0,
            event_type: event_type.to_string(),
            payload: serde_json::json!({}),
            metadata: EventMetadata {
                event_id: sim_uuid(),
                causation_id: sim_uuid(),
                correlation_id: sim_uuid(),
                timestamp: sim_now(),
                actor_id: persistence_id.clone(),
            },
        };

        store
            .append(&persistence_id, 0, &[envelope("Created")])
            .await
            .expect("seed pre-contract journal");
        let repair_revision = store
            .begin_key_index_backfill(&tenant, entity_type, key_set)
            .await
            .expect("begin key repair");
        store
            .backfill_entity_keys(
                &tenant,
                entity_type,
                entity_id,
                1,
                KeyIndexBackfillFence {
                    key_set_signature: key_set,
                    contract_revision: repair_revision,
                    expected_journal_sequence: 1,
                    expected_entity_live: true,
                    expected_snapshot: None,
                },
                std::slice::from_ref(&key),
            )
            .await
            .expect("repair entity before racing writer");

        store
            .append(&persistence_id, 1, &[envelope("UnreconciledWriter")])
            .await
            .expect("commit unreconciled writer in publication window");

        assert!(
            !store
                .mark_key_index_backfilled_if_revision(
                    &tenant,
                    entity_type,
                    key_set,
                    repair_revision,
                )
                .await
                .expect("attempt stale readiness publication"),
            "the writer must make the captured repair revision lose its CAS"
        );
        assert!(
            store
                .key_index_backfilled_types(&tenant)
                .await
                .expect("read closed readiness")
                .is_empty(),
            "a partially repaired row set must never become authoritative"
        );
        assert!(
            store
                .key_index_reconciliation_revision(&tenant, entity_type)
                .await
                .expect("read post-writer revision")
                > repair_revision
        );
    });
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
fn stale_snapshot_sources_leave_journal_keys_history_and_segments_unchanged() {
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
        let tenant = format!("arn238-stale-writer-nonmutation-{}", sim_uuid());
        let entity_type = "Doc";
        let entity_id = "stale-source";
        let persistence_id = format!("{tenant}:{entity_type}:{entity_id}");
        let timestamp = sim_now();
        let envelope = |event_type: &str| PersistenceEnvelope {
            sequence_nr: 0,
            event_type: event_type.to_string(),
            payload: serde_json::json!({"event": event_type}),
            metadata: EventMetadata {
                event_id: sim_uuid(),
                causation_id: sim_uuid(),
                correlation_id: sim_uuid(),
                timestamp,
                actor_id: persistence_id.clone(),
            },
        };
        let original_key = EntityKeyRow {
            key_name: "path".to_string(),
            key_hash: "original".to_string(),
        };
        let rejected_key = EntityKeyRow {
            key_name: "path".to_string(),
            key_hash: "rejected".to_string(),
        };
        let signature = "v4:path";
        store
            .append_with_index_rows(
                &persistence_id,
                0,
                &[envelope("Created")],
                std::slice::from_ref(&original_key),
                &[],
                IndexReconciliation {
                    keys: true,
                    key_set_signature: Some(signature.to_string()),
                    vectors: false,
                    snapshot_source: SnapshotSourceFence::Absent,
                },
            )
            .await
            .expect("seed journal and key ownership");
        let captured_snapshot = b"captured".to_vec();
        let replacement_snapshot = b"replacement".to_vec();
        store
            .save_snapshot(&persistence_id, 1, &captured_snapshot)
            .await
            .expect("seed captured snapshot");
        store
            .save_snapshot(&persistence_id, 1, &replacement_snapshot)
            .await
            .expect("replace captured snapshot generation");

        let events_before: Vec<(i64, String, serde_json::Value, i64)> = sqlx::query_as(
            "SELECT sequence_nr, event_type, payload, segment_index FROM events \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 ORDER BY sequence_nr",
        )
        .bind(&tenant)
        .bind(entity_type)
        .bind(entity_id)
        .fetch_all(&pool)
        .await
        .expect("capture journal rows");
        let segments_before: Vec<SegmentRow> = sqlx::query_as(
            "SELECT segment_index, start_sequence_nr, end_sequence_nr, snapshot_sequence, \
                        event_count, sealed_at IS NOT NULL, xmin::text \
                 FROM event_segments WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 \
                 ORDER BY segment_index",
        )
        .bind(&tenant)
        .bind(entity_type)
        .bind(entity_id)
        .fetch_all(&pool)
        .await
        .expect("capture segment rows");
        let history_before: Vec<(i64, Vec<u8>, String)> = sqlx::query_as(
            "SELECT sequence_nr, state, xmin::text FROM snapshot_history \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 ORDER BY sequence_nr",
        )
        .bind(&tenant)
        .bind(entity_type)
        .bind(entity_id)
        .fetch_all(&pool)
        .await
        .expect("capture snapshot history");

        for stale_source in [
            SnapshotSourceFence::Exact {
                sequence_nr: 1,
                state: captured_snapshot.clone(),
            },
            SnapshotSourceFence::Absent,
        ] {
            let error = store
                .append_with_index_rows(
                    &persistence_id,
                    1,
                    &[envelope("Rejected")],
                    std::slice::from_ref(&rejected_key),
                    &[],
                    IndexReconciliation {
                        keys: true,
                        key_set_signature: Some(signature.to_string()),
                        vectors: false,
                        snapshot_source: stale_source,
                    },
                )
                .await
                .expect_err("stale snapshot source must reject append");
            assert!(matches!(error, PersistenceError::SnapshotGenerationChanged));
        }
        let checked_error = store
            .save_snapshot_if_source(
                &persistence_id,
                2,
                b"must-not-commit",
                &SnapshotSourceFence::Exact {
                    sequence_nr: 1,
                    state: captured_snapshot,
                },
                None,
            )
            .await
            .expect_err("stale conditional snapshot save must reject");
        assert!(matches!(
            checked_error,
            PersistenceError::SnapshotGenerationChanged
        ));

        let events_after: Vec<(i64, String, serde_json::Value, i64)> = sqlx::query_as(
            "SELECT sequence_nr, event_type, payload, segment_index FROM events \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 ORDER BY sequence_nr",
        )
        .bind(&tenant)
        .bind(entity_type)
        .bind(entity_id)
        .fetch_all(&pool)
        .await
        .expect("reload journal rows");
        let segments_after: Vec<SegmentRow> = sqlx::query_as(
            "SELECT segment_index, start_sequence_nr, end_sequence_nr, snapshot_sequence, \
                        event_count, sealed_at IS NOT NULL, xmin::text \
                 FROM event_segments WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 \
                 ORDER BY segment_index",
        )
        .bind(&tenant)
        .bind(entity_type)
        .bind(entity_id)
        .fetch_all(&pool)
        .await
        .expect("reload segment rows");
        let history_after: Vec<(i64, Vec<u8>, String)> = sqlx::query_as(
            "SELECT sequence_nr, state, xmin::text FROM snapshot_history \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 ORDER BY sequence_nr",
        )
        .bind(&tenant)
        .bind(entity_type)
        .bind(entity_id)
        .fetch_all(&pool)
        .await
        .expect("reload snapshot history");
        assert_eq!(events_after, events_before);
        assert_eq!(segments_after, segments_before);
        assert_eq!(history_after, history_before);
        assert_eq!(
            store.load_snapshot(&persistence_id).await.unwrap(),
            Some((1, replacement_snapshot))
        );
        assert_eq!(
            store
                .lookup_by_key(&tenant, entity_type, "path", &original_key.key_hash)
                .await
                .unwrap(),
            Some(entity_id.to_string())
        );
        assert_eq!(
            store
                .lookup_by_key(&tenant, entity_type, "path", &rejected_key.key_hash)
                .await
                .unwrap(),
            None
        );
    });
}

#[test]
fn equal_snapshot_rewrite_wins_shared_stream_fence_before_stale_append() {
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
        let tenant = format!("arn238-append-snapshot-race-{}", sim_uuid());
        let persistence_id = format!("{tenant}:Doc:shared-stream-fence");
        let event = |event_type: &str| PersistenceEnvelope {
            sequence_nr: 0,
            event_type: event_type.to_string(),
            payload: serde_json::json!({}),
            metadata: EventMetadata {
                event_id: sim_uuid(),
                causation_id: sim_uuid(),
                correlation_id: sim_uuid(),
                timestamp: sim_now(),
                actor_id: persistence_id.clone(),
            },
        };
        store
            .append(&persistence_id, 0, &[event("Created")])
            .await
            .expect("seed journal");
        let captured = b"captured".to_vec();
        let replacement = b"replacement".to_vec();
        store
            .save_snapshot(&persistence_id, 1, &captured)
            .await
            .expect("seed captured snapshot");
        let blocker = hold_stream_fence(&pool, &persistence_id).await;

        let rewrite = Box::pin(store.save_snapshot(&persistence_id, 1, &replacement));
        let deadline = Box::pin(sqlx::query("SELECT pg_sleep(0.25)").execute(&pool));
        let rewrite = match select(rewrite, deadline).await {
            Either::Left((result, _)) => {
                panic!("snapshot rewrite crossed held stream fence: {result:?}")
            }
            Either::Right((deadline, rewrite)) => {
                deadline.expect("snapshot rewrite wait deadline");
                rewrite
            }
        };

        let rejected_events = vec![event("MustReject")];
        let append = Box::pin(store.append_with_index_rows(
            &persistence_id,
            1,
            &rejected_events,
            &[],
            &[],
            IndexReconciliation {
                snapshot_source: SnapshotSourceFence::Exact {
                    sequence_nr: 1,
                    state: captured,
                },
                ..IndexReconciliation::default()
            },
        ));
        let deadline = Box::pin(sqlx::query("SELECT pg_sleep(0.25)").execute(&pool));
        let append = match select(append, deadline).await {
            Either::Left((result, _)) => {
                panic!("append crossed held stream fence: {result:?}")
            }
            Either::Right((deadline, append)) => {
                deadline.expect("append wait deadline");
                append
            }
        };

        blocker.commit().await.expect("release shared stream fence");
        rewrite
            .await
            .expect("queued snapshot rewrite commits first");
        let append_error = append.await.expect_err("stale append must reject");
        assert!(matches!(
            append_error,
            PersistenceError::SnapshotGenerationChanged
        ));
        assert_eq!(
            store.read_events(&persistence_id, 0).await.unwrap().len(),
            1
        );
        assert_eq!(
            store.load_snapshot(&persistence_id).await.unwrap(),
            Some((1, replacement))
        );
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
                    snapshot_source: Default::default(),
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
                    expected_snapshot: Some(SnapshotBackfillFence {
                        sequence_nr: 1,
                        state: b"snapshot-only",
                    }),
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

#[test]
fn same_sequence_snapshot_content_rewrite_rejects_repair() {
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
        let tenant = format!("arn238-snapshot-content-fence-{}", sim_uuid());
        let entity_type = "Doc";
        let entity_id = "same-sequence-rewrite";
        let persistence_id = format!("{tenant}:{entity_type}:{entity_id}");
        let repair_signature = "v4:path";
        let captured_snapshot = br#"{"fields":{"WorkspaceId":"before"}}"#;
        let replacement_snapshot = br#"{"fields":{"WorkspaceId":"after"}}"#;
        let timestamp = sim_now();

        store
            .append(
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
            )
            .await
            .expect("seed journal generation");
        store
            .save_snapshot(&persistence_id, 1, captured_snapshot)
            .await
            .expect("seed captured snapshot bytes");
        let repair_revision = store
            .begin_key_index_backfill(&tenant, entity_type, repair_signature)
            .await
            .expect("begin snapshot-derived repair");

        let updated = sqlx::query(
            "UPDATE snapshots SET state = $4, created_at = now() \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3",
        )
        .bind(&tenant)
        .bind(entity_type)
        .bind(entity_id)
        .bind(replacement_snapshot.as_slice())
        .execute(&pool)
        .await
        .expect("replace persisted snapshot bytes outside the store writer");
        assert_eq!(updated.rows_affected(), 1);

        let result = store
            .backfill_entity_keys(
                &tenant,
                entity_type,
                entity_id,
                1,
                KeyIndexBackfillFence {
                    key_set_signature: repair_signature,
                    contract_revision: repair_revision,
                    expected_journal_sequence: 1,
                    expected_entity_live: true,
                    expected_snapshot: Some(SnapshotBackfillFence {
                        sequence_nr: 1,
                        state: captured_snapshot,
                    }),
                },
                &[],
            )
            .await;
        assert!(matches!(
            result,
            Err(PersistenceError::SnapshotGenerationChanged)
        ));
    });
}
