//! Redis snapshot replacement and concurrent-writer durability regressions.

use super::*;

fn redis_url() -> Option<String> {
    std::env::var("REDIS_URL").ok()
}

fn unique_persistence_id() -> String {
    let id = uuid::Uuid::new_v4();
    format!("test-{id}:Order:ord-{id}")
}

async fn make_store() -> Option<RedisEventStore> {
    let url = redis_url()?;
    Some(
        RedisEventStore::new(&url)
            .await
            .expect("failed to connect to Redis"),
    )
}

#[tokio::test]
async fn noncanonical_snapshot_wrapper_remains_repairable() {
    let Some(store) = make_store().await else {
        eprintln!("REDIS_URL not set, skipping test");
        return;
    };
    let pid = unique_persistence_id();
    let (tenant, entity_type, entity_id) = parse_persistence_id_parts(&pid).unwrap();
    let snapshot_key = RedisEventStore::snapshot_key(tenant, entity_type, entity_id);
    let original = b"{\"status\":\"created\"}";
    let upgraded = b"{\"status\":\"created-upgraded\"}";
    let noncanonical_record = format!(
        "{{\n  \"snapshot\": {},\n  \"future_field\": true,\n  \"sequence_nr\": 5\n}}",
        serde_json::to_string(&original.to_vec()).unwrap()
    );
    let _: () = store
        .client
        .set(&snapshot_key, noncanonical_record, None, None, false)
        .await
        .unwrap();

    store
        .replace_snapshot(&pid, 5, original, upgraded)
        .await
        .expect("semantic compare must accept a valid noncanonical snapshot wrapper");
    assert_eq!(
        store.load_snapshot(&pid).await.unwrap(),
        Some((5, upgraded.to_vec()))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_snapshot_saves_publish_one_complete_current_and_history_pair() {
    let Some(store) = make_store().await else {
        eprintln!("REDIS_URL not set, skipping test");
        return;
    };

    for iteration in 0..8_u64 {
        let pid = unique_persistence_id();
        let original = format!("original-{iteration}").into_bytes();
        let large_snapshot = vec![u8::try_from(iteration).unwrap_or_default(); 1024 * 1024];
        let winner = format!("winner-{iteration}").into_bytes();
        store.save_snapshot(&pid, 5, &original).await.unwrap();

        let (tenant, entity_type, entity_id) = parse_persistence_id_parts(&pid).unwrap();
        let snapshot_key = RedisEventStore::snapshot_key(tenant, entity_type, entity_id);
        let large_store = store.clone();
        let large_pid = pid.clone();
        let large_write = tokio::spawn(async move {
            large_store
                .save_snapshot(&large_pid, 6, &large_snapshot)
                .await
        });

        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                let prefix: String = store.client.getrange(&snapshot_key, 0, 63).await.unwrap();
                if prefix.contains("\"sequence_nr\":6") {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("large snapshot current record becomes observable");

        store
            .save_snapshot(&pid, 6, &winner)
            .await
            .expect("publish the winning concurrent snapshot");
        large_write
            .await
            .expect("large snapshot task")
            .expect("large snapshot write");

        let (_, current) = store
            .load_snapshot(&pid)
            .await
            .unwrap()
            .expect("current snapshot");
        let history_key = RedisEventStore::snapshot_history_key(tenant, entity_type, entity_id, 6);
        let history: String = store.client.get(&history_key).await.unwrap();
        let history: SnapshotHistoryRecord = serde_json::from_str(&history).unwrap();
        assert_eq!(current, winner, "later snapshot writer owns current state");
        assert_eq!(history.snapshot, winner, "history must match current state");
    }
}
