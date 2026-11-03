//! A journal page is the newest events below a sequence, oldest first, and
//! only from its own journal.

use temper_runtime::persistence::EventStore;

use super::{TursoEventStore, make_store, test_envelope};

async fn page(
    store: &TursoEventStore,
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
async fn read_events_page_reads_newest_first_windows() {
    let store = make_store("event-pages").await;
    let pid = "tenant-a:Order:paged";
    assert!(page(&store, pid, None, 3).await.is_empty());
    let events: Vec<_> = (0..7)
        .map(|i| test_envelope("Moved", serde_json::json!({ "i": i })))
        .collect();
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
