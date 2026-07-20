use super::*;

#[tokio::test]
async fn snapshot_save_and_load_roundtrip() {
    let store = make_store("snapshot").await;
    let persistence_id = "tenant-a:Order:ord-3";

    store
        .save_snapshot(persistence_id, 5, b"{\"status\":\"created\"}")
        .await
        .unwrap();

    let snapshot = store.load_snapshot(persistence_id).await.unwrap();
    assert_eq!(snapshot, Some((5, b"{\"status\":\"created\"}".to_vec())));

    let conn = store.configured_connection().await.unwrap();
    let mut segment_rows = conn
        .query(
            "SELECT segment_index, start_sequence_nr, end_sequence_nr, \
                    snapshot_sequence, event_count \
             FROM event_segments \
             WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3 \
             ORDER BY segment_index",
            params!["tenant-a", "Order", "ord-3"],
        )
        .await
        .unwrap();
    let mut segments_before = Vec::new();
    while let Some(row) = segment_rows.next().await.unwrap() {
        segments_before.push((
            row.get::<i64>(0).unwrap(),
            row.get::<i64>(1).unwrap(),
            row.get::<Option<i64>>(2).unwrap(),
            row.get::<Option<i64>>(3).unwrap(),
            row.get::<i64>(4).unwrap(),
        ));
    }
    drop(segment_rows);

    store
        .replace_snapshot(
            persistence_id,
            5,
            b"{\"status\":\"created\"}",
            b"{\"status\":\"created-upgraded\"}",
        )
        .await
        .unwrap();

    let replacement = store.load_snapshot(persistence_id).await.unwrap();
    assert_eq!(
        replacement,
        Some((5, b"{\"status\":\"created-upgraded\"}".to_vec()))
    );

    let stale_replacement = store
        .replace_snapshot(
            persistence_id,
            5,
            b"{\"status\":\"created\"}",
            b"{\"status\":\"stale-overwrite\"}",
        )
        .await
        .expect_err("a stale same-boundary writer must lose");
    assert!(matches!(stale_replacement, PersistenceError::Storage(_)));
    assert_eq!(
        store.load_snapshot(persistence_id).await.unwrap(),
        Some((5, b"{\"status\":\"created-upgraded\"}".to_vec()))
    );
    let mut history_rows = conn
        .query(
            "SELECT snapshot FROM snapshot_history \
             WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3 AND sequence_nr = 5",
            params!["tenant-a", "Order", "ord-3"],
        )
        .await
        .unwrap();
    let history = history_rows
        .next()
        .await
        .unwrap()
        .expect("replacement history row")
        .get::<Vec<u8>>(0)
        .unwrap();
    assert_eq!(history, b"{\"status\":\"created-upgraded\"}");
    drop(history_rows);
    let mut segment_rows = conn
        .query(
            "SELECT segment_index, start_sequence_nr, end_sequence_nr, \
                    snapshot_sequence, event_count \
             FROM event_segments \
             WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3 \
             ORDER BY segment_index",
            params!["tenant-a", "Order", "ord-3"],
        )
        .await
        .unwrap();
    let mut segments_after = Vec::new();
    while let Some(row) = segment_rows.next().await.unwrap() {
        segments_after.push((
            row.get::<i64>(0).unwrap(),
            row.get::<i64>(1).unwrap(),
            row.get::<Option<i64>>(2).unwrap(),
            row.get::<Option<i64>>(3).unwrap(),
            row.get::<i64>(4).unwrap(),
        ));
    }
    assert_eq!(segments_after, segments_before);

    store
        .save_snapshot(persistence_id, 8, b"{\"status\":\"shipped\"}")
        .await
        .unwrap();

    let updated = store.load_snapshot(persistence_id).await.unwrap();
    assert_eq!(updated, Some((8, b"{\"status\":\"shipped\"}".to_vec())));
}

#[tokio::test]
async fn snapshot_replacement_is_readiness_priority() {
    let mut store = make_store("snapshot-replacement-priority").await;
    let persistence_id = "tenant-a:Order:ord-readiness";
    store
        .save_snapshot(persistence_id, 1, b"{\"status\":\"legacy\"}")
        .await
        .expect("seed legacy snapshot");

    store.write_gate = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
    let held_gate = store
        .write_gate
        .clone()
        .acquire_owned()
        .await
        .expect("hold the only write lane");
    let replacement_store = store.clone();
    let replacement = tokio::spawn(async move {
        replacement_store
            .replace_snapshot(
                persistence_id,
                1,
                b"{\"status\":\"legacy\"}",
                b"{\"status\":\"legacy-repaired\"}",
            )
            .await
    });

    for _ in 0..32 {
        if store
            .high_priority_write_waiters
            .load(std::sync::atomic::Ordering::Acquire)
            == 1
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(
        store
            .high_priority_write_waiters
            .load(std::sync::atomic::Ordering::Acquire),
        1,
        "snapshot replacement blocks actor readiness and must queue ahead of background writes"
    );

    drop(held_gate);
    replacement
        .await
        .expect("replacement task completes")
        .expect("snapshot replacement succeeds");
    assert_eq!(
        store
            .load_snapshot(persistence_id)
            .await
            .expect("load repaired snapshot"),
        Some((1, b"{\"status\":\"legacy-repaired\"}".to_vec()))
    );
}
