//! The delta is co-committed, sequence guarded, durable, and tenant scoped.
//!
//! Gated on DATABASE_URL (skips without it); each test works in its own schema.
use sqlx::PgPool;
use temper_runtime::persistence::{
    EventMetadata, EventStore, PersistenceAppend, PersistenceEnvelope,
};

use crate::migration::run_migrations_in_schema;
use crate::{PostgresEventStore, PostgresSchema};

const MIGRATION: &str = include_str!("../migrations/0023_entity_projection_delta.sql");

fn envelope(event_type: &str, payload: serde_json::Value) -> PersistenceEnvelope {
    PersistenceEnvelope {
        sequence_nr: 0,
        event_type: event_type.to_string(),
        payload,
        metadata: EventMetadata {
            event_id: uuid::Uuid::new_v4(),
            causation_id: uuid::Uuid::new_v4(),
            correlation_id: uuid::Uuid::new_v4(),
            timestamp: chrono::Utc::now(),
            actor_id: "projection-delta-test".to_string(),
        },
    }
}

fn created() -> PersistenceEnvelope {
    envelope("Create", serde_json::json!({}))
}

fn tombstone() -> PersistenceEnvelope {
    envelope(
        "Deleted",
        serde_json::json!({
            "action": "Deleted",
            "from_status": "Draft",
            "to_status": "Deleted",
            "params": {},
            "kind": "tombstone"
        }),
    )
}

/// A store over a fresh, fully migrated schema of its own.
async fn isolated_store(url: &str) -> (PgPool, PostgresSchema, PostgresEventStore) {
    let pool = PgPool::connect(url).await.unwrap();
    let name = format!("delta_{}", uuid::Uuid::new_v4().simple());
    let schema = PostgresSchema::new(name).unwrap();
    run_migrations_in_schema(&pool, &schema).await.unwrap();
    let store = PostgresEventStore::with_schema(pool.clone(), schema.clone());
    (pool, schema, store)
}

async fn drop_schema(pool: &PgPool, schema: &PostgresSchema) {
    sqlx::query(&format!(
        "DROP SCHEMA {} CASCADE",
        schema.prefix().trim_end_matches('.')
    ))
    .execute(pool)
    .await
    .unwrap();
}

async fn candidates(store: &PostgresEventStore, tenant: &str) -> (Vec<String>, Vec<String>) {
    store
        .reconciled_equality_candidates_pg(
            tenant,
            "Payment",
            &[("app".into(), "needle".into())],
            Some(100),
        )
        .await
        .unwrap()
}

async fn project(store: &PostgresEventStore, id: &str, app: &str, sequence: u64) {
    store
        .upsert_query_projection(
            "t",
            "Payment",
            id,
            "Draft",
            &serde_json::json!({"Id": id, "app": app}),
            sequence,
        )
        .await
        .unwrap();
}

async fn create(store: &PostgresEventStore, id: &str) {
    store
        .append(&format!("t:Payment:{id}"), 0, &[created()])
        .await
        .unwrap();
}

/// The migration's backfill reads tombstones with the store's own predicate.
#[test]
fn backfill_tombstone_predicate_is_the_store_predicate() {
    let words = |sql: &str| sql.split_whitespace().collect::<Vec<_>>().join(" ");
    let predicate = words(crate::schema::tombstone_sql!());
    assert!(
        words(MIGRATION).contains(&predicate),
        "migration 0023 must read tombstones exactly as tombstone_sql!(): {predicate}"
    );
}

