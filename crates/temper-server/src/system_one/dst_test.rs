//! Seeded fault schedules around the production receipt and transition paths.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use serde_json::{Value, json};
use temper_jit::table::TransitionTable;
use temper_runtime::scheduler::install_deterministic_context;
use temper_store_sim::{SimEventStore, SimFaultConfig};

use super::evidence::json_digest;
use super::provider::SystemOneProvider;
use super::*;
use crate::entity_actor::effects::entity_authorization_precondition;
use crate::entity_actor::{EntityActor, EntityState, process_action_with_xref};
use crate::storage::BoxedEventStore;

const SPEC: &str = r#"
[automaton]
name = "Case"
states = ["Open", "Escalated"]
initial = "Open"
allow_indefinite_states = ["Open", "Escalated"]

[[state]]
name = "Messages"
type = "string"
initial = "Please connect me with a human."

[[action]]
name = "Escalate"
kind = "input"
from = ["Open"]
to = "Escalated"
guard = [{ type = "system_one", model = "jev-latest", state = { ref = "entity.Messages" }, questions = { human = { type = "noul", instructions = "Has a human been requested?" } }, assert = "answers.human.noul >= 0.8" }]
"#;

struct SeededProvider {
    seed: u64,
    calls: AtomicUsize,
}

#[async_trait]
impl SystemOneProvider for SeededProvider {
    async fn evaluate(&self, _tenant: &str, _request: &Value) -> Result<Value, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match self.seed % 7 {
            0 => Err("simulated provider unavailable".into()),
            1 => Ok(json!({"model":"jev-sim", "answers":{"human":{"type":"noul", "noul":0.2}}})),
            2 => Ok(json!({"model":"jev-sim", "answers":{"human":{"type":"noul", "noul":8}}})),
            _ => Ok(json!({"model":"jev-sim", "answers":{"human":{"type":"noul", "noul":0.9}}})),
        }
    }
}

fn initial(table: &TransitionTable) -> EntityState {
    EntityActor::build_initial_state(
        "Case",
        "case-1",
        table,
        &json!({"Messages":"Please connect me with a human."}),
    )
}

fn binding(table: &TransitionTable, state: &EntityState, attempt: &str) -> ReceiptBinding {
    ReceiptBinding {
        tenant: "tenant-a".into(),
        principal: "agent-a".into(),
        entity_type: "Case".into(),
        entity_id: "case-1".into(),
        action: "Escalate".into(),
        attempt_id: attempt.into(),
        expected_state: entity_authorization_precondition(state),
        expected_table: table_digest(table).unwrap(),
        params_digest: json_digest(&json!({})).unwrap(),
        guard_key: collect_guards(table, "Escalate")[0].key(),
        guard_slot: 0,
    }
}

