use super::*;
use temper_runtime::persistence::{
    EntityKeyRow, KeyIndexBackfillFence, SnapshotBackfillFence, encode_activated_key_contract,
};

#[tokio::test]
async fn activation_epoch_rejects_delayed_a_writer_after_a_is_reactivated() {
    let store = SimEventStore::no_faults(296);
    let persistence_id = "default:Doc:delayed-old-writer";
    let old_signature = "v3|4:path[4:Path]";
    let empty_signature = "v3";
    let old_row = EntityKeyRow {
        key_name: "path".to_string(),
        key_hash: "released-path".to_string(),
    };

    let old_epoch = store
        .activate_key_index_contract("default", "Doc", old_signature, false)
        .await
        .expect("activate original contract");
    store
        .mark_key_index_backfilled("default", "Doc", old_signature)
        .await
        .expect("publish original contract readiness");
    let old_writer_contract = encode_activated_key_contract(old_signature, old_epoch);
    store
        .append_with_index_rows(
            persistence_id,
            0,
            &[test_envelope(0, "Created")],
            std::slice::from_ref(&old_row),
            &[],
            IndexReconciliation {
                keys: true,
                key_set_signature: Some(old_writer_contract.clone()),
                ..Default::default()
            },
        )
        .await
        .expect("seed original owner");

    store
        .activate_key_index_contract("default", "Doc", empty_signature, true)
        .await
        .expect("activate empty contract and purge ownership");
    let current_epoch = store
        .activate_key_index_contract("default", "Doc", old_signature, false)
        .await
        .expect("reactivate original signature at a new epoch");
    let delayed = store
        .append_with_index_rows(
            persistence_id,
            1,
            &[test_envelope(0, "UpdatedByOldTable")],
            std::slice::from_ref(&old_row),
            &[],
            IndexReconciliation {
                keys: true,
                key_set_signature: Some(old_writer_contract),
                ..Default::default()
            },
        )
        .await;
    assert!(matches!(
        delayed,
        Err(PersistenceError::KeyContractActivationStale {
            activated_epoch,
            attempted_epoch: Some(attempted_epoch),
        }) if activated_epoch == current_epoch && attempted_epoch == old_epoch
    ));
    assert_eq!(store.dump_journal(persistence_id).len(), 1);
    assert_eq!(
        store
            .lookup_by_key("default", "Doc", "path", &old_row.key_hash)
            .await
            .expect("lookup released ownership"),
        None,
        "the rejected old writer must not reinsert a purged claim"
    );
}