#[test]
fn projection_delta_tracks_commits_and_only_retires_applied_sequences() {
    let Some(url) = temper_test_resource::env_var("DATABASE_URL") else {
        return;
    };
    sqlx::test_block_on(async {
        let (pool, schema, store) = isolated_store(&url).await;
        for id in ["changed", "match", "missing", "noise"] {
            create(&store, id).await;
        }
        project(&store, "changed", "old", 1).await;
        project(&store, "match", "needle", 1).await;
        project(&store, "noise", "other", 1).await;
        // A committed entity whose projection has not landed is a candidate.
        assert_eq!(
            candidates(&store, "t").await,
            (
                vec!["match".into(), "missing".into()],
                vec!["missing".into()]
            )
        );
        store
            .append(
                "t:Payment:changed",
                1,
                &[envelope("Update", serde_json::json!({}))],
            )
            .await
            .unwrap();
        // A projection arriving late must not erase a newer commit's mark.
        project(&store, "changed", "old", 1).await;
        assert_eq!(candidates(&store, "t").await.1, vec!["changed", "missing"]);
        project(&store, "changed", "needle", 2).await;
        assert_eq!(
            candidates(&store, "t").await.0,
            vec!["changed", "match", "missing"]
        );
        assert_eq!(candidates(&store, "t").await.1, vec!["missing"]);
        // An unchanged projection rewrites only the catalog row, and still
        // retires the mark.
        store
            .append(
                "t:Payment:changed",
                2,
                &[envelope("Touch", serde_json::json!({}))],
            )
            .await
            .unwrap();
        assert_eq!(candidates(&store, "t").await.1, vec!["changed", "missing"]);
        project(&store, "changed", "needle", 3).await;
        assert_eq!(candidates(&store, "t").await.1, vec!["missing"]);
        assert!(candidates(&store, "other").await.0.is_empty());
        // An optimistic concurrency rejection must not leave a phantom mark.
        assert!(
            store
                .append(
                    "t:Payment:noise",
                    0,
                    &[envelope("Touch", serde_json::json!({}))]
                )
                .await
                .is_err()
        );
        assert_eq!(candidates(&store, "t").await.1, vec!["missing"]);
        // Pending ids must be prioritized even when their id sorts after all
        // projected matches. Otherwise an empty bounded gap could falsely vouch
        // for a broader native page.
        let bounded = store
            .reconciled_equality_candidates_pg(
                "t",
                "Payment",
                &[("app".into(), "needle".into())],
                Some(1),
            )
            .await
            .unwrap();
        assert_eq!(bounded.0.len(), 2);
        assert_eq!(bounded.1, vec!["missing"]);
        let complete = store
            .reconciled_equality_candidates_pg(
                "t",
                "Payment",
                &[("app".into(), "needle".into())],
                None,
            )
            .await
            .unwrap();
        assert_eq!(complete.0, vec!["changed", "match", "missing"]);
        assert_eq!(complete.1, vec!["missing"]);
        // Every equality must hold: the projected matches are an intersection.
        let both = store
            .reconciled_equality_candidates_pg(
                "t",
                "Payment",
                &[
                    ("app".into(), "needle".into()),
                    ("Id".into(), "match".into()),
                ],
                None,
            )
            .await
            .unwrap();
        assert_eq!(both.0, vec!["match", "missing"]);
        drop_schema(&pool, &schema).await;
    });
}

