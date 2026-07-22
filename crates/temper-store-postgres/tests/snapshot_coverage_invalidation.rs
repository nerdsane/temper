//! PostgreSQL regression for snapshot baseline changes after coverage publication.

use temper_runtime::persistence::{
    EntityKeyRow, EventMetadata, EventStore, IndexReconciliation, KeyIndexBackfillFence,
    PersistenceAppend, PersistenceEnvelope, ProjectionSourceFence, SnapshotBackfillFence,
    SnapshotSourceFence,
};
use temper_runtime::scheduler::{sim_now, sim_uuid};
use temper_store_postgres::PostgresEventStore;

type SegmentTopologyRow = (i64, i64, Option<i64>, Option<i64>, i64, bool);

#[test]
fn database_triggers_fence_source_writes_from_pre_ledger_binaries() {
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
        let tenant = format!("arn238-mixed-version-source-{}", sim_uuid());
        let entity_type = "Doc";
        let event_id = "legacy-event-writer";
        let snapshot_id = "legacy-snapshot-writer";

        sqlx::query(
            "INSERT INTO events \
             (tenant, entity_type, entity_id, sequence_nr, event_type, payload, metadata) \
             VALUES ($1, $2, $3, 1, 'Created', '{}'::jsonb, '{}'::jsonb)",
        )
        .bind(&tenant)
        .bind(entity_type)
        .bind(event_id)
        .execute(&pool)
        .await
        .expect("simulate an event append from a pre-ledger binary");
        assert!(
            sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM query_projection_dirty \
                 WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3)",
            )
            .bind(&tenant)
            .bind(entity_type)
            .bind(event_id)
            .fetch_one(&pool)
            .await
            .expect("read event-trigger marker"),
            "the database trigger must fence event writes from older binaries"
        );

        sqlx::query(
            "DELETE FROM query_projection_dirty \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3",
        )
        .bind(&tenant)
        .bind(entity_type)
        .bind(event_id)
        .execute(&pool)
        .await
        .expect("simulate a new reader clearing the source marker");
        sqlx::query(
            "INSERT INTO entity_catalog \
             (tenant, entity_type, entity_id, status, fields, sequence_nr) \
             VALUES ($1, $2, $3, 'Live', '{\"Version\":\"stale\"}'::jsonb, 1)",
        )
        .bind(&tenant)
        .bind(entity_type)
        .bind(event_id)
        .execute(&pool)
        .await
        .expect("simulate a delayed projector from a pre-ledger binary");
        assert!(
            sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM query_projection_dirty \
                 WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3)",
            )
            .bind(&tenant)
            .bind(entity_type)
            .bind(event_id)
            .fetch_one(&pool)
            .await
            .expect("read delayed-projector marker"),
            "the catalog trigger must re-fence a delayed projector from an older binary"
        );

        sqlx::query(
            "INSERT INTO snapshots \
             (tenant, entity_type, entity_id, sequence_nr, state) \
             VALUES ($1, $2, $3, 4, $4)",
        )
        .bind(&tenant)
        .bind(entity_type)
        .bind(snapshot_id)
        .bind(b"legacy-snapshot".as_slice())
        .execute(&pool)
        .await
        .expect("simulate a snapshot write from a pre-ledger binary");
        assert!(
            sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM query_projection_dirty \
                 WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3)",
            )
            .bind(&tenant)
            .bind(entity_type)
            .bind(snapshot_id)
            .fetch_one(&pool)
            .await
            .expect("read snapshot-trigger marker"),
            "the database trigger must fence snapshot writes from older binaries"
        );
    });
}

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
        let store = PostgresEventStore::new(pool.clone());
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
fn exact_snapshot_projection_repair_preserves_published_key_coverage() {
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
        let tenant = format!("arn238-snapshot-projection-coverage-{}", sim_uuid());
        let entity_type = "Doc";
        let entity_id = "snapshot-projection";
        let persistence_id = format!("{tenant}:{entity_type}:{entity_id}");
        let signature = "v4:path";
        let snapshot = b"snapshot-only-generation";
        let fields = serde_json::json!({"Id": entity_id, "Path": "/snapshot"});
        let state = serde_json::json!({
            "entity_type": entity_type,
            "entity_id": entity_id,
            "status": "Live",
            "fields": fields,
            "sequence_nr": 5
        });
        let key = EntityKeyRow {
            key_name: "path".to_string(),
            key_hash: format!("snapshot-projection-key-{}", sim_uuid()),
        };

        store
            .save_snapshot(&persistence_id, 5, snapshot)
            .await
            .expect("seed snapshot-only source");
        let revision = store
            .begin_key_index_backfill(&tenant, entity_type, signature)
            .await
            .expect("begin snapshot-derived key repair");
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
            .expect("repair snapshot-derived key ownership");
        assert!(
            store
                .mark_key_index_backfilled_if_revision(&tenant, entity_type, signature, revision,)
                .await
                .expect("publish key coverage")
        );

        assert!(
            store
                .upsert_query_projection_with_state_if_source(
                    &tenant,
                    entity_type,
                    entity_id,
                    "Live",
                    &fields,
                    &state,
                    5,
                    ProjectionSourceFence {
                        expected_journal_sequence: 0,
                        expected_snapshot: Some(SnapshotBackfillFence {
                            sequence_nr: 5,
                            state: snapshot,
                        }),
                    },
                )
                .await
                .expect("repair query projection from the exact snapshot source")
        );

        assert_eq!(
            store
                .key_index_reconciliation_revision(&tenant, entity_type)
                .await
                .expect("read preserved key revision"),
            revision,
            "derived projection repair must not invalidate snapshot-derived ownership"
        );
        assert_eq!(
            store
                .key_index_backfilled_types(&tenant)
                .await
                .expect("read preserved key coverage"),
            vec![(entity_type.to_string(), signature.to_string())]
        );
        assert_eq!(
            store
                .lookup_by_key(&tenant, entity_type, "path", &key.key_hash)
                .await
                .expect("resolve preserved snapshot-derived key"),
            Some(entity_id.to_string())
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
fn projection_repair_fences_snapshot_bytes_and_survives_delayed_plain_delivery() {
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
        let tenant = format!("arn238-projection-source-{}", sim_uuid());
        let entity_type = "Doc";
        let entity_id = "projection-race";
        let persistence_id = format!("{tenant}:{entity_type}:{entity_id}");
        let snapshot_a = b"snapshot-a".to_vec();
        let snapshot_b = b"snapshot-b".to_vec();
        let fields_a = serde_json::json!({"Id": entity_id, "Status": "Live", "Version": "A"});
        let state_a = fields_a.clone();
        let fields_b = serde_json::json!({"Id": entity_id, "Status": "Live", "Version": "B"});
        let state_b = fields_b.clone();
        let envelope = |event_type: &str, to_status: &str| PersistenceEnvelope {
            sequence_nr: 0,
            event_type: event_type.to_string(),
            payload: serde_json::json!({"to_status": to_status}),
            metadata: EventMetadata {
                event_id: sim_uuid(),
                causation_id: sim_uuid(),
                correlation_id: sim_uuid(),
                timestamp: sim_now(),
                actor_id: persistence_id.clone(),
            },
        };

        store
            .append(&persistence_id, 0, &[envelope("Created", "Live")])
            .await
            .expect("seed journal generation");
        store
            .save_snapshot(&persistence_id, 1, &snapshot_a)
            .await
            .expect("seed snapshot A");
        assert!(
            store
                .upsert_query_projection_with_state_if_source(
                    &tenant,
                    entity_type,
                    entity_id,
                    "Live",
                    &fields_a,
                    &state_a,
                    1,
                    ProjectionSourceFence {
                        expected_journal_sequence: 1,
                        expected_snapshot: Some(SnapshotBackfillFence {
                            sequence_nr: 1,
                            state: &snapshot_a,
                        }),
                    },
                )
                .await
                .expect("project snapshot A")
        );
        store
            .save_snapshot(&persistence_id, 1, &snapshot_b)
            .await
            .expect("replace snapshot A with B at the same HWM");
        assert!(
            !store
                .upsert_query_projection_with_state_if_source(
                    &tenant,
                    entity_type,
                    entity_id,
                    "Live",
                    &fields_a,
                    &state_a,
                    1,
                    ProjectionSourceFence {
                        expected_journal_sequence: 1,
                        expected_snapshot: Some(SnapshotBackfillFence {
                            sequence_nr: 1,
                            state: &snapshot_a,
                        }),
                    },
                )
                .await
                .expect("reject stale snapshot A fence")
        );
        assert_eq!(
            store
                .dirty_query_projection_entity_ids(&tenant, entity_type, 10)
                .await
                .expect("snapshot B remains dirty"),
            vec![entity_id.to_string()]
        );
        assert!(
            store
                .upsert_query_projection_with_state_if_source(
                    &tenant,
                    entity_type,
                    entity_id,
                    "Live",
                    &fields_b,
                    &state_b,
                    1,
                    ProjectionSourceFence {
                        expected_journal_sequence: 1,
                        expected_snapshot: Some(SnapshotBackfillFence {
                            sequence_nr: 1,
                            state: &snapshot_b,
                        }),
                    },
                )
                .await
                .expect("repair snapshot B")
        );

        store
            .upsert_query_projection_with_state(
                &tenant,
                entity_type,
                entity_id,
                "Live",
                &fields_a,
                &state_a,
                1,
            )
            .await
            .expect("deliver delayed unfenced snapshot A projection");
        assert_eq!(
            store
                .dirty_query_projection_entity_ids(&tenant, entity_type, 10)
                .await
                .expect("delayed projection re-marks dirty"),
            vec![entity_id.to_string()]
        );
        assert!(
            store
                .upsert_query_projection_with_state_if_source(
                    &tenant,
                    entity_type,
                    entity_id,
                    "Live",
                    &fields_b,
                    &state_b,
                    1,
                    ProjectionSourceFence {
                        expected_journal_sequence: 1,
                        expected_snapshot: Some(SnapshotBackfillFence {
                            sequence_nr: 1,
                            state: &snapshot_b,
                        }),
                    },
                )
                .await
                .expect("repair delayed A back to B")
        );
        assert!(
            !store
                .remove_query_projection_if_exact(
                    &tenant,
                    entity_type,
                    entity_id,
                    "Live",
                    &fields_a,
                    &state_a,
                    1,
                )
                .await
                .expect("full-row cleanup CAS preserves B")
        );
        let row = store
            .load_entity_catalog_rows_pg(&tenant, entity_type, &[entity_id.to_string()])
            .await
            .expect("load repaired catalog")
            .pop()
            .expect("B row remains");
        assert_eq!(row.fields, fields_b);
        let indexed_ids = sqlx::query_scalar::<_, String>(
            "SELECT entity_id FROM entity_field_index \
             WHERE tenant = $1 AND entity_type = $2 \
               AND field_name = $3 AND field_value = $4 \
             ORDER BY entity_id",
        )
        .bind(&tenant)
        .bind(entity_type)
        .bind("Version")
        .bind("B")
        .fetch_all(&pool)
        .await
        .expect("query B field index");
        assert!(
            indexed_ids.contains(&entity_id.to_string()),
            "failed exact cleanup must preserve B's EAV rows"
        );

        store
            .append(&persistence_id, 1, &[envelope("Delete", "Deleted")])
            .await
            .expect("advance source to Deleted");
        assert!(
            store
                .remove_query_projection_if_source(
                    &tenant,
                    entity_type,
                    entity_id,
                    ProjectionSourceFence {
                        expected_journal_sequence: 2,
                        expected_snapshot: Some(SnapshotBackfillFence {
                            sequence_nr: 1,
                            state: &snapshot_b,
                        }),
                    },
                )
                .await
                .expect("remove Deleted projection")
        );
        store
            .append(&persistence_id, 2, &[envelope("Restore", "Live")])
            .await
            .expect("advance source back to Live");
        assert!(
            store
                .upsert_query_projection_with_state_if_source(
                    &tenant,
                    entity_type,
                    entity_id,
                    "Live",
                    &fields_b,
                    &state_b,
                    3,
                    ProjectionSourceFence {
                        expected_journal_sequence: 3,
                        expected_snapshot: Some(SnapshotBackfillFence {
                            sequence_nr: 1,
                            state: &snapshot_b,
                        }),
                    },
                )
                .await
                .expect("restore Live projection")
        );
        assert!(
            store
                .dirty_query_projection_entity_ids(&tenant, entity_type, 1)
                .await
                .expect("restored source is clean")
                .is_empty()
        );

        assert!(
            store
                .remove_query_projection_if_exact(
                    &tenant,
                    entity_type,
                    entity_id,
                    "Live",
                    &fields_b,
                    &state_b,
                    3,
                )
                .await
                .expect("remove exact attempted projection after closing fault")
        );
        assert_eq!(
            store
                .dirty_query_projection_entity_ids(&tenant, entity_type, 1)
                .await
                .expect("exact cleanup marks projection dirty"),
            vec![entity_id.to_string()]
        );
        assert!(
            store
                .upsert_query_projection_with_state_if_source(
                    &tenant,
                    entity_type,
                    entity_id,
                    "Live",
                    &fields_b,
                    &state_b,
                    3,
                    ProjectionSourceFence {
                        expected_journal_sequence: 3,
                        expected_snapshot: Some(SnapshotBackfillFence {
                            sequence_nr: 1,
                            state: &snapshot_b,
                        }),
                    },
                )
                .await
                .expect("repair exact-cleanup absence")
        );
        assert!(
            store
                .dirty_query_projection_entity_ids(&tenant, entity_type, 1)
                .await
                .expect("exact-cleanup repair clears marker")
                .is_empty()
        );
    });
}

