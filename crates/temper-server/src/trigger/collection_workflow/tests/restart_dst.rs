use temper_runtime::scheduler::install_deterministic_context;
use temper_store_sim::{DeterministicRng, SimEventStore, SimFaultConfig};

use super::*;

const DST_SEEDS: std::ops::Range<u64> = 0..32;
const GENERATED_DST_SEEDS: std::ops::Range<u64> = 0..128;
const RESTART_ATTEMPT_BUDGET: usize = 64;

#[derive(Default)]
struct GeneratedCoverage {
    injected_failures: u64,
    lost_replies: [u64; 5],
}

async fn observe_start_across_restarts(
    sim: &SimEventStore,
    rng: &mut DeterministicRng,
    coverage: &mut GeneratedCoverage,
    source: PersistenceAppend,
    intent: &CollectionStartIntentV1,
    record: &CollectionWorkflowRecordV1,
) {
    for _ in 0..RESTART_ATTEMPT_BUDGET {
        let restarted = BoxedEventStore::new(sim.clone());
        match commit_collection_start(&restarted, source.clone(), intent, record).await {
            Ok(CollectionLedgerCommitOutcome::Committed(_)) if rng.chance(0.5) => {
                coverage.lost_replies[0] += 1;
            }
            Ok(_) => return,
            Err(PersistenceError::ConcurrencyViolation { .. } | PersistenceError::Storage(_)) => {
                coverage.injected_failures += 1;
            }
            Err(error) => panic!("generated start failed permanently: {error}"),
        }
    }
    panic!("generated start exhausted its restart attempt budget");
}

async fn observe_record_across_restarts(
    sim: &SimEventStore,
    rng: &mut DeterministicRng,
    coverage: &mut GeneratedCoverage,
    boundary: usize,
    expected_sequence: u64,
    event_type: &str,
    record: &CollectionWorkflowRecordV1,
) -> u64 {
    for _ in 0..RESTART_ATTEMPT_BUDGET {
        let restarted = BoxedEventStore::new(sim.clone());
        match append_collection_record_idempotent(&restarted, expected_sequence, event_type, record)
            .await
        {
            Ok((CollectionMutationOutcome::Applied, _)) if rng.chance(0.5) => {
                coverage.lost_replies[boundary] += 1;
            }
            Ok((_, sequence)) => return sequence,
            Err(PersistenceError::ConcurrencyViolation { .. } | PersistenceError::Storage(_)) => {
                coverage.injected_failures += 1;
            }
            Err(error) => panic!("generated lifecycle append failed permanently: {error}"),
        }
    }
    panic!("generated lifecycle append exhausted its restart attempt budget");
}

async fn observe_control_across_restarts(
    sim: &SimEventStore,
    rng: &mut DeterministicRng,
    coverage: &mut GeneratedCoverage,
    source: PersistenceAppend,
    intent: &CollectionControlIntentV1,
    expected_sequence: u64,
    record: &CollectionWorkflowRecordV1,
) {
    for _ in 0..RESTART_ATTEMPT_BUDGET {
        let restarted = BoxedEventStore::new(sim.clone());
        match commit_collection_control(
            &restarted,
            source.clone(),
            intent,
            expected_sequence,
            record,
        )
        .await
        {
            Ok(CollectionLedgerCommitOutcome::Committed(_)) if rng.chance(0.5) => {
                coverage.lost_replies[3] += 1;
            }
            Ok(_) => return,
            Err(PersistenceError::ConcurrencyViolation { .. } | PersistenceError::Storage(_)) => {
                coverage.injected_failures += 1;
            }
            Err(error) => panic!("generated control failed permanently: {error}"),
        }
    }
    panic!("generated control exhausted its restart attempt budget");
}

