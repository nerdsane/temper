//! The Redis batch append moves every journal or none.

use temper_runtime::persistence::{EventMetadata, EventStore, PersistenceEnvelope};

use super::*;

async fn make_store() -> Option<RedisEventStore> {
    let url = std::env::var("REDIS_URL").ok()?;
    Some(
        RedisEventStore::new(&url)
            .await
            .expect("failed to connect to Redis"),
    )
}

fn envelope(event_type: &str) -> PersistenceEnvelope {
    PersistenceEnvelope {
        sequence_nr: 0,
        event_type: event_type.to_string(),
        payload: serde_json::json!({}),
        metadata: EventMetadata {
            event_id: uuid::Uuid::new_v4(),
            causation_id: uuid::Uuid::new_v4(),
            correlation_id: uuid::Uuid::new_v4(),
            timestamp: chrono::Utc::now(),
            actor_id: "redis-batch-test".to_string(),
        },
    }
}

fn append(persistence_id: &str, expected_sequence: u64) -> PersistenceAppend {
    PersistenceAppend {
        persistence_id: persistence_id.to_string(),
        expected_sequence,
        events: vec![envelope("Deleted")],
        state_timer: None,
        projection: None,
    }
}

#[tokio::test]
async fn every_journal_moves_or_none_does() {
    let Some(store) = make_store().await else {
        eprintln!("REDIS_URL not set, skipping test");
        return;
    };
    let tenant = format!("test-{}", uuid::Uuid::new_v4());
    let ids = ["t-1", "t-2", "t-3"].map(|id| format!("{tenant}:Track:{id}"));
    for id in &ids {
        store.append(id, 0, &[envelope("Created")]).await.unwrap();
    }
    // t-2 moves on after it was read at sequence 1.
    store
        .append(&ids[1], 1, &[envelope("Moved")])
        .await
        .unwrap();

    let stale = [append(&ids[0], 1), append(&ids[1], 1), append(&ids[2], 1)];
    let refused = store.append_batch(&stale).await;
    assert!(
        matches!(
            refused,
            Err(PersistenceError::ConcurrencyViolation {
                expected: 1,
                actual: 2
            })
        ),
        "a journal past its expected sequence refuses the batch: {refused:?}"
    );
    for (id, length) in ids.iter().zip([1, 2, 1]) {
        assert_eq!(store.read_events(id, 0).await.unwrap().len(), length);
    }

    let current = [append(&ids[0], 1), append(&ids[1], 2), append(&ids[2], 1)];
    let results = store.append_batch(&current).await.unwrap();
    assert_eq!(
        results.iter().map(|r| r.sequence_nr).collect::<Vec<_>>(),
        vec![2, 3, 2]
    );
    for (id, length) in ids.iter().zip([2, 3, 2]) {
        let events = store.read_events(id, 0).await.unwrap();
        assert_eq!(events.len(), length);
        assert_eq!(events.last().unwrap().event_type, "Deleted");
        assert_eq!(events.last().unwrap().sequence_nr, length as u64);
    }
}
