//! Focused Redis event-store regression group.

use super::*;

#[tokio::test]
async fn committed_append_survives_corrupt_advisory_segment_metadata() {
    let Some(store) = make_store().await else {
        eprintln!("REDIS_URL not set, skipping test");
        return;
    };
    let pid = unique_persistence_id();
    store
        .append(&pid, 0, &[test_envelope("Created", serde_json::json!({}))])
        .await
        .expect("seed journal");
    let (tenant, entity_type, entity_id) = parse_persistence_id_parts(&pid).unwrap();
    let corrupt_key = RedisEventStore::segment_key(tenant, entity_type, entity_id, 0);
    let _: () = store
        .client
        .set(&corrupt_key, "not-json", None, None, false)
        .await
        .expect("corrupt advisory metadata");

    assert_eq!(
        store
            .append(&pid, 1, &[test_envelope("Updated", serde_json::json!({}))],)
            .await
            .expect("journal success is authoritative"),
        2
    );
    assert_eq!(
        store
            .read_events_with_head(&pid, 0)
            .await
            .expect("read committed journal")
            .journal_head_sequence_nr,
        2
    );
}

#[tokio::test]
async fn concurrent_appends_detect_conflict() {
    let Some(store) = make_store().await else {
        eprintln!("REDIS_URL not set, skipping test");
        return;
    };
    let pid = unique_persistence_id();

    let store1 = store.clone();
    let store2 = store.clone();
    let pid1 = pid.clone();
    let pid2 = pid.clone();

    let handle1 = tokio::spawn(async move {
        store1
            .append(
                &pid1,
                0,
                &[test_envelope(
                    "OrderCreated",
                    serde_json::json!({ "writer": 1 }),
                )],
            )
            .await
    });

    let handle2 = tokio::spawn(async move {
        store2
            .append(
                &pid2,
                0,
                &[test_envelope(
                    "OrderCreated",
                    serde_json::json!({ "writer": 2 }),
                )],
            )
            .await
    });

    let (r1, r2) = tokio::join!(handle1, handle2);
    let r1 = r1.unwrap();
    let r2 = r2.unwrap();

    // Exactly one should succeed, the other should get a ConcurrencyViolation.
    let successes = [r1.is_ok(), r2.is_ok()].iter().filter(|&&ok| ok).count();
    let conflicts = [&r1, &r2]
        .iter()
        .filter(|r| matches!(r, Err(PersistenceError::ConcurrencyViolation { .. })))
        .count();

    assert_eq!(successes, 1, "exactly one writer should succeed");
    assert_eq!(conflicts, 1, "exactly one writer should see a conflict");
}
