//! DST for ADR-0171 exact declared-key reconciliation under failure and retry.

use temper_runtime::persistence::{
    EntityKeyRow, EventMetadata, EventStore, IndexReconciliation, PersistenceEnvelope,
    PersistenceError,
};
use temper_runtime::scheduler::{install_deterministic_context, sim_now, sim_uuid};
use temper_store_sim::SimEventStore;

#[path = "dst_entity_key_reconciliation/seeded_workload.rs"]
mod seeded_workload;

fn envelope(event_type: &str) -> PersistenceEnvelope {
    PersistenceEnvelope {
        sequence_nr: 0,
        event_type: event_type.to_string(),
        payload: serde_json::json!({}),
        metadata: EventMetadata {
            event_id: sim_uuid(),
            causation_id: sim_uuid(),
            correlation_id: sim_uuid(),
            timestamp: sim_now(),
            actor_id: "dst-key-reconciliation".to_string(),
        },
    }
}

fn key(name: &str, hash: &str) -> EntityKeyRow {
    EntityKeyRow {
        key_name: name.to_string(),
        key_hash: hash.to_string(),
    }
}

/// A rejected exact replacement changes neither journal nor ownership. Retrying the
/// same append replaces the complete set, including removal of a declaration no
/// longer emitted; a later empty exact set releases the remaining claim.
#[tokio::test]
async fn dst_failed_exact_reconcile_is_atomic_and_retryable() {
    for seed in [11] {
        let (_guard, _clock, _id) = install_deterministic_context(seed);
        let store = SimEventStore::no_faults(seed);
        let pid = "default:Doc:doc-a";
        let old_path = key("path", "old-path");
        let removed_decl = key("legacy", "legacy-value");
        let new_path = key("path", "new-path");

        store
            .append_with_keys(
                pid,
                0,
                &[envelope("Create")],
                &[old_path.clone(), removed_decl.clone()],
            )
            .await
            .unwrap();

        store.inject_concurrency_violations(pid, 1);
        let rejected = store
            .append_with_keys(
                pid,
                1,
                &[envelope("Rekey")],
                std::slice::from_ref(&new_path),
            )
            .await;
        assert!(
            matches!(rejected, Err(PersistenceError::ConcurrencyViolation { .. })),
            "seed {seed}: injected pre-commit failure must reject the replacement"
        );
        assert_eq!(store.read_events(pid, 0).await.unwrap().len(), 1);
        for row in [&old_path, &removed_decl] {
            assert_eq!(
                store
                    .lookup_by_key("default", "Doc", &row.key_name, &row.key_hash)
                    .await
                    .unwrap(),
                Some("doc-a".to_string()),
                "seed {seed}: rejection must preserve the prior '{}' claim",
                row.key_name
            );
        }

        store
            .append_with_keys(
                pid,
                1,
                &[envelope("Rekey")],
                std::slice::from_ref(&new_path),
            )
            .await
            .unwrap();
        assert_eq!(store.read_events(pid, 0).await.unwrap().len(), 2);
        for row in [&old_path, &removed_decl] {
            assert_eq!(
                store
                    .lookup_by_key("default", "Doc", &row.key_name, &row.key_hash)
                    .await
                    .unwrap(),
                None,
                "seed {seed}: retry must remove stale '{}' ownership",
                row.key_name
            );
        }
        assert_eq!(
            store
                .lookup_by_key("default", "Doc", "path", "new-path")
                .await
                .unwrap(),
            Some("doc-a".to_string())
        );

        store
            .append_with_keys(pid, 2, &[envelope("ClearKey")], &[])
            .await
            .unwrap();
        assert_eq!(
            store
                .lookup_by_key("default", "Doc", "path", "new-path")
                .await
                .unwrap(),
            None,
            "seed {seed}: an authoritative empty set must release the final claim"
        );
    }
}