fn evidence(receipt_id: String, receipt: &EvaluationReceipt) -> SystemOneEvidence {
    SystemOneEvidence {
        receipt_ids: vec![receipt_id],
        expected_state: receipt.binding.expected_state.clone(),
        expected_table: receipt.binding.expected_table.clone(),
        attempt_id: receipt.binding.attempt_id.clone(),
        params_digest: receipt.binding.params_digest.clone(),
        outcomes: BTreeMap::from([(receipt.binding.guard_key.clone(), receipt.passed)]),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn many_seed_receipt_and_freshness_invariants() {
    for seed in 1..=1024 {
        let _context = install_deterministic_context(seed);
        let sim = SimEventStore::new(
            seed,
            SimFaultConfig {
                write_failure_prob: 0.15,
                concurrency_violation_prob: 0.05,
                read_truncation_prob: 0.05,
                snapshot_failure_prob: 0.0,
            },
        );
        let store = BoxedEventStore::new(sim.clone());
        let provider = SeededProvider {
            seed,
            calls: AtomicUsize::new(0),
        };
        let table = TransitionTable::from_ioa_source(SPEC);
        let mut state = initial(&table);
        let guard = collect_guards(&table, "Escalate")[0];
        let request = guard.request(guard.resolve_state(&state.fields, &json!({})).unwrap());
        let before = state.clone();
        let result = resolve_receipt(
            &store,
            &provider,
            guard,
            binding(&table, &state, "attempt-1"),
            request.clone(),
        )
        .await;
        assert_eq!(
            state.status, before.status,
            "seed={seed}: inference mutated domain state"
        );
        // Environmental faults cannot produce a successful domain action.
        let Ok((id, receipt)) = result else { continue };
        let stable_faults = sim.disable_faults();
        let calls = provider.calls.load(Ordering::SeqCst);
        let duplicate = resolve_receipt(
            &store,
            &provider,
            guard,
            binding(&table, &state, "attempt-1"),
            request.clone(),
        )
        .await
        .unwrap();
        assert_eq!(
            duplicate.1.passed, receipt.passed,
            "seed={seed}: resampled receipt"
        );
        assert_eq!(
            provider.calls.load(Ordering::SeqCst),
            calls,
            "seed={seed}: retry called provider"
        );
        let evidence = evidence(id, &receipt);
        evidence
            .validate(&table, &state, "Escalate", &json!({}), Some("attempt-1"))
            .unwrap();
        let missing =
            process_action_with_xref(&mut state, &table, "Escalate", &json!({}), &BTreeMap::new());
        assert!(
            !missing.success,
            "seed={seed}: absent evidence enabled action"
        );
        let applied = process_action_with_xref(
            &mut state,
            &table,
            "Escalate",
            &json!({}),
            &evidence.outcomes,
        );
        assert_eq!(
            applied.success,
            receipt.passed && receipt.error.is_none(),
            "seed={seed}: invalid/negative answer enabled action"
        );
        let mut changed = before.clone();
        changed.fields["Messages"] = json!("Never mind, no human is wanted.");
        assert!(
            evidence
                .validate(&table, &changed, "Escalate", &json!({}), Some("attempt-1"))
                .is_err(),
            "seed={seed}: stale field evidence accepted"
        );
        changed = before.clone();
        changed.counters.insert("new_counter".into(), 1);
        assert!(
            evidence
                .validate(&table, &changed, "Escalate", &json!({}), Some("attempt-1"))
                .is_err(),
            "seed={seed}: stale runtime counter evidence accepted"
        );
        changed = before.clone();
        changed.booleans.insert("new_boolean".into(), true);
        assert!(
            evidence
                .validate(&table, &changed, "Escalate", &json!({}), Some("attempt-1"))
                .is_err(),
            "seed={seed}: stale runtime boolean evidence accepted"
        );
        changed = before.clone();
        changed.sequence_nr += 1;
        assert!(
            evidence
                .validate(&table, &changed, "Escalate", &json!({}), Some("attempt-1"))
                .is_err(),
            "seed={seed}: stale sequence evidence accepted"
        );
        let changed_table = TransitionTable::from_ioa_source(&SPEC.replace(
            "Has a human been requested?",
            "Is a human explicitly requested?",
        ));
        assert!(
            evidence
                .validate(
                    &changed_table,
                    &before,
                    "Escalate",
                    &json!({}),
                    Some("attempt-1")
                )
                .is_err(),
            "seed={seed}: old-spec evidence accepted"
        );
        assert!(
            evidence
                .validate(
                    &table,
                    &before,
                    "Escalate",
                    &json!({"extra":true}),
                    Some("attempt-1")
                )
                .is_err(),
            "seed={seed}: evidence accepted different params"
        );
        assert!(
            evidence
                .validate(
                    &table,
                    &before,
                    "Escalate",
                    &json!({}),
                    Some("another-attempt")
                )
                .is_err(),
            "seed={seed}: evidence accepted different attempt"
        );
        // Reconstruct the receipt through the persisted journal, as after restart.
        let recovered = resolve_receipt(
            &store,
            &provider,
            guard,
            binding(&table, &before, "attempt-1"),
            request,
        )
        .await
        .unwrap();
        assert_eq!(recovered.1.passed, receipt.passed);
        assert_eq!(provider.calls.load(Ordering::SeqCst), calls);
        sim.restore_faults(stable_faults);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn same_seed_produces_identical_evaluation_journal() {
    async fn scenario(seed: u64) -> Value {
        let _context = install_deterministic_context(seed);
        let sim = SimEventStore::new(
            seed,
            SimFaultConfig {
                write_failure_prob: 0.1,
                concurrency_violation_prob: 0.1,
                read_truncation_prob: 0.1,
                snapshot_failure_prob: 0.0,
            },
        );
        let store = BoxedEventStore::new(sim.clone());
        let provider = SeededProvider {
            seed,
            calls: AtomicUsize::new(0),
        };
        let table = TransitionTable::from_ioa_source(SPEC);
        let state = initial(&table);
        let guard = collect_guards(&table, "Escalate")[0];
        let request = guard.request(guard.resolve_state(&state.fields, &json!({})).unwrap());
        let result = resolve_receipt(
            &store,
            &provider,
            guard,
            binding(&table, &state, "attempt"),
            request,
        )
        .await;
        sim.disable_faults();
        match result {
            Ok((id, receipt)) => {
                json!({"id":id,"receipt":receipt,"journal":store.read_events(&id,0).await.unwrap(),"calls":provider.calls.load(Ordering::SeqCst)})
            }
            Err(error) => json!({"error":error,"calls":provider.calls.load(Ordering::SeqCst)}),
        }
    }
    for seed in 1..=64 {
        assert_eq!(
            scenario(seed).await,
            scenario(seed).await,
            "seed={seed}: evaluation or journal is nondeterministic"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn tenant_receipts_and_attempt_inputs_are_isolated() {
    let _context = install_deterministic_context(700);
    let store = BoxedEventStore::new(SimEventStore::no_faults(700));
    let provider = SeededProvider {
        seed: 3,
        calls: AtomicUsize::new(0),
    };
    let table = TransitionTable::from_ioa_source(SPEC);
    let state = initial(&table);
    let guard = collect_guards(&table, "Escalate")[0];
    let request = guard.request(guard.resolve_state(&state.fields, &json!({})).unwrap());
    let original = binding(&table, &state, "attempt");
    let a = resolve_receipt(&store, &provider, guard, original.clone(), request.clone())
        .await
        .unwrap();
    let mut other_tenant = original.clone();
    other_tenant.tenant = "tenant-b".into();
    let b = resolve_receipt(&store, &provider, guard, other_tenant, request.clone())
        .await
        .unwrap();
    assert_ne!(a.0, b.0);
    let mut changed = original;
    changed.expected_state = "stale".into();
    assert!(
        resolve_receipt(&store, &provider, guard, changed, request)
            .await
            .is_err()
    );
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn attempt_manifest_binds_completed_retries_and_structural_spec_changes() {
    let _context = install_deterministic_context(701);
    let store = BoxedEventStore::new(SimEventStore::no_faults(701));
    let tenant = temper_runtime::tenant::TenantId::new("tenant-a");
    let table = TransitionTable::from_ioa_source(SPEC);
    let state = initial(&table);
    bind_attempt(
        &store,
        AttemptInput {
            tenant: &tenant,
            table: &table,
            state: &state,
            action: "Escalate",
            params: &json!({}),
            principal: "principal-a",
            attempt: "attempt",
        },
    )
    .await
    .unwrap();
    let mut completed = state.clone();
    completed.status = "Escalated".into();
    completed.sequence_nr += 1;
    assert!(
        check_attempt(
            &store,
            AttemptInput {
                tenant: &tenant,
                table: &table,
                state: &completed,
                action: "Escalate",
                params: &json!({}),
                principal: "principal-a",
                attempt: "attempt",
            },
            true
        )
        .await
        .unwrap()
    );
    assert!(
        check_attempt(
            &store,
            AttemptInput {
                tenant: &tenant,
                table: &table,
                state: &completed,
                action: "Escalate",
                params: &json!({"changed":true}),
                principal: "principal-a",
                attempt: "attempt",
            },
            true
        )
        .await
        .is_err()
    );
    assert!(
        check_attempt(
            &store,
            AttemptInput {
                tenant: &tenant,
                table: &table,
                state: &completed,
                action: "Escalate",
                params: &json!({}),
                principal: "principal-b",
                attempt: "attempt",
            },
            true
        )
        .await
        .is_err()
    );
    assert!(
        check_attempt(
            &store,
            AttemptInput {
                tenant: &tenant,
                table: &table,
                state: &completed,
                action: "Other",
                params: &json!({}),
                principal: "principal-a",
                attempt: "attempt",
            },
            true
        )
        .await
        .is_err()
    );
    assert!(
        check_attempt(
            &store,
            AttemptInput {
                tenant: &tenant,
                table: &table,
                state: &completed,
                action: "Escalate",
                params: &json!({}),
                principal: "principal-a",
                attempt: "attempt",
            },
            false
        )
        .await
        .is_err()
    );
    let guard_line = SPEC
        .lines()
        .find(|line| line.starts_with("guard ="))
        .unwrap();
    let removed = TransitionTable::from_ioa_source(&SPEC.replace(guard_line, "guard = []"));
    assert!(collect_guards(&removed, "Escalate").is_empty());
    assert!(
        check_attempt(
            &store,
            AttemptInput {
                tenant: &tenant,
                table: &removed,
                state: &state,
                action: "Escalate",
                params: &json!({}),
                principal: "principal-a",
                attempt: "attempt",
            },
            false
        )
        .await
        .is_err()
    );
    assert!(
        check_attempt(
            &store,
            AttemptInput {
                tenant: &tenant,
                table: &removed,
                state: &completed,
                action: "Escalate",
                params: &json!({}),
                principal: "principal-a",
                attempt: "attempt",
            },
            true
        )
        .await
        .is_err()
    );
}
