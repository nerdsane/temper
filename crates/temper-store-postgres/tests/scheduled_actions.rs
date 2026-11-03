//! Real PostgreSQL: scheduled-action rows commit with the event that arms them
//! and with the event that fires them. Cancellation durably clears the action
//! before cleanup.

use sqlx::postgres::PgPoolOptions;
use temper_runtime::persistence::{
    CommitRows, EventMetadata, EventStore, PersistenceEnvelope, PersistenceError,
    ScheduledActionRecord,
};
use temper_store_postgres::{PostgresEventStore, PostgresSchema, migration};

fn envelope(event_type: &str) -> PersistenceEnvelope {
    PersistenceEnvelope {
        sequence_nr: 0,
        event_type: event_type.into(),
        payload: serde_json::json!({}),
        metadata: EventMetadata {
            event_id: uuid::Uuid::new_v4(),
            causation_id: uuid::Uuid::new_v4(),
            correlation_id: uuid::Uuid::new_v4(),
            timestamp: chrono::Utc::now(),
            actor_id: "scheduled-actions-test".into(),
        },
    }
}

fn row(tenant: &str, arm_id: &str) -> ScheduledActionRecord {
    ScheduledActionRecord {
        tenant: tenant.to_string(),
        entity_type: "Job".to_string(),
        entity_id: "job-1".to_string(),
        arm_id: arm_id.to_string(),
        action: "Fire".to_string(),
        due_at: "2026-01-01T00:10:00+00:00".to_string(),
        armed_by: "{}".to_string(),
    }
}

async fn arm_ids(store: &PostgresEventStore, tenant: &str) -> Vec<String> {
    let mut ids: Vec<String> = store
        .list_scheduled_actions(tenant)
        .await
        .expect("rows readable")
        .into_iter()
        .map(|row| row.arm_id)
        .collect();
    ids.sort();
    ids
}

#[tokio::test]
#[ignore = "requires TEMPER_TEST_POSTGRES_URL pointing to disposable PostgreSQL"]
async fn scheduled_action_rows_commit_with_their_events() {
    let url = std::env::var("TEMPER_TEST_POSTGRES_URL").expect("disposable PostgreSQL URL");
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .expect("connect");
    let name = format!("scheduled_{}", uuid::Uuid::new_v4().simple());
    let schema = PostgresSchema::new(&name).expect("schema name");
    migration::run_migrations_in_schema(&pool, &schema)
        .await
        .expect("migrate");
    let store = PostgresEventStore::with_schema(pool.clone(), schema);
    let armed = [row("tenant-a", "1.0"), row("tenant-a", "1.1")];

    // Arming: the rows land with the event, and only in their tenant.
    store
        .append_with_rows(
            "tenant-a:Job:job-1",
            0,
            &[envelope("Start")],
            CommitRows {
                scheduled_actions: &armed,
                ..CommitRows::default()
            },
        )
        .await
        .expect("arming append");
    store
        .append_with_rows(
            "tenant-b:Job:job-1",
            0,
            &[envelope("Start")],
            CommitRows {
                scheduled_actions: &[row("tenant-b", "1.0")],
                ..CommitRows::default()
            },
        )
        .await
        .expect("other tenant's append");
    let rows = store.list_scheduled_actions("tenant-a").await.unwrap();
    assert_eq!(rows.iter().find(|r| r.arm_id == "1.0"), Some(&armed[0]));
    assert_eq!(arm_ids(&store, "tenant-a").await, ["1.0", "1.1"]);
    assert_eq!(arm_ids(&store, "tenant-b").await, ["1.0"]);

    // A refused append writes no row and removes none.
    let refused = store
        .append_with_rows(
            "tenant-a:Job:job-1",
            0,
            &[envelope("Fire")],
            CommitRows {
                scheduled_actions: &[row("tenant-a", "9.0")],
                fired_scheduled_action: Some("1.0"),
                ..CommitRows::default()
            },
        )
        .await;
    assert!(matches!(
        refused,
        Err(PersistenceError::ConcurrencyViolation { .. })
    ));
    assert_eq!(arm_ids(&store, "tenant-a").await, ["1.0", "1.1"]);

    // Firing: the event removes its own row in the same commit, and not
    // another tenant's row with the same arm id.
    store
        .append_with_rows(
            "tenant-a:Job:job-1",
            1,
            &[envelope("Fire")],
            CommitRows {
                fired_scheduled_action: Some("1.0"),
                ..CommitRows::default()
            },
        )
        .await
        .expect("firing append");
    assert_eq!(arm_ids(&store, "tenant-a").await, ["1.1"]);
    assert_eq!(arm_ids(&store, "tenant-b").await, ["1.0"]);

    // A refused call is durably cancelled before deletion. Repeating the
    // cancellation, including after deletion, is harmless and tenant-scoped.
    for _ in 0..2 {
        store
            .cancel_scheduled_action("tenant-a", "Job", "job-1", "1.1")
            .await
            .expect("cancellation");
    }
    let cancelled = store.list_scheduled_actions("tenant-a").await.unwrap();
    assert_eq!(cancelled.len(), 1);
    assert!(
        cancelled[0].action.is_empty(),
        "a restart sees terminal disposition"
    );
    assert_eq!(
        store.list_scheduled_actions("tenant-b").await.unwrap()[0].action,
        "Fire"
    );

    store
        .cancel_scheduled_action("tenant-a", "Job", "missing", "1.1")
        .await
        .expect("absent cancellation");
    // A removal outside a commit takes one row, and repeating it is a no-op.
    for _ in 0..2 {
        store
            .delete_scheduled_action("tenant-a", "Job", "job-1", "1.1")
            .await
            .expect("removal");
    }
    store
        .cancel_scheduled_action("tenant-a", "Job", "job-1", "1.1")
        .await
        .expect("cancellation after deletion");
    let left = arm_ids(&store, "tenant-a").await;
    let other = arm_ids(&store, "tenant-b").await;

    sqlx::query(&format!("DROP SCHEMA \"{name}\" CASCADE"))
        .execute(&pool)
        .await
        .expect("drop schema");
    pool.close().await;
    assert!(left.is_empty());
    assert_eq!(other, ["1.0"]);
}