/// A background repair may replay sequence N while the live actor commits N+1.
/// The stale repair must be rejected rather than restoring N's old key row over the
/// newer rename. This is the deterministic interleaving history for the backfill
/// sequence fence; retrying from N+1 converges idempotently.
#[tokio::test]
async fn dst_stale_backfill_cannot_overwrite_newer_live_ownership() {
    for seed in [12] {
        let (_guard, _clock, _id) = install_deterministic_context(seed);
        let store = SimEventStore::no_faults(seed);
        let pid = "default:Doc:doc-race";
        let old_path = key("path", "old-path");
        let new_path = key("path", "new-path");

        store
            .append_with_keys(
                pid,
                0,
                &[envelope("Create")],
                std::slice::from_ref(&old_path),
            )
            .await
            .unwrap();

        // Backfill has replayed sequence 1 and derived old_path. Before it writes,
        // the live actor commits a rename at sequence 2.
        store
            .append_with_keys(
                pid,
                1,
                &[envelope("Rekey")],
                std::slice::from_ref(&new_path),
            )
            .await
            .unwrap();
        let stale = store
            .backfill_entity_keys(
                "default",
                "Doc",
                "doc-race",
                1,
                std::slice::from_ref(&old_path),
            )
            .await;
        assert!(
            matches!(
                stale,
                Err(PersistenceError::ConcurrencyViolation {
                    expected: 1,
                    actual: 2
                })
            ),
            "seed {seed}: stale sequence-1 repair must be fenced after sequence 2"
        );
        assert_eq!(
            store
                .lookup_by_key("default", "Doc", "path", "old-path")
                .await
                .unwrap(),
            None,
            "seed {seed}: stale ownership must not be restored"
        );
        assert_eq!(
            store
                .lookup_by_key("default", "Doc", "path", "new-path")
                .await
                .unwrap(),
            Some("doc-race".to_string())
        );

        store
            .backfill_entity_keys(
                "default",
                "Doc",
                "doc-race",
                2,
                std::slice::from_ref(&new_path),
            )
            .await
            .unwrap();
    }
}

/// Backfill establishes its target contract before replay. A live writer still
/// running the prior contract must advance the revision and fence publication;
/// merely capturing the prior revision would miss this interleaving.
#[tokio::test]
async fn dst_old_contract_live_write_fences_new_contract_backfill() {
    for seed in [13] {
        let (_guard, _clock, _id) = install_deterministic_context(seed);
        let store = SimEventStore::no_faults(seed);
        let pid = "default:Doc:doc-contract-race";

        store
            .append_with_index_rows(
                pid,
                0,
                &[envelope("Create")],
                &[key("path", "old-contract")],
                &[],
                IndexReconciliation {
                    keys: true,
                    key_set_signature: Some("v3:old-contract".to_string()),
                    vectors: false,
                },
            )
            .await
            .unwrap();
        store
            .mark_key_index_backfilled("default", "Doc", "v3:old-contract")
            .await
            .unwrap();

        let repair_revision = store
            .begin_key_index_backfill("default", "Doc", "v3:new-contract")
            .await
            .unwrap();
        assert!(
            store
                .key_index_backfilled_types("default")
                .await
                .unwrap()
                .is_empty(),
            "seed {seed}: beginning repair must withhold old coverage"
        );

        // An old-table actor writes after the new repair target was established.
        store
            .append_with_index_rows(
                pid,
                1,
                &[envelope("OldContractWrite")],
                &[key("path", "old-contract-after-repair-start")],
                &[],
                IndexReconciliation {
                    keys: true,
                    key_set_signature: Some("v3:old-contract".to_string()),
                    vectors: false,
                },
            )
            .await
            .unwrap();
        assert!(
            !store
                .mark_key_index_backfilled_if_revision(
                    "default",
                    "Doc",
                    "v3:new-contract",
                    repair_revision,
                )
                .await
                .unwrap(),
            "seed {seed}: mixed-contract repair must not publish"
        );
        assert!(
            store
                .key_index_backfilled_types("default")
                .await
                .unwrap()
                .is_empty(),
            "seed {seed}: failed publication must remain scan-safe"
        );
    }
}

/// Historical streams can predate declared-key enforcement. If two live streams
/// replay to the same claim, exact repair cannot represent both and must fail closed:
/// preserving the existing owner while preventing the caller from certifying a
/// complete watermark. Silently skipping the second claim would make a partial index
/// look authoritative.
#[tokio::test]
async fn dst_conflicting_backfill_claim_fails_without_partial_mutation() {
    for seed in [14] {
        let (_guard, _clock, _id) = install_deterministic_context(seed);
        let store = SimEventStore::no_faults(seed);
        let claimed = key("path", "shared-path");

        store
            .append_with_keys(
                "default:Doc:owner",
                0,
                &[envelope("Create")],
                std::slice::from_ref(&claimed),
            )
            .await
            .unwrap();
        // A legacy stream written before key co-commit has durable state but no key
        // row. Its replay now derives the same current claim as `owner`.
        store
            .append("default:Doc:legacy-duplicate", 0, &[envelope("Create")])
            .await
            .unwrap();

        let conflict = store
            .backfill_entity_keys(
                "default",
                "Doc",
                "legacy-duplicate",
                1,
                std::slice::from_ref(&claimed),
            )
            .await;
        assert!(
            matches!(conflict, Err(PersistenceError::Storage(_))),
            "seed {seed}: conflicting exact repair must block watermark eligibility"
        );
        assert_eq!(
            store
                .lookup_by_key("default", "Doc", "path", "shared-path")
                .await
                .unwrap(),
            Some("owner".to_string()),
            "seed {seed}: failed repair must preserve the established owner"
        );
    }
}
