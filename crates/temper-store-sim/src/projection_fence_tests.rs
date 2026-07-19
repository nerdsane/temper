use super::*;
use crate::tests::test_envelope;

#[tokio::test]
async fn projection_fence_blocks_live_projection_writes_until_release() {
    let store = SimEventStore::no_faults(42);
    let fence = store
        .acquire_projection_reconciliation_fence("default", "Order")
        .await
        .expect("acquire exclusive projection fence");
    let key_rows = [temper_runtime::persistence::EntityKeyRow {
        key_name: "number".to_string(),
        key_hash: "hash-1".to_string(),
    }];
    let events = [test_envelope(0, "Created")];
    let append = store.append_with_index_rows(
        "default:Order:ord-fenced",
        0,
        &events,
        &key_rows,
        &[],
        temper_runtime::persistence::IndexReconciliation {
            keys: true,
            vectors: false,
        },
    );
    tokio::pin!(append);

    // Poll both futures on this single-threaded test task. The append cannot
    // finish while reconciliation owns the write side; `yield_now` winning is
    // deterministic evidence that the live write is pending on the shared side.
    let remained_blocked = tokio::select! {
        biased;
        result = &mut append => panic!("live append bypassed projection fence: {result:?}"),
        _ = tokio::task::yield_now() => true,
    };
    assert!(remained_blocked);

    drop(fence);
    assert_eq!(append.await.expect("append after fence release"), 1);
    assert_eq!(
        store
            .lookup_by_key("default", "Order", "number", "hash-1")
            .await
            .expect("key lookup")
            .as_deref(),
        Some("ord-fenced")
    );
}

#[tokio::test]
async fn projection_read_fence_hides_exact_reconciliation_purge_window() {
    let store = SimEventStore::no_faults(43);
    let key = temper_runtime::persistence::EntityKeyRow {
        key_name: "number".to_string(),
        key_hash: "hash-final".to_string(),
    };
    store
        .backfill_entity_keys(
            "default",
            "Order",
            "ord-rebuilt",
            std::slice::from_ref(&key),
        )
        .await
        .expect("seed exact key row");

    let reconcile = store
        .acquire_projection_reconciliation_fence("default", "Order")
        .await
        .expect("acquire exclusive reconciliation fence");
    store
        .backfill_entity_keys("default", "Order", "ord-rebuilt", &[])
        .await
        .expect("purge old row inside exact repair");

    let guarded_lookup = async {
        let _read = store
            .acquire_projection_read_fence("default", "Order")
            .await
            .expect("acquire shared projection fence");
        store
            .lookup_by_key("default", "Order", "number", "hash-final")
            .await
    };
    tokio::pin!(guarded_lookup);
    let remained_blocked = tokio::select! {
        biased;
        result = &mut guarded_lookup => panic!("indexed read observed purge window: {result:?}"),
        _ = tokio::task::yield_now() => true,
    };
    assert!(remained_blocked);

    store
        .backfill_entity_keys(
            "default",
            "Order",
            "ord-rebuilt",
            std::slice::from_ref(&key),
        )
        .await
        .expect("restore final exact key row");
    drop(reconcile);
    assert_eq!(
        guarded_lookup
            .await
            .expect("lookup after exact repair")
            .as_deref(),
        Some("ord-rebuilt")
    );
}

#[tokio::test]
async fn exact_key_backfill_rejects_duplicate_live_holder() {
    let store = SimEventStore::no_faults(42);
    let key = temper_runtime::persistence::EntityKeyRow {
        key_name: "number".to_string(),
        key_hash: "duplicate".to_string(),
    };
    store
        .backfill_entity_keys("default", "Order", "first", std::slice::from_ref(&key))
        .await
        .expect("seed first holder");

    let error = store
        .backfill_entity_keys("default", "Order", "second", std::slice::from_ref(&key))
        .await
        .expect_err("duplicate live key must fail exact reconciliation");

    assert!(error.to_string().contains("duplicate declared key"));
    assert_eq!(
        store
            .lookup_by_key("default", "Order", "number", "duplicate")
            .await
            .expect("lookup retained holder")
            .as_deref(),
        Some("first")
    );
}