#[tokio::test]
async fn generated_faults_and_crashes_reconstruct_every_collection_boundary() {
    let mut coverage = GeneratedCoverage::default();
    for seed in GENERATED_DST_SEEDS {
        let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
        let sim = SimEventStore::new(
            seed,
            SimFaultConfig {
                write_failure_prob: 0.2,
                concurrency_violation_prob: 0.2,
                read_truncation_prob: 0.0,
                snapshot_failure_prob: 0.0,
            },
        );
        let mut rng = DeterministicRng::new(seed.wrapping_add(0xd57c_011e_c710));
        let tenant = format!("collection-generated-restart-{seed}");
        let source_id = "generated";
        let (intent, mut record) =
            CollectionWorkflowRecordV1::start(start(&tenant, source_id, &["a", "b"]))
                .expect("valid generated workflow start");

        observe_start_across_restarts(
            &sim,
            &mut rng,
            &mut coverage,
            source_append(&tenant, source_id, 0, "StartChecks"),
            &intent,
            &record,
        )
        .await;

        record
            .admit_member(0, "delivery-a".to_string(), 0)
            .expect("admit generated member");
        let mut workflow_sequence = observe_record_across_restarts(
            &sim,
            &mut rng,
            &mut coverage,
            1,
            1,
            "CollectionWorkflow::AdmittedV1",
            &record,
        )
        .await;

        let receipt = CollectionMemberReceipt {
            delivery_id: "delivery-a".to_string(),
            fencing_token: 7,
        };
        record
            .record_member_receipt(
                &record.members[0].member_id.clone(),
                "delivery-a",
                0,
                1,
                receipt.clone(),
            )
            .expect("record generated receipt");
        workflow_sequence = observe_record_across_restarts(
            &sim,
            &mut rng,
            &mut coverage,
            2,
            workflow_sequence,
            "CollectionWorkflow::ReceiptedV1",
            &record,
        )
        .await;

        let (control, _) = record
            .request_control(
                CollectionRequestedOutcome::Cancelled,
                None,
                "CancelChecks".to_string(),
                2,
                serde_json::json!({"principal": "controller"}),
                None,
            )
            .expect("request generated control");
        observe_control_across_restarts(
            &sim,
            &mut rng,
            &mut coverage,
            source_append(&tenant, source_id, 1, "CancelChecks"),
            &control,
            workflow_sequence,
            &record,
        )
        .await;
        workflow_sequence += 1;

        record
            .record_member_terminal(CollectionMemberTerminalEvidence {
                member_id: record.members[0].member_id.clone(),
                control_epoch: 1,
                status: CollectionMemberStatus::Succeeded,
                attempts: 1,
                delivery_id: Some("delivery-a".to_string()),
                delivery_status: ReactionDeliveryStatus::Succeeded,
                receipt: Some(receipt),
                failure_class: None,
            })
            .expect("terminalize generated member");
        workflow_sequence = observe_record_across_restarts(
            &sim,
            &mut rng,
            &mut coverage,
            4,
            workflow_sequence,
            "CollectionWorkflow::TerminalV1",
            &record,
        )
        .await;

        let recovered = BoxedEventStore::new(sim.clone());
        assert_eq!(
            load_collection_record(&recovered, &tenant, &record.workflow_id)
                .await
                .expect("load generated workflow")
                .expect("generated workflow exists"),
            (record.clone(), workflow_sequence),
            "seed {seed}: generated restart must reconstruct exact terminal truth"
        );
        assert_eq!(record.status, CollectionWorkflowStatus::Cancelled);
        assert_eq!(record.counts.succeeded, 1);
        assert_eq!(record.counts.cancelled, 1);
    }

    assert!(
        coverage.injected_failures > 0,
        "generated workload must exercise pre-commit store failures"
    );
    for (boundary, count) in coverage.lost_replies.into_iter().enumerate() {
        assert!(
            count > 0,
            "generated workload must lose replies at boundary {boundary}"
        );
    }
}

#[tokio::test]
async fn restart_reconciles_ambiguous_collection_writes_without_duplicates() {
    for seed in DST_SEEDS {
        let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
        let sim = SimEventStore::no_faults(seed);
        let tenant = format!("collection-restart-{seed}");
        let source_id = "ambiguous";
        let (intent, mut record) =
            CollectionWorkflowRecordV1::start(start(&tenant, source_id, &["a", "b", "c"]))
                .expect("valid workflow start");

        let before_start_crash = BoxedEventStore::new(sim.clone());
        commit_collection_start(
            &before_start_crash,
            source_append(&tenant, source_id, 0, "StartChecks"),
            &intent,
            &record,
        )
        .await
        .expect("start commit before lost reply");
        drop(before_start_crash);

        let after_start_restart = BoxedEventStore::new(sim.clone());
        assert!(matches!(
            commit_collection_start(
                &after_start_restart,
                source_append(&tenant, source_id, 0, "StartChecks"),
                &intent,
                &record,
            )
            .await
            .expect("reconcile start after restart"),
            CollectionLedgerCommitOutcome::Reconciled(_)
        ));

        record
            .admit_member(0, "delivery-a".to_string(), 0)
            .expect("admit first member");
        append_collection_record_idempotent(
            &after_start_restart,
            1,
            "CollectionWorkflow::AdmittedV1",
            &record,
        )
        .await
        .expect("admission commit before lost reply");
        drop(after_start_restart);

        let after_admission_restart = BoxedEventStore::new(sim.clone());
        assert_eq!(
            append_collection_record_idempotent(
                &after_admission_restart,
                1,
                "CollectionWorkflow::AdmittedV1",
                &record,
            )
            .await
            .expect("reconcile admission after restart"),
            (CollectionMutationOutcome::Replayed, 2)
        );

        let receipt = CollectionMemberReceipt {
            delivery_id: "delivery-a".to_string(),
            fencing_token: 7,
        };
        record
            .record_member_receipt(
                &record.members[0].member_id.clone(),
                "delivery-a",
                0,
                1,
                receipt,
            )
            .expect("record member receipt");
        append_collection_record_idempotent(
            &after_admission_restart,
            2,
            "CollectionWorkflow::ReceiptedV1",
            &record,
        )
        .await
        .expect("receipt commit");

        let (control, _) = record
            .request_control(
                CollectionRequestedOutcome::Cancelled,
                None,
                "CancelChecks".to_string(),
                2,
                serde_json::json!({"principal": "controller"}),
                None,
            )
            .expect("request control");
        commit_collection_control(
            &after_admission_restart,
            source_append(&tenant, source_id, 1, "CancelChecks"),
            &control,
            3,
            &record,
        )
        .await
        .expect("control commit before lost reply");
        drop(after_admission_restart);

        let after_control_restart = BoxedEventStore::new(sim.clone());
        assert!(matches!(
            commit_collection_control(
                &after_control_restart,
                source_append(&tenant, source_id, 1, "CancelChecks"),
                &control,
                3,
                &record,
            )
            .await
            .expect("reconcile control after restart"),
            CollectionLedgerCommitOutcome::Reconciled(_)
        ));
        assert_eq!(
            load_collection_record(&after_control_restart, &tenant, &record.workflow_id)
                .await
                .expect("reload workflow")
                .expect("workflow exists"),
            (record.clone(), 4),
            "seed {seed}: restart must reconstruct the exact workflow state"
        );

        let source_journal = format!("{tenant}:Batch:{source_id}");
        let workflow_journal = collection_workflow_journal_id(&tenant, &record.workflow_id);
        assert_eq!(
            sim.dump_journal(&source_journal).len(),
            2,
            "seed {seed}: ambiguous retries must not duplicate source events"
        );
        assert_eq!(
            sim.dump_journal(&workflow_journal).len(),
            4,
            "seed {seed}: ambiguous retries must not duplicate workflow snapshots"
        );
    }
}

