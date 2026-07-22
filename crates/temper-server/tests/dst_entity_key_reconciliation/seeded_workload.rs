//! Seed-varied fault, interleaving, and restart workloads.

use std::collections::BTreeSet;

use super::*;
use temper_store_sim::{DeterministicRng, SimFaultConfig};

const NUM_SEEDS: u64 = 100;

#[derive(Debug, Default)]
struct FaultCounts {
    concurrency: u64,
    storage: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReconciliationTrace {
    repair_before_write: bool,
    old_contract_writer: bool,
    delete_final: bool,
    restart_phase: u8,
    forced_concurrency_failures: u64,
    observed_concurrency_failures: u64,
    observed_storage_failures: u64,
    replayed_event_types: Vec<String>,
    final_owner: Option<String>,
}

async fn append_exact_with_retry(
    store: &SimEventStore,
    persistence_id: &str,
    expected_sequence: u64,
    event_type: &str,
    rows: &[EntityKeyRow],
    signature: &str,
    faults: &mut FaultCounts,
) -> u64 {
    let mut event = envelope(event_type);
    if event_type == "Delete" {
        event.payload = serde_json::json!({
            "action": "Delete",
            "from_status": "Ready",
            "to_status": "Deleted",
        });
    }
    for _attempt in 0..128 {
        match store
            .append_with_index_rows(
                persistence_id,
                expected_sequence,
                std::slice::from_ref(&event),
                rows,
                &[],
                IndexReconciliation {
                    keys: true,
                    key_set_signature: Some(signature.to_string()),
                    vectors: false,
                    snapshot_source: Default::default(),
                },
            )
            .await
        {
            Ok(sequence) => return sequence,
            Err(PersistenceError::ConcurrencyViolation { .. }) => faults.concurrency += 1,
            Err(PersistenceError::Storage(_)) => faults.storage += 1,
            Err(error) => panic!("unexpected append failure: {error}"),
        }
    }
    panic!("deterministic append retry budget exhausted for {persistence_id}")
}

async fn replay_after_restart(
    store: &SimEventStore,
    persistence_id: &str,
    expected_events: usize,
) -> Vec<String> {
    // A clone models a fresh store handle after process/actor restart while the
    // durable simulated journal remains shared.
    let restarted = store.clone();
    let events = restarted
        .read_events(persistence_id, 0)
        .await
        .expect("restart replay must read the durable journal");
    assert_eq!(events.len(), expected_events);
    events.into_iter().map(|event| event.event_type).collect()
}

async fn run_reconciliation_workload(seed: u64) -> ReconciliationTrace {
    let (_guard, _clock, _id) = install_deterministic_context(seed);
    let mut schedule = DeterministicRng::new(seed ^ 0x00A1_7017_1D57);
    let repair_before_write = schedule.next_u64() & 1 == 0;
    let old_contract_writer = schedule.next_u64() & 1 == 0;
    let delete_final = matches!(schedule.next_u64().checked_rem(3), Some(0));
    let restart_phase = (schedule.next_u64() % 3) as u8;
    let forced_concurrency_failures = 1 + schedule.next_u64() % 3;
    let store = SimEventStore::new(
        seed,
        SimFaultConfig {
            write_failure_prob: 0.20,
            concurrency_violation_prob: 0.12,
            read_truncation_prob: 0.0,
            snapshot_failure_prob: 0.0,
        },
    );
    let persistence_id = "default:Doc:dst-workload";
    let contract_a = "v3|4:path[4:Path]";
    let contract_b = "v3|8:new_path[7:NewPath]";
    let old_row = key("path", "old-path");
    let old_writer_row = key("path", "old-writer-path");
    let final_event_type = if delete_final { "Delete" } else { "LiveWrite" };
    let final_rows = if delete_final {
        Vec::new()
    } else {
        vec![key("new_path", "new-path")]
    };
    let old_contract_rows = if delete_final {
        Vec::new()
    } else {
        vec![old_writer_row.clone()]
    };
    let mut faults = FaultCounts::default();

    assert_eq!(
        append_exact_with_retry(
            &store,
            persistence_id,
            0,
            "Create",
            std::slice::from_ref(&old_row),
            contract_a,
            &mut faults,
        )
        .await,
        1
    );
    store
        .mark_key_index_backfilled("default", "Doc", contract_a)
        .await
        .expect("mark initial coverage");
    if restart_phase == 0 {
        replay_after_restart(&store, persistence_id, 1).await;
    }

    store.inject_concurrency_violations(persistence_id, forced_concurrency_failures);
    if repair_before_write {
        let stale_sequence = 1;
        let stale_rows = vec![old_row.clone()];
        let first_repair_revision = store
            .begin_key_index_backfill("default", "Doc", contract_b)
            .await
            .expect("begin contract-B repair before live write");
        let (live_rows, live_signature) = if old_contract_writer {
            (old_contract_rows.as_slice(), contract_a)
        } else {
            (final_rows.as_slice(), contract_b)
        };
        assert_eq!(
            append_exact_with_retry(
                &store,
                persistence_id,
                1,
                final_event_type,
                live_rows,
                live_signature,
                &mut faults,
            )
            .await,
            2
        );
        let stale = store
            .backfill_entity_keys(
                "default",
                "Doc",
                "dst-workload",
                stale_sequence,
                KeyIndexBackfillFence {
                    key_set_signature: contract_b,
                    contract_revision: first_repair_revision,
                    expected_journal_sequence: stale_sequence,
                    expected_entity_live: true,
                    expected_snapshot: None,
                },
                &stale_rows,
            )
            .await;
        if old_contract_writer {
            assert!(matches!(
                stale,
                Err(PersistenceError::KeyContractChanged {
                    expected_revision,
                    actual_revision,
                    ..
                }) if expected_revision == first_repair_revision
                    && actual_revision > first_repair_revision
            ));
            assert!(
                !store
                    .mark_key_index_backfilled_if_revision(
                        "default",
                        "Doc",
                        contract_b,
                        first_repair_revision,
                    )
                    .await
                    .expect("mixed-contract publication check")
            );
        } else {
            assert!(matches!(
                stale,
                Err(PersistenceError::JournalBoundaryChanged {
                    expected: 1,
                    actual: 2
                })
            ));
        }
    } else {
        assert_eq!(
            append_exact_with_retry(
                &store,
                persistence_id,
                1,
                final_event_type,
                &final_rows,
                contract_b,
                &mut faults,
            )
            .await,
            2
        );
    }

    if restart_phase == 1 {
        replay_after_restart(&store, persistence_id, 2).await;
    }

    let final_revision = store
        .begin_key_index_backfill("default", "Doc", contract_b)
        .await
        .expect("begin converging contract-B repair");
    store
        .backfill_entity_keys(
            "default",
            "Doc",
            "dst-workload",
            2,
            KeyIndexBackfillFence {
                key_set_signature: contract_b,
                contract_revision: final_revision,
                expected_journal_sequence: 2,
                expected_entity_live: !delete_final,
                expected_snapshot: None,
            },
            &final_rows,
        )
        .await
        .expect("latest-sequence repair converges");
    assert!(
        store
            .mark_key_index_backfilled_if_revision("default", "Doc", contract_b, final_revision,)
            .await
            .expect("publish converged contract-B coverage")
    );
    if restart_phase == 2 {
        replay_after_restart(&store, persistence_id, 2).await;
    }

    let replayed_event_types = replay_after_restart(&store, persistence_id, 2).await;
    assert_eq!(
        replayed_event_types.last().map(String::as_str),
        Some(final_event_type),
        "the delete schedule must persist and replay a real terminal boundary"
    );
    assert_eq!(
        store
            .list_entity_ids_by_type("default", "Doc")
            .await
            .expect("classify final stream liveness")
            .contains(&"dst-workload".to_string()),
        !delete_final,
        "terminal schedules must remain deleted after restart"
    );
    let final_owner = store
        .lookup_by_key("default", "Doc", "new_path", "new-path")
        .await
        .expect("read final ownership");
    assert_eq!(
        store
            .lookup_by_key("default", "Doc", "path", "old-path")
            .await
            .expect("read released ownership"),
        None
    );
    assert_eq!(
        final_owner,
        (!delete_final).then(|| "dst-workload".to_string())
    );
    assert_eq!(
        store
            .key_index_backfilled_types("default")
            .await
            .expect("read final watermark"),
        vec![("Doc".to_string(), contract_b.to_string())]
    );

    ReconciliationTrace {
        repair_before_write,
        old_contract_writer,
        delete_final,
        restart_phase,
        forced_concurrency_failures,
        observed_concurrency_failures: faults.concurrency,
        observed_storage_failures: faults.storage,
        replayed_event_types,
        final_owner,
    }
}

/// Seeded schedules choose both repair/write orderings, contract races, deletes,
/// restart points, and one-to-three forced concurrency failures while nonzero
/// probabilistic write/concurrency faults exercise retry. Every history converges
/// to replayable contract-B ownership.
#[tokio::test]
async fn dst_seeded_fault_workloads_vary_and_converge_after_restart() {
    let mut schedules = BTreeSet::new();
    let mut storage_failures = 0;
    for seed in 0..NUM_SEEDS {
        let trace = run_reconciliation_workload(seed).await;
        assert!(
            trace.observed_concurrency_failures >= trace.forced_concurrency_failures,
            "seed {seed}: every forced concurrency failure must be observed"
        );
        storage_failures += trace.observed_storage_failures;
        schedules.insert((
            trace.repair_before_write,
            trace.old_contract_writer,
            trace.delete_final,
            trace.restart_phase,
        ));
    }
    assert!(
        schedules.len() >= 8,
        "seed must materially vary interleavings, contracts, deletes, and restart points"
    );
    assert!(
        storage_failures > 0,
        "nonzero deterministic write-fault configuration must be exercised"
    );
}

/// Same seed means the same workload choices, injected failures, durable replay,
/// and final ownership; a neighboring seed must produce a distinct trace.
#[tokio::test]
async fn dst_reconciliation_trace_canary_is_seed_stable() {
    let first = run_reconciliation_workload(37).await;
    let second = run_reconciliation_workload(37).await;
    assert_eq!(first, second);
    assert_ne!(first, run_reconciliation_workload(38).await);
}
