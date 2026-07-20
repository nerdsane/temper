use super::*;
use temper_runtime::persistence::{EntityKeyRow, KeyIndexBackfillFence};

#[tokio::test]
async fn append_batch_reconciles_keys_and_rolls_back_conflicting_claims() {
    let store = SimEventStore::no_faults(42);
    let owner_pid = "default:Doc:doc-owner";
    let claimant_pid = "default:Doc:doc-claimant";
    let claimed_key = EntityKeyRow {
        key_name: "path".to_string(),
        key_hash: "shared-path".to_string(),
    };

    store
        .append_with_keys(
            owner_pid,
            0,
            &[test_envelope(0, "Created")],
            std::slice::from_ref(&claimed_key),
        )
        .await
        .unwrap();

    store
        .append_batch(&[
            PersistenceAppend {
                persistence_id: owner_pid.to_string(),
                expected_sequence: 1,
                events: vec![test_envelope(0, "Deleted")],
                key_rows: Vec::new(),
                reconcile_keys: true,
                key_set_signature: Some("v3:path".to_string()),
            },
            PersistenceAppend {
                persistence_id: claimant_pid.to_string(),
                expected_sequence: 0,
                events: vec![test_envelope(0, "Created")],
                key_rows: vec![claimed_key.clone()],
                reconcile_keys: true,
                key_set_signature: Some("v3:path".to_string()),
            },
        ])
        .await
        .expect("one atomic batch may release and reclaim the same key");
    assert_eq!(
        store
            .lookup_by_key("default", "Doc", "path", &claimed_key.key_hash)
            .await
            .unwrap(),
        Some("doc-claimant".to_string())
    );
    assert_eq!(store.dump_journal(owner_pid).len(), 2);
    assert_eq!(store.dump_journal(claimant_pid).len(), 1);

    let unrelated_key = EntityKeyRow {
        key_name: "path".to_string(),
        key_hash: "unrelated-path".to_string(),
    };
    let rejected = store
        .append_batch(&[
            PersistenceAppend {
                persistence_id: "default:Doc:doc-unrelated".to_string(),
                expected_sequence: 0,
                events: vec![test_envelope(0, "Created")],
                key_rows: vec![unrelated_key.clone()],
                reconcile_keys: true,
                key_set_signature: Some("v3:path".to_string()),
            },
            PersistenceAppend {
                persistence_id: "default:Doc:doc-conflict".to_string(),
                expected_sequence: 0,
                events: vec![test_envelope(0, "Created")],
                key_rows: vec![claimed_key.clone()],
                reconcile_keys: true,
                key_set_signature: Some("v3:path".to_string()),
            },
        ])
        .await;
    assert!(
        rejected.is_err(),
        "an occupied final key must reject the batch"
    );
    assert!(
        store.dump_journal("default:Doc:doc-unrelated").is_empty(),
        "a later key conflict must roll back an earlier stream"
    );
    assert!(store.dump_journal("default:Doc:doc-conflict").is_empty());
    assert_eq!(
        store
            .lookup_by_key("default", "Doc", "path", &unrelated_key.key_hash)
            .await
            .unwrap(),
        None,
        "the rejected batch must not publish any key rows"
    );
    assert_eq!(
        store
            .lookup_by_key("default", "Doc", "path", &claimed_key.key_hash)
            .await
            .unwrap(),
        Some("doc-claimant".to_string()),
        "the prior owner must survive a rejected batch"
    );
}