#[tokio::test]
async fn restart_after_atomic_batch_failure_never_observes_a_torn_commit() {
    for seed in DST_SEEDS {
        let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
        let sim = SimEventStore::no_faults(seed);
        let tenant = format!("collection-atomic-restart-{seed}");
        let source_id = "fenced";
        let (intent, record) =
            CollectionWorkflowRecordV1::start(start(&tenant, source_id, &["a", "b"]))
                .expect("valid workflow start");
        let source_journal = format!("{tenant}:Batch:{source_id}");
        let workflow_journal = collection_workflow_journal_id(&tenant, &record.workflow_id);

        sim.inject_concurrency_violations(&workflow_journal, 1);
        let before_crash = BoxedEventStore::new(sim.clone());
        assert!(matches!(
            commit_collection_start(
                &before_crash,
                source_append(&tenant, source_id, 0, "StartChecks"),
                &intent,
                &record,
            )
            .await,
            Err(PersistenceError::ConcurrencyViolation { .. })
        ));
        drop(before_crash);

        assert!(
            sim.dump_journal(&source_journal).is_empty(),
            "seed {seed}: failed start batch must not commit the source side"
        );
        assert!(
            sim.dump_journal(&workflow_journal).is_empty(),
            "seed {seed}: failed start batch must not materialize a workflow"
        );

        let after_restart = BoxedEventStore::new(sim.clone());
        assert!(matches!(
            commit_collection_start(
                &after_restart,
                source_append(&tenant, source_id, 0, "StartChecks"),
                &intent,
                &record,
            )
            .await
            .expect("retry start after restart"),
            CollectionLedgerCommitOutcome::Committed(_)
        ));

        let mut controlled = record.clone();
        let timeout_delivery_id = bind_test_timeout(&mut controlled);
        let (control, _) = controlled
            .request_control(
                CollectionRequestedOutcome::TimedOut,
                Some(&timeout_delivery_id),
                "TimeoutChecks".to_string(),
                2,
                serde_json::json!({"principal": "timer"}),
                None,
            )
            .expect("request timeout control");
        sim.inject_concurrency_violations(&workflow_journal, 1);
        assert!(matches!(
            commit_collection_control(
                &after_restart,
                source_append(&tenant, source_id, 1, "TimeoutChecks"),
                &control,
                1,
                &controlled,
            )
            .await,
            Err(PersistenceError::ConcurrencyViolation { .. })
        ));
        drop(after_restart);

        assert_eq!(
            sim.dump_journal(&source_journal).len(),
            1,
            "seed {seed}: failed control batch must not commit its source event"
        );
        assert_eq!(
            sim.dump_journal(&workflow_journal).len(),
            1,
            "seed {seed}: failed control batch must not advance the workflow"
        );

        let after_control_restart = BoxedEventStore::new(sim.clone());
        assert!(matches!(
            commit_collection_control(
                &after_control_restart,
                source_append(&tenant, source_id, 1, "TimeoutChecks"),
                &control,
                1,
                &controlled,
            )
            .await
            .expect("retry control after restart"),
            CollectionLedgerCommitOutcome::Committed(_)
        ));
        assert_eq!(
            load_collection_record(&after_control_restart, &tenant, &record.workflow_id)
                .await
                .expect("reload controlled workflow")
                .expect("workflow exists"),
            (controlled, 2),
            "seed {seed}: committed control must reconstruct exactly after restart"
        );
    }
}