#[test]
fn exact_snapshot_source_replaces_a_higher_stale_catalog_sequence() {
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
        let tenant = format!("arn238-snapshot-catalog-fence-{}", sim_uuid());
        let entity_type = "Doc";
        let entity_id = "snapshot-owner";
        let persistence_id = format!("{tenant}:{entity_type}:{entity_id}");
        let stale_fields =
            serde_json::json!({"Id": entity_id, "Status": "Live", "Version": "stale"});
        let current_fields =
            serde_json::json!({"Id": entity_id, "Status": "Live", "Version": "snapshot"});
        let snapshot = b"authoritative-snapshot".to_vec();

        store
            .upsert_query_projection_with_state(
                &tenant,
                entity_type,
                entity_id,
                "Live",
                &stale_fields,
                &stale_fields,
                10,
            )
            .await
            .expect("seed higher catalog-only compatibility sequence");
        store
            .save_snapshot(&persistence_id, 5, &snapshot)
            .await
            .expect("publish lower authoritative snapshot generation");
        assert!(
            store
                .upsert_query_projection_with_state_if_source(
                    &tenant,
                    entity_type,
                    entity_id,
                    "Live",
                    &current_fields,
                    &current_fields,
                    5,
                    ProjectionSourceFence {
                        expected_journal_sequence: 0,
                        expected_snapshot: Some(SnapshotBackfillFence {
                            sequence_nr: 5,
                            state: &snapshot,
                        }),
                    },
                )
                .await
                .expect("repair from exact snapshot source"),
            "the exact current snapshot must outrank a higher stale catalog sequence"
        );
        let row = store
            .load_entity_catalog_rows_pg(&tenant, entity_type, &[entity_id.to_string()])
            .await
            .expect("load repaired catalog")
            .pop()
            .expect("repaired catalog row");
        assert_eq!(row.sequence_nr, 5);
        assert_eq!(row.fields, current_fields);
        assert!(
            store
                .dirty_query_projection_entity_ids(&tenant, entity_type, 1)
                .await
                .expect("read closed dirty ledger")
                .is_empty()
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

#[test]
fn snapshot_only_and_batch_appends_keep_real_segments_contiguous() {
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
        let tenant = format!("arn238-snapshot-only-segments-{}", sim_uuid());
        let envelope = |persistence_id: &str, event_type: &str| PersistenceEnvelope {
            sequence_nr: 0,
            event_type: event_type.to_string(),
            payload: serde_json::json!({}),
            metadata: EventMetadata {
                event_id: sim_uuid(),
                causation_id: sim_uuid(),
                correlation_id: sim_uuid(),
                timestamp: sim_now(),
                actor_id: persistence_id.to_string(),
            },
        };

        let snapshot_only_id = "snapshot-only-first-journal";
        let snapshot_only_pid = format!("{tenant}:Doc:{snapshot_only_id}");
        let snapshot = b"snapshot-only".to_vec();
        store
            .save_snapshot(&snapshot_only_pid, 5, &snapshot)
            .await
            .expect("save snapshot-only generation");
        let segment_count: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM event_segments \
             WHERE tenant = $1 AND entity_type = 'Doc' AND entity_id = $2",
        )
        .bind(&tenant)
        .bind(snapshot_only_id)
        .fetch_one(&pool)
        .await
        .expect("count snapshot-only segments");
        assert_eq!(segment_count.0, 0);
        store
            .append_with_index_rows(
                &snapshot_only_pid,
                0,
                &[envelope(&snapshot_only_pid, "FirstJournalEvent")],
                &[],
                &[],
                IndexReconciliation {
                    snapshot_source: SnapshotSourceFence::Exact {
                        sequence_nr: 5,
                        state: snapshot,
                    },
                    ..IndexReconciliation::default()
                },
            )
            .await
            .expect("append first journal generation over snapshot baseline");
        let first_topology: Vec<(i64, i64, Option<i64>, i64, bool)> = sqlx::query_as(
            "SELECT segment_index, start_sequence_nr, end_sequence_nr, event_count, sealed_at IS NOT NULL \
             FROM event_segments WHERE tenant = $1 AND entity_type = 'Doc' AND entity_id = $2 \
             ORDER BY segment_index",
        )
        .bind(&tenant)
        .bind(snapshot_only_id)
        .fetch_all(&pool)
        .await
        .expect("load first journal topology");
        assert_eq!(first_topology, vec![(0, 1, Some(1), 1, false)]);

        let batch_id = "batch-after-snapshot";
        let batch_pid = format!("{tenant}:Doc:{batch_id}");
        store
            .append(&batch_pid, 0, &[envelope(&batch_pid, "Created")])
            .await
            .expect("seed batch journal");
        store
            .save_snapshot(&batch_pid, 1, b"snapshot-1")
            .await
            .expect("seal first batch segment");
        store
            .append_batch(&[PersistenceAppend {
                persistence_id: batch_pid.clone(),
                expected_sequence: 1,
                events: vec![envelope(&batch_pid, "Batched")],
                key_rows: Vec::new(),
                reconcile_keys: false,
                key_set_signature: None,
                snapshot_source: SnapshotSourceFence::Exact {
                    sequence_nr: 1,
                    state: b"snapshot-1".to_vec(),
                },
                batch_idempotency: None,
            }])
            .await
            .expect("append batch into open successor");
        let batch_topology: Vec<(i64, i64, Option<i64>, i64, bool)> = sqlx::query_as(
            "SELECT segment_index, start_sequence_nr, end_sequence_nr, event_count, sealed_at IS NOT NULL \
             FROM event_segments WHERE tenant = $1 AND entity_type = 'Doc' AND entity_id = $2 \
             ORDER BY segment_index",
        )
        .bind(&tenant)
        .bind(batch_id)
        .fetch_all(&pool)
        .await
        .expect("load batch topology");
        assert_eq!(
            batch_topology,
            vec![(0, 1, Some(1), 1, true), (1, 2, Some(2), 1, false)]
        );
        let assignments: Vec<(i64, i64)> = sqlx::query_as(
            "SELECT sequence_nr, segment_index FROM events \
             WHERE tenant = $1 AND entity_type = 'Doc' AND entity_id = $2 ORDER BY sequence_nr",
        )
        .bind(&tenant)
        .bind(batch_id)
        .fetch_all(&pool)
        .await
        .expect("load batch assignments");
        assert_eq!(assignments, vec![(1, 0), (2, 1)]);

        let ahead_id = "snapshot-ahead-of-journal";
        let ahead_pid = format!("{tenant}:Doc:{ahead_id}");
        store
            .append(
                &ahead_pid,
                0,
                &[
                    envelope(&ahead_pid, "Created"),
                    envelope(&ahead_pid, "Updated"),
                ],
            )
            .await
            .expect("seed journal below migration snapshot");
        let ahead_snapshot = b"snapshot-5".to_vec();
        store
            .save_snapshot(&ahead_pid, 5, &ahead_snapshot)
            .await
            .expect("save migration snapshot ahead of journal HWM 2");
        let topology_before_legacy_ghost: Vec<SegmentTopologyRow> =
            sqlx::query_as(
                "SELECT segment_index, start_sequence_nr, end_sequence_nr, snapshot_sequence, \
                        event_count, sealed_at IS NOT NULL FROM event_segments \
                 WHERE tenant = $1 AND entity_type = 'Doc' AND entity_id = $2 ORDER BY segment_index",
            )
            .bind(&tenant)
            .bind(ahead_id)
            .fetch_all(&pool)
            .await
            .expect("load topology after snapshot ahead of journal");
        assert_eq!(
            topology_before_legacy_ghost,
            vec![(0, 1, Some(2), None, 2, false)],
            "snapshot sequence 5 must not manufacture a boundary beyond journal HWM 2"
        );

        sqlx::query(
            "INSERT INTO event_segments \
             (tenant, entity_type, entity_id, segment_index, start_sequence_nr, event_count) \
             VALUES ($1, 'Doc', $2, 1, 6, 0)",
        )
        .bind(&tenant)
        .bind(ahead_id)
        .execute(&pool)
        .await
        .expect("seed legacy future-start segment");
        store
            .append_with_index_rows(
                &ahead_pid,
                2,
                &[envelope(&ahead_pid, "AfterMigrationSnapshot")],
                &[],
                &[],
                IndexReconciliation {
                    snapshot_source: SnapshotSourceFence::Exact {
                        sequence_nr: 5,
                        state: ahead_snapshot,
                    },
                    ..IndexReconciliation::default()
                },
            )
            .await
            .expect("append repairs legacy future-start topology");
        let repaired_topology: Vec<SegmentTopologyRow> =
            sqlx::query_as(
                "SELECT segment_index, start_sequence_nr, end_sequence_nr, snapshot_sequence, \
                        event_count, sealed_at IS NOT NULL FROM event_segments \
                 WHERE tenant = $1 AND entity_type = 'Doc' AND entity_id = $2 ORDER BY segment_index",
            )
            .bind(&tenant)
            .bind(ahead_id)
            .fetch_all(&pool)
            .await
            .expect("load repaired topology");
        assert_eq!(repaired_topology, vec![(0, 1, Some(3), None, 3, false)]);
        let repaired_assignments: Vec<(i64, i64)> = sqlx::query_as(
            "SELECT sequence_nr, segment_index FROM events \
             WHERE tenant = $1 AND entity_type = 'Doc' AND entity_id = $2 ORDER BY sequence_nr",
        )
        .bind(&tenant)
        .bind(ahead_id)
        .fetch_all(&pool)
        .await
        .expect("load repaired event assignments");
        assert_eq!(repaired_assignments, vec![(1, 0), (2, 0), (3, 0)]);
    });
}