#[tokio::test]
async fn key_reconciliation_includes_snapshot_and_key_only_owners() {
    let store = SimEventStore::no_faults(42);
    let orphan = EntityKeyRow {
        key_name: "path".to_string(),
        key_hash: "orphan-path".to_string(),
    };
    let repair_signature = "v3:path";
    let repair_revision = store
        .begin_key_index_backfill("default", "Doc", repair_signature)
        .await
        .expect("begin orphan repair contract");
    store
        .backfill_entity_keys(
            "default",
            "Doc",
            "orphan-owner",
            0,
            KeyIndexBackfillFence {
                key_set_signature: repair_signature,
                contract_revision: repair_revision,
                expected_journal_sequence: 0,
                expected_entity_live: false,
            },
            &[orphan],
        )
        .await
        .expect("seed key-only orphan");
    store
        .save_snapshot("default:Doc:snapshot-only", 5, b"snapshot-only")
        .await
        .expect("seed snapshot-only owner");
    let repair_revision = store
        .begin_key_index_backfill("default", "Doc", repair_signature)
        .await
        .expect("refresh repair epoch after snapshot-only writer");

    assert_eq!(
        store
            .list_entity_ids_by_type("default", "Doc")
            .await
            .expect("live durable enumeration"),
        vec!["snapshot-only".to_string()],
        "snapshot-only state is live while a key-only orphan is not"
    );
    assert_eq!(
        store
            .list_entity_ids_for_key_reconciliation("default", "Doc")
            .await
            .expect("repair enumeration"),
        vec!["orphan-owner".to_string(), "snapshot-only".to_string()],
        "exact repair must see snapshot and derived owners without journals"
    );

    let snapshot_key = EntityKeyRow {
        key_name: "path".to_string(),
        key_hash: "snapshot-path".to_string(),
    };
    let stale = store
        .backfill_entity_keys(
            "default",
            "Doc",
            "snapshot-only",
            0,
            KeyIndexBackfillFence {
                key_set_signature: repair_signature,
                contract_revision: repair_revision,
                expected_journal_sequence: 0,
                expected_entity_live: true,
            },
            std::slice::from_ref(&snapshot_key),
        )
        .await;
    assert!(matches!(
        stale,
        Err(PersistenceError::ConcurrencyViolation {
            expected: 0,
            actual: 5
        })
    ));
    store
        .backfill_entity_keys(
            "default",
            "Doc",
            "snapshot-only",
            5,
            KeyIndexBackfillFence {
                key_set_signature: repair_signature,
                contract_revision: repair_revision,
                expected_journal_sequence: 0,
                expected_entity_live: true,
            },
            std::slice::from_ref(&snapshot_key),
        )
        .await
        .expect("repair snapshot-only owner at durable sequence");
    assert_eq!(
        store
            .lookup_by_key("default", "Doc", "path", "snapshot-path")
            .await
            .expect("snapshot-only lookup"),
        Some("snapshot-only".to_string())
    );
}

#[tokio::test]
async fn journal_source_fence_rejects_equal_sequence_snapshot_repair() {
    let store = SimEventStore::no_faults(42);
    let persistence_id = "default:Doc:source-aba";
    let repair_signature = "v4:path";
    let snapshot_key = EntityKeyRow {
        key_name: "path".to_string(),
        key_hash: "snapshot-path".to_string(),
    };
    let journal_key = EntityKeyRow {
        key_name: "path".to_string(),
        key_hash: "journal-path".to_string(),
    };

    store
        .save_snapshot(persistence_id, 1, b"snapshot-only")
        .await
        .expect("seed snapshot-only generation");
    let repair_revision = store
        .begin_key_index_backfill("default", "Doc", repair_signature)
        .await
        .expect("begin snapshot-derived repair");

    store
        .append_with_index_rows(
            persistence_id,
            0,
            &[test_envelope(0, "Create")],
            std::slice::from_ref(&journal_key),
            &[],
            IndexReconciliation {
                keys: true,
                key_set_signature: Some(repair_signature.to_string()),
                vectors: false,
            },
        )
        .await
        .expect("replace snapshot-only source with equal-sequence journal state");

    let stale = store
        .backfill_entity_keys(
            "default",
            "Doc",
            "source-aba",
            1,
            KeyIndexBackfillFence {
                key_set_signature: repair_signature,
                contract_revision: repair_revision,
                expected_journal_sequence: 0,
                expected_entity_live: true,
            },
            std::slice::from_ref(&snapshot_key),
        )
        .await;
    assert!(matches!(
        stale,
        Err(PersistenceError::JournalBoundaryChanged {
            expected: 0,
            actual: 1,
        })
    ));
    assert_eq!(
        store
            .lookup_by_key("default", "Doc", "path", &journal_key.key_hash)
            .await
            .expect("lookup current journal ownership"),
        Some("source-aba".to_string())
    );
    assert_eq!(
        store
            .lookup_by_key("default", "Doc", "path", &snapshot_key.key_hash)
            .await
            .expect("lookup rejected snapshot ownership"),
        None
    );
}
