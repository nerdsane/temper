//! Real PostgreSQL: a journal page is the newest events below a sequence,
//! oldest first, and only from its own journal.

use sqlx::postgres::PgPoolOptions;
use temper_runtime::persistence::{EventMetadata, EventStore, PersistenceEnvelope};
use temper_store_postgres::{PostgresEventStore, PostgresSchema, migration};

fn envelope(i: u64) -> PersistenceEnvelope {
    PersistenceEnvelope {
        sequence_nr: 0,
        event_type: "Moved".into(),
        payload: serde_json::json!({ "i": i }),
        metadata: EventMetadata {
            event_id: uuid::Uuid::new_v4(),
            causation_id: uuid::Uuid::new_v4(),
            correlation_id: uuid::Uuid::new_v4(),
            timestamp: chrono::Utc::now(),
            actor_id: "event-pages-test".into(),
        },
    }
}

async fn page(
    store: &PostgresEventStore,
    persistence_id: &str,
    before: Option<u64>,
    limit: usize,
) -> Vec<u64> {
    store
        .read_events_page(persistence_id, before, limit)
        .await
        .expect("page readable")
        .iter()
        .map(|envelope| envelope.sequence_nr)
        .collect()
}

#[tokio::test]
#[ignore = "requires TEMPER_TEST_POSTGRES_URL pointing to disposable PostgreSQL"]
async fn read_events_page_reads_newest_first_windows() {
    let url = std::env::var("TEMPER_TEST_POSTGRES_URL").expect("disposable PostgreSQL URL");
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .expect("connect");
    let name = format!("event_pages_{}", uuid::Uuid::new_v4().simple());
    let schema = PostgresSchema::new(&name).expect("schema name");
    migration::run_migrations_in_schema(&pool, &schema)
        .await
        .expect("migrate");
    let store = PostgresEventStore::with_schema(pool, schema);

    let pid = "tenant-a:Order:paged";
    assert!(page(&store, pid, None, 3).await.is_empty());
    let events: Vec<_> = (0..7).map(envelope).collect();
    store.append(pid, 0, &events).await.expect("append");
    // Another journal of the same type and another tenant stay out of the page.
    store
        .append("tenant-a:Order:other", 0, &events[..2])
        .await
        .expect("append other entity");
    store
        .append("tenant-b:Order:paged", 0, &events[..2])
        .await
        .expect("append other tenant");

    assert_eq!(page(&store, pid, None, 3).await, [5, 6, 7]);
    assert_eq!(page(&store, pid, Some(5), 3).await, [2, 3, 4]);
    assert_eq!(page(&store, pid, Some(2), 3).await, [1]);
    assert!(page(&store, pid, Some(1), 3).await.is_empty());
    assert_eq!(page(&store, pid, None, 100).await, [1, 2, 3, 4, 5, 6, 7]);
    assert_eq!(page(&store, pid, Some(100), 2).await, [6, 7]);
    assert_eq!(
        page(&store, pid, Some(u64::MAX), usize::MAX).await,
        [1, 2, 3, 4, 5, 6, 7],
        "bounds above i64::MAX mean no bound"
    );

    let newest = store
        .read_events_page(pid, None, 1)
        .await
        .expect("page readable");
    assert_eq!(newest[0].payload["i"], 6, "payloads decode with their rows");
}