#[tokio::test]
async fn failed_spec_persistence_before_activation_preserves_old_writer_contract() {
    let store = SimEventStore::no_faults(297);
    let persistence_id = "default:Doc:crash-cut-owner";
    let old_signature = "v3|4:path[4:Path]";
    let old_row = EntityKeyRow {
        key_name: "path".to_string(),
        key_hash: "crash-cut-path".to_string(),
    };

    let old_epoch = store
        .activate_key_index_contract("default", "Doc", old_signature, false)
        .await
        .expect("activate original contract");
    store
        .mark_key_index_backfilled("default", "Doc", old_signature)
        .await
        .expect("publish original contract readiness");
    let old_writer_contract = encode_activated_key_contract(old_signature, old_epoch);
    store
        .append_with_index_rows(
            persistence_id,
            0,
            &[test_envelope(0, "Created")],
            std::slice::from_ref(&old_row),
            &[],
            IndexReconciliation {
                keys: true,
                key_set_signature: Some(old_writer_contract.clone()),
                ..Default::default()
            },
        )
        .await
        .expect("seed original owner");
    store
        .mark_key_index_backfilled("default", "Doc", old_signature)
        .await
        .expect("publish original coverage");

    // Persist-before-activate rollout means a failed durable spec write never
    // touches the live contract. The old table must remain writable after the
    // failed install returns; activation is attempted only after persistence.
    let failed_persistence: Result<(), &str> = Err("injected durable spec failure");
    assert!(failed_persistence.is_err());
    store
        .append_with_index_rows(
            persistence_id,
            1,
            &[test_envelope(0, "OldSpecStillLive")],
            std::slice::from_ref(&old_row),
            &[],
            IndexReconciliation {
                keys: true,
                key_set_signature: Some(old_writer_contract),
                ..Default::default()
            },
        )
        .await
        .expect("old writer remains accepted after failed persistence");

    assert!(
        store
            .key_index_backfilled_types("default")
            .await
            .expect("read post-crash coverage")
            == vec![("Doc".to_string(), old_signature.to_string())],
        "failed persistence must preserve the original coverage proof"
    );
    assert_eq!(
        store
            .lookup_by_key("default", "Doc", "path", &old_row.key_hash)
            .await
            .expect("read post-crash ownership"),
        Some("crash-cut-owner".to_string()),
        "failed persistence must not purge the original ownership row"
    );
}

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
                snapshot_source: Default::default(),
                batch_idempotency: None,
            },
            PersistenceAppend {
                persistence_id: claimant_pid.to_string(),
                expected_sequence: 0,
                events: vec![test_envelope(0, "Created")],
                key_rows: vec![claimed_key.clone()],
                reconcile_keys: true,
                key_set_signature: Some("v3:path".to_string()),
                snapshot_source: Default::default(),
                batch_idempotency: None,
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
                snapshot_source: Default::default(),
                batch_idempotency: None,
            },
            PersistenceAppend {
                persistence_id: "default:Doc:doc-conflict".to_string(),
                expected_sequence: 0,
                events: vec![test_envelope(0, "Created")],
                key_rows: vec![claimed_key.clone()],
                reconcile_keys: true,
                key_set_signature: Some("v3:path".to_string()),
                snapshot_source: Default::default(),
                batch_idempotency: None,
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
                expected_snapshot: None,
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
                expected_snapshot: Some(SnapshotBackfillFence {
                    sequence_nr: 5,
                    state: b"snapshot-only",
                }),
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
                expected_snapshot: Some(SnapshotBackfillFence {
                    sequence_nr: 5,
                    state: b"snapshot-only",
                }),
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
async fn key_reconciliation_pages_stop_at_the_captured_candidate_boundary() {
    let store = SimEventStore::no_faults(43);
    for entity_id in ["a", "b", "c"] {
        store
            .save_snapshot(&format!("default:Doc:{entity_id}"), 1, b"seed")
            .await
            .expect("seed initial repair candidate");
    }

    let boundary = store
        .key_reconciliation_boundary("default", "Doc")
        .await
        .expect("capture repair boundary")
        .expect("initial candidates establish a boundary");
    assert_eq!(boundary, "c");

    let mut cursor = None;
    let mut observed = Vec::new();
    for growth_index in 0..8 {
        let page = store
            .list_key_reconciliation_page("default", "Doc", cursor.as_deref(), &boundary, 1)
            .await
            .expect("read bounded repair page");
        let Some(candidate) = page.into_iter().next() else {
            break;
        };
        cursor = Some(candidate.entity_id.clone());
        observed.push(candidate.entity_id);

        store
            .save_snapshot(
                &format!("default:Doc:z-growing-{growth_index}"),
                1,
                b"concurrent writer",
            )
            .await
            .expect("grow the candidate set after boundary capture");
    }

    assert_eq!(
        observed,
        ["a", "b", "c"],
        "concurrent writers beyond the captured terminal ID must not extend repair paging"
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
                snapshot_source: Default::default(),
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
                expected_snapshot: Some(SnapshotBackfillFence {
                    sequence_nr: 1,
                    state: b"snapshot-only",
                }),
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

#[tokio::test]
async fn nonempty_journal_generation_outranks_an_ahead_legacy_snapshot() {
    let store = SimEventStore::no_faults(43);
    let persistence_id = "default:Doc:journal-generation";
    let repair_signature = "v4:path";
    let journal_key = EntityKeyRow {
        key_name: "path".to_string(),
        key_hash: "journal-generation-path".to_string(),
    };

    store
        .save_snapshot(persistence_id, 5, b"legacy-snapshot-five")
        .await
        .expect("seed ahead legacy snapshot");
    store
        .append(
            persistence_id,
            0,
            &[test_envelope(0, "Create"), test_envelope(0, "Update")],
        )
        .await
        .expect("seed lower-coordinate journal generation");
    let repair_revision = store
        .begin_key_index_backfill("default", "Doc", repair_signature)
        .await
        .expect("begin journal-derived repair");

    store
        .backfill_entity_keys(
            "default",
            "Doc",
            "journal-generation",
            2,
            KeyIndexBackfillFence {
                key_set_signature: repair_signature,
                contract_revision: repair_revision,
                expected_journal_sequence: 2,
                expected_entity_live: true,
                expected_snapshot: Some(SnapshotBackfillFence {
                    sequence_nr: 5,
                    state: b"legacy-snapshot-five",
                }),
            },
            std::slice::from_ref(&journal_key),
        )
        .await
        .expect("journal generation owns the repair coordinate");
    let owner = store
        .lookup_by_key_with_sequence("default", "Doc", "path", &journal_key.key_hash)
        .await
        .expect("lookup journal-generation owner")
        .expect("journal-generation key row");
    assert_eq!(owner.entity_id, "journal-generation");
    assert_eq!(owner.sequence_nr, 2);
}