#[test]
fn projection_delta_covers_batch_snapshot_wrapped_removed_and_native_rows() {
    let Some(url) = temper_test_resource::env_var("DATABASE_URL") else {
        return;
    };
    sqlx::test_block_on(async {
        let (pool, schema, store) = isolated_store(&url).await;
        store
            .append_batch(&[PersistenceAppend {
                persistence_id: "t:Payment:batch".into(),
                expected_sequence: 0,
                events: vec![created()],
                state_timer: None,
                projection: None,
            }])
            .await
            .unwrap();
        store
            .save_snapshot("t:Payment:snapshot", 7, b"{}")
            .await
            .unwrap();
        store
            .upsert_query_projection(
                "t",
                "Payment",
                "wrapped",
                "Draft",
                &serde_json::json!({"fields": {"app": "needle"}}),
                1,
            )
            .await
            .unwrap();
        project(&store, "removed", "other", 1).await;
        store
            .remove_query_projection("t", "Payment", "removed")
            .await
            .unwrap();
        // A create that commits its projection with its event is not behind.
        let mut first = created();
        first.sequence_nr = 1;
        let native_fields = serde_json::json!({"Id": "native", "app": "needle"});
        let native_state = serde_json::json!({"status": "Draft", "sequence_nr": 1});
        store
            .append_with_rows(
                "t:Payment:native",
                0,
                &[first],
                temper_runtime::persistence::CommitRows {
                    projection: Some(temper_runtime::persistence::ProjectionRow::Upsert {
                        status: "Draft",
                        fields: &native_fields,
                        state: &native_state,
                    }),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(
            candidates(&store, "t").await,
            (
                vec![
                    "batch".into(),
                    "native".into(),
                    "removed".into(),
                    "snapshot".into(),
                    "wrapped".into()
                ],
                vec![
                    "batch".into(),
                    "removed".into(),
                    "snapshot".into(),
                    "wrapped".into()
                ]
            )
        );
        project(&store, "snapshot", "other", 7).await;
        store
            .save_snapshot("t:Payment:snapshot", 7, b"{}")
            .await
            .unwrap();
        assert_eq!(
            candidates(&store, "t").await.1,
            vec!["batch", "removed", "wrapped"]
        );
        drop_schema(&pool, &schema).await;
    });
}

#[test]
fn projection_delta_backfill_marks_every_unvouched_row_once() {
    let Some(url) = temper_test_resource::env_var("DATABASE_URL") else {
        return;
    };
    sqlx::test_block_on(async {
        let (pool, schema, store) = isolated_store(&url).await;
        for id in [
            "behind",
            "current",
            "gone",
            "missing",
            "unindexed",
            "wrapped",
        ] {
            create(&store, id).await;
            if id != "missing" {
                project(&store, id, if id == "gone" { "needle" } else { "other" }, 1).await;
            }
        }
        store
            .append(
                "t:Payment:behind",
                1,
                &[envelope("Touch", serde_json::json!({}))],
            )
            .await
            .unwrap();
        store
            .append("t:Payment:gone", 1, &[tombstone()])
            .await
            .unwrap();
        store
            .upsert_query_projection(
                "t",
                "Payment",
                "wrapped",
                "Draft",
                &serde_json::json!({"fields": {"app": "needle"}}),
                1,
            )
            .await
            .unwrap();
        // Recreate a database from before the delta, including a row from
        // before the field index, and migrate it again.
        for sql in [
            "DROP TABLE {schema}entity_projection_delta",
            "DELETE FROM {schema}entity_field_index WHERE entity_id = 'unindexed'",
            "DELETE FROM {schema}_sqlx_migrations WHERE version = 23",
        ] {
            crate::dbm::postgres_query!(&schema, sql)
                .execute(&pool)
                .await
                .unwrap();
        }
        run_migrations_in_schema(&pool, &schema).await.unwrap();
        let (ids, pending) = candidates(&store, "t").await;
        assert_eq!(pending, vec!["behind", "missing", "unindexed", "wrapped"]);
        assert!(
            !ids.contains(&"gone".to_string()),
            "a deleted entity's stale index row must not match"
        );
        project(&store, "behind", "other", 2).await;
        project(&store, "unindexed", "other", 1).await;
        assert_eq!(
            candidates(&store, "t").await.1,
            vec!["missing", "unindexed", "wrapped"],
            "an unchanged projection must not vouch for a missing legacy index"
        );
        drop_schema(&pool, &schema).await;
    });
}

#[test]
fn projection_delta_never_resurrects_a_tombstone_or_scans_deleted_rows() {
    let Some(url) = temper_test_resource::env_var("DATABASE_URL") else {
        return;
    };
    sqlx::test_block_on(async {
        let (pool, schema, store) = isolated_store(&url).await;
        create(&store, "gone").await;
        project(&store, "gone", "needle", 1).await;
        store
            .append("t:Payment:gone", 1, &[tombstone()])
            .await
            .unwrap();
        assert_eq!(
            candidates(&store, "t").await,
            (Vec::new(), Vec::new()),
            "the stale index must not return a deleted row"
        );
        store
            .remove_query_projection("t", "Payment", "gone")
            .await
            .unwrap();
        // A queued projection can arrive after removal; the terminal marker
        // survives it and is excluded through the pending index, not hydrated
        // on each read.
        project(&store, "gone", "needle", 1).await;
        assert_eq!(candidates(&store, "t").await, (Vec::new(), Vec::new()));
        drop_schema(&pool, &schema).await;
    });
}

mod snapshot_test;
