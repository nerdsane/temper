use super::{
    AwaitedExecutionIdentityV1, AwaitedExecutionPhase, DeliveryKind, DurableFailureKind,
    MAX_AWAITED_CALLBACK_EVIDENCE_BYTES, PersistedReactionIntent, REACTION_INTENTS_FIELD,
    ReactionDeliveryRecord, ReactionDeliveryStatus, ReactionReceipt, append_delivery_record,
    attach_intents, attach_receipt, delivery_failure_envelope, delivery_journal_id,
    extract_intents, extract_receipt, load_delivery_record, stable_delivery_id,
    state_timeout_intents,
};
use chrono::{Duration, TimeZone, Utc};
use serde_json::json;
use temper_runtime::persistence::PersistenceError;
use temper_runtime::scheduler::install_deterministic_context;
use temper_store_sim::SimEventStore;

const TIMEOUT_IOA: &str = r#"
[automaton]
name = "Ticket"
states = ["Open", "Assigned"]
initial = "Open"
allow_indefinite_states = ["Assigned"]

[[action]]
name = "Heartbeat"
kind = "input"
from = ["Open"]
to = "Open"

[[action]]
name = "Assign"
kind = "input"
from = ["Open"]
to = "Assigned"

[[state_timeout]]
state = "Open"
after_seconds = 30
on_timeout = "Assign"
reset_on = ["Heartbeat"]
params = { reason = "deadline" }
"#;

use crate::storage::BoxedEventStore;
use crate::trigger::ReactionFailureKind;

fn intent() -> PersistedReactionIntent {
    PersistedReactionIntent {
        kind: DeliveryKind::Reaction,
        delivery_id: "reaction-v1-a".to_string(),
        root_delivery_id: "reaction-v1-a".to_string(),
        tenant: "tenant-a".to_string(),
        source_entity_type: "Order".to_string(),
        source_entity_id: "order-7".to_string(),
        source_action: "Confirm".to_string(),
        source_sequence: 42,
        source_to_state: "Confirmed".to_string(),
        source_fields: json!({"payment_id": "payment-9"}),
        source_stream_descriptor: None,
        guard_passed: true,
        target_entity_id: Some("payment-9".to_string()),
        trigger_name: "create-payment".to_string(),
        trigger_index: 0,
        depth: 0,
        rule: json!({"name": "create-payment"}),
        authority: json!({"principal": {"id": "User::alice"}}),
        created_at: Utc.timestamp_opt(1_800_000_000, 0).single().unwrap(),
        not_before: None,
        state_timeout: None,
        collection: None,
        schema_pin: None,
    }
}

#[test]
fn acknowledgement_loss_is_ambiguous_and_uses_durable_identity() {
    let intent = intent();
    let envelope = delivery_failure_envelope(
        &intent,
        5,
        DurableFailureKind::Reaction(ReactionFailureKind::AcknowledgementLost),
        Some("diagnostic only"),
        None,
    )
    .expect("valid envelope");

    assert_eq!(
        envelope.category,
        temper_failure::FailureCategory::Ambiguous
    );
    assert_eq!(envelope.outcome, temper_failure::FailureOutcome::Unknown);
    assert_eq!(envelope.operation.id.as_str(), intent.delivery_id);
    assert_eq!(envelope.operation.attempt.get(), 5);
    assert_eq!(
        envelope.provenance.source,
        temper_failure::FailureSource::Reaction
    );
    assert!(envelope.message.is_some());
}

#[test]
fn authorization_denial_retains_bounded_decision_identity() {
    let intent = intent();
    let envelope = delivery_failure_envelope(
        &intent,
        1,
        DurableFailureKind::Reaction(ReactionFailureKind::AuthorizationDenied),
        Some("diagnostic only"),
        Some("cedar:policies:policy-a,policy-b"),
    )
    .expect("valid authorization envelope");

    assert_eq!(
        envelope.category,
        temper_failure::FailureCategory::Authorization
    );
    assert_eq!(
        serde_json::to_value(&envelope).expect("serializable envelope")["details"]["decision_id"]["value"],
        "cedar:policies:policy-a,policy-b"
    );
}

fn timeout_intents(
    table: &temper_jit::table::TransitionTable,
    event: &crate::entity_actor::EntityEvent,
    source_sequence: u64,
    authority: Option<&serde_json::Value>,
) -> Result<Vec<PersistedReactionIntent>, String> {
    state_timeout_intents(super::StateTimeoutIntentContext {
        tenant: "tenant-a",
        entity_type: "Ticket",
        entity_id: "ticket-1",
        source_sequence,
        event,
        source_fields: &json!({"Id": "ticket-1"}),
        table,
        schema_pin: None,
        triggering_authority: authority,
        durable_idempotency_evidence: &std::collections::BTreeMap::new(),
    })
}

#[test]
fn timeout_intent_fixes_deadline_and_schema_to_committed_event() {
    let table = temper_jit::table::TransitionTable::from_ioa_source(TIMEOUT_IOA);
    let timestamp = Utc.timestamp_opt(1_800_000_000, 0).single().unwrap();
    let event = crate::entity_actor::EntityEvent {
        action: "Created".to_string(),
        from_status: String::new(),
        to_status: "Open".to_string(),
        timestamp,
        params: json!({}),
        idempotency_key: None,
    };
    let intents = timeout_intents(&table, &event, 1, None).expect("timeout intent");
    assert_eq!(intents.len(), 1);
    let timeout = &intents[0];
    assert_eq!(timeout.kind, DeliveryKind::StateTimeout);
    assert_eq!(timeout.not_before, Some(timestamp + Duration::seconds(30)));
    assert_eq!(timeout.target_entity_id.as_deref(), Some("ticket-1"));
    let clock = timeout.state_timeout.as_ref().expect("clock evidence");
    assert_eq!(clock.clock_sequence, 1);
    assert_eq!(clock.state, "Open");
    assert!(clock.schema_digest.starts_with("sha256:"));

    let pending = ReactionDeliveryRecord::pending(timeout.clone());
    assert_eq!(pending.next_attempt_at, timeout.not_before);
}

#[test]
fn transition_timeout_retains_exact_triggering_authority() {
    let table = temper_jit::table::TransitionTable::from_ioa_source(TIMEOUT_IOA);
    let timestamp = Utc.timestamp_opt(1_800_000_000, 0).single().unwrap();
    let event = crate::entity_actor::EntityEvent {
        action: "Heartbeat".to_string(),
        from_status: "Open".to_string(),
        to_status: "Open".to_string(),
        timestamp,
        params: json!({}),
        idempotency_key: None,
    };
    let authority = serde_json::to_value(temper_authz::SecurityContext::from_resolved_identity(
        "operator-1",
        "operator",
        None,
    ))
    .unwrap();
    let intent = timeout_intents(&table, &event, 2, Some(&authority))
        .unwrap()
        .pop()
        .expect("reset should commit a timeout clock");
    let rule: crate::trigger::ReactionRule = serde_json::from_value(intent.rule.clone()).unwrap();

    assert_eq!(intent.authority, authority);
    assert_eq!(
        rule.principal, None,
        "synthetic rule must not replace authority"
    );
}

#[test]
fn timeout_intent_is_created_only_by_entry_or_reset_evidence() {
    let table = temper_jit::table::TransitionTable::from_ioa_source(TIMEOUT_IOA);
    let timestamp = Utc.timestamp_opt(1_800_000_000, 0).single().unwrap();
    let same_state = |action: &str| crate::entity_actor::EntityEvent {
        action: action.to_string(),
        from_status: "Open".to_string(),
        to_status: "Open".to_string(),
        timestamp,
        params: json!({}),
        idempotency_key: None,
    };
    assert!(
        timeout_intents(&table, &same_state("Unrelated"), 2, None)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        timeout_intents(&table, &same_state("Heartbeat"), 3, None)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn timeout_deadline_remains_absolute_across_clock_skew_and_forward_jumps() {
    let table = temper_jit::table::TransitionTable::from_ioa_source(TIMEOUT_IOA);
    let entered_at = Utc.timestamp_opt(1_800_000_000, 0).single().unwrap();
    let event = crate::entity_actor::EntityEvent {
        action: "Created".to_string(),
        from_status: String::new(),
        to_status: "Open".to_string(),
        timestamp: entered_at,
        params: json!({}),
        idempotency_key: None,
    };
    let intent = timeout_intents(&table, &event, 1, None)
        .unwrap()
        .pop()
        .expect("timeout intent");
    let deadline = entered_at + Duration::seconds(30);
    let mut record = ReactionDeliveryRecord::pending(intent);

    assert!(
        record
            .claim(deadline - Duration::seconds(1), Duration::seconds(5))
            .is_err(),
        "a backward-skewed observer cannot claim before the committed deadline"
    );
    assert_eq!(record.next_attempt_at, Some(deadline));
    assert_eq!(
        record
            .claim(deadline + Duration::hours(12), Duration::seconds(5))
            .expect("a forward jump makes the original deadline eligible"),
        1
    );
}

#[test]
fn delivery_identity_is_stable_and_binds_source_sequence_and_trigger() {
    let first = stable_delivery_id(
        "tenant-a",
        "Order",
        "order-7",
        "Confirm",
        42,
        "create-payment",
        0,
    );
    let repeated = stable_delivery_id(
        "tenant-a",
        "Order",
        "order-7",
        "Confirm",
        42,
        "create-payment",
        0,
    );
    let next_sequence = stable_delivery_id(
        "tenant-a",
        "Order",
        "order-7",
        "Confirm",
        43,
        "create-payment",
        0,
    );
    let next_trigger = stable_delivery_id(
        "tenant-a",
        "Order",
        "order-7",
        "Confirm",
        42,
        "audit-order",
        1,
    );

    assert_eq!(first, repeated);
    assert_ne!(first, next_sequence);
    assert_ne!(first, next_trigger);
    assert!(first.starts_with("reaction-v1-"));
    assert_eq!(first.len(), "reaction-v1-".len() + 64);
}

#[test]
fn intents_round_trip_inside_the_atomic_source_event_payload() {
    let mut payload = json!({"action": "Confirm", "params": {}});
    attach_intents(&mut payload, std::slice::from_ref(&intent())).unwrap();

    assert!(payload.get(REACTION_INTENTS_FIELD).is_some());
    assert_eq!(extract_intents(&payload).unwrap(), vec![intent()]);
}

#[test]
fn receipt_round_trips_inside_the_atomic_target_event_payload() {
    let mut payload = json!({"action": "Create", "params": {}});
    let receipt = ReactionReceipt {
        delivery_id: "reaction-v1-a".to_string(),
        fencing_token: 3,
        received_at: Utc.timestamp_opt(1_800_000_001, 0).single().unwrap(),
        state_timeout_state: None,
        schema_pin: None,
        collection: None,
        awaited_callback: None,
    };

    attach_receipt(&mut payload, &receipt).unwrap();
    assert_eq!(extract_receipt(&payload).unwrap(), Some(receipt));
}

#[test]
fn awaited_execution_requires_exact_fence_and_bounded_callback_evidence() {
    let now = Utc.timestamp_opt(1_800_000_000, 0).single().unwrap();
    let deadline = now + Duration::minutes(2);
    let mut delivery = ReactionDeliveryRecord::pending(intent());
    let fence = delivery.claim(now, Duration::seconds(30)).unwrap();
    delivery.begin_dispatch(fence).unwrap();
    let identity = AwaitedExecutionIdentityV1 {
        execution_id: "execution-1".to_string(),
        integration_name: "check".to_string(),
        module_name: "check.wasm".to_string(),
        module_digest: "sha256:abc".to_string(),
        success_callback: "Checked".to_string(),
        failure_callback: Some("CheckFailed".to_string()),
        schema_pin: None,
        deadline,
    };
    delivery
        .bind_awaited_execution(fence, identity, now)
        .unwrap();
    for (elapsed, expected) in [(20, 50), (40, 70), (65, 95)] {
        assert_eq!(
            delivery
                .renew_awaited_execution(
                    fence,
                    "execution-1",
                    now + Duration::seconds(elapsed),
                    Duration::seconds(30),
                )
                .unwrap(),
            now + Duration::seconds(expected)
        );
    }
    assert_eq!(
        delivery
            .renew_awaited_execution(
                fence,
                "execution-1",
                now + Duration::seconds(90),
                Duration::seconds(30),
            )
            .unwrap(),
        deadline
    );
    assert!(
        delivery
            .record_awaited_completion(
                fence + 1,
                "execution-1",
                true,
                Some("Checked"),
                Some(json!({"ok": true})),
                None,
                now,
            )
            .is_err()
    );
    delivery
        .record_awaited_completion(
            fence,
            "execution-1",
            true,
            Some("Checked"),
            Some(json!({"ok": true})),
            None,
            now,
        )
        .unwrap();
    delivery
        .accept_awaited_callback(fence, "execution-1", "Checked", 7, now)
        .unwrap();
    assert_eq!(
        delivery.awaited_execution.as_ref().unwrap().phase,
        AwaitedExecutionPhase::CallbackAccepted
    );

    let mut oversized = ReactionDeliveryRecord::pending(intent());
    let fence = oversized.claim(now, Duration::seconds(30)).unwrap();
    oversized.begin_dispatch(fence).unwrap();
    oversized
        .bind_awaited_execution(
            fence,
            AwaitedExecutionIdentityV1 {
                execution_id: "execution-2".to_string(),
                integration_name: "check".to_string(),
                module_name: "check.wasm".to_string(),
                module_digest: "sha256:abc".to_string(),
                success_callback: "Checked".to_string(),
                failure_callback: None,
                schema_pin: None,
                deadline,
            },
            now,
        )
        .unwrap();
    assert!(
        oversized
            .record_awaited_completion(
                fence,
                "execution-2",
                true,
                Some("Checked"),
                Some(json!({"payload": "x".repeat(MAX_AWAITED_CALLBACK_EVIDENCE_BYTES)})),
                None,
                now,
            )
            .is_err()
    );
}

#[test]
fn expired_awaited_owner_cannot_complete_after_fenced_takeover() {
    let now = Utc.timestamp_opt(1_800_000_000, 0).single().unwrap();
    let deadline = now + Duration::minutes(2);
    let identity = AwaitedExecutionIdentityV1 {
        execution_id: "execution-takeover".to_string(),
        integration_name: "check".to_string(),
        module_name: "check.wasm".to_string(),
        module_digest: "sha256:abc".to_string(),
        success_callback: "Checked".to_string(),
        failure_callback: None,
        schema_pin: None,
        deadline,
    };
    let mut delivery = ReactionDeliveryRecord::pending(intent());
    let old_fence = delivery.claim(now, Duration::seconds(30)).unwrap();
    delivery.begin_dispatch(old_fence).unwrap();
    delivery
        .bind_awaited_execution(old_fence, identity.clone(), now)
        .unwrap();

    let takeover_at = now + Duration::seconds(31);
    assert!(
        delivery
            .record_awaited_completion(
                old_fence,
                "execution-takeover",
                true,
                Some("Checked"),
                Some(json!({"late": true})),
                None,
                takeover_at,
            )
            .is_err(),
        "an expired owner must fail before recovery raises the fence"
    );
    assert!(delivery.recover_expired_lease(takeover_at));
    let new_fence = delivery.claim(takeover_at, Duration::seconds(30)).unwrap();
    delivery.begin_dispatch(new_fence).unwrap();
    delivery
        .bind_awaited_execution(new_fence, identity, takeover_at)
        .unwrap();
    assert_eq!(new_fence, old_fence + 1);
    assert!(
        delivery
            .record_awaited_completion(
                old_fence,
                "execution-takeover",
                true,
                Some("Checked"),
                Some(json!({"ok": true})),
                None,
                takeover_at,
            )
            .is_err()
    );
    delivery
        .record_awaited_completion(
            new_fence,
            "execution-takeover",
            true,
            Some("Checked"),
            Some(json!({"ok": true})),
            None,
            takeover_at,
        )
        .unwrap();
}

#[test]
fn lifecycle_uses_fenced_leases_and_bounds_manual_retry() {
    let now = Utc.timestamp_opt(1_800_000_000, 0).single().unwrap();
    let mut delivery = ReactionDeliveryRecord::pending(intent());

    let first_fence = delivery.claim(now, Duration::seconds(30)).unwrap();
    assert_eq!(first_fence, 1);
    assert_eq!(delivery.status, ReactionDeliveryStatus::Claimed);
    assert!(delivery.claim(now, Duration::seconds(30)).is_err());

    delivery.recover_expired_lease(now + Duration::seconds(31));
    assert_eq!(delivery.status, ReactionDeliveryStatus::Pending);
    let second_fence = delivery
        .claim(now + Duration::seconds(31), Duration::seconds(30))
        .unwrap();
    assert_eq!(second_fence, 2);
    assert!(delivery.begin_dispatch(first_fence).is_err());
    delivery.begin_dispatch(second_fence).unwrap();
    delivery
        .dead_letter(second_fence, true, "temporary outage")
        .unwrap();
    delivery.failure = Some(
        delivery_failure_envelope(
            &delivery.intent,
            delivery.attempts,
            DurableFailureKind::Reaction(ReactionFailureKind::MailboxCapacityExhausted),
            delivery.last_error.as_deref(),
            None,
        )
        .expect("valid transient envelope"),
    );

    for expected in 1..=3 {
        assert_eq!(delivery.request_manual_retry().unwrap(), expected);
        assert!(delivery.failure.is_none());
        delivery.status = ReactionDeliveryStatus::DeadLettered;
        delivery.transient_failure = true;
    }
    assert!(delivery.request_manual_retry().is_err());
}

#[tokio::test]
async fn delivery_journal_restores_state_and_fences_competing_writers() {
    let (_guard, _clock, _ids) = install_deterministic_context(414);
    let inner = SimEventStore::no_faults(414);
    let store = BoxedEventStore::new(inner.clone());
    let mut record = ReactionDeliveryRecord::pending(intent());
    let now = Utc.timestamp_opt(1_800_000_000, 0).single().unwrap();
    record.claim(now, Duration::seconds(30)).unwrap();

    let sequence = append_delivery_record(&store, 0, &record).await.unwrap();
    assert_eq!(sequence, 1);
    let restored = load_delivery_record(&store, intent()).await.unwrap();
    assert_eq!(restored, (record.clone(), 1));

    let conflict = append_delivery_record(&store, 0, &record)
        .await
        .unwrap_err();
    assert!(matches!(
        conflict,
        PersistenceError::ConcurrencyViolation { .. }
    ));
    assert_eq!(
        delivery_journal_id(&intent()),
        "tenant-a:_ReactionDelivery:reaction-v1-a"
    );
}

async fn prove_awaited_evidence_round_trip(store: BoxedEventStore, tenant: String) {
    let now = Utc.timestamp_opt(1_800_000_000, 0).single().unwrap();
    let mut persisted_intent = intent();
    persisted_intent.tenant = tenant;
    persisted_intent.delivery_id = format!("reaction-v1-{}", uuid::Uuid::new_v4());
    persisted_intent.root_delivery_id = persisted_intent.delivery_id.clone();
    let mut record = ReactionDeliveryRecord::pending(persisted_intent.clone());
    let fence = record.claim(now, Duration::seconds(30)).unwrap();
    record.begin_dispatch(fence).unwrap();
    record
        .bind_awaited_execution(
            fence,
            AwaitedExecutionIdentityV1 {
                execution_id: "backend-execution".to_string(),
                integration_name: "check".to_string(),
                module_name: "check.wasm".to_string(),
                module_digest: "sha256:backend".to_string(),
                success_callback: "Checked".to_string(),
                failure_callback: None,
                schema_pin: None,
                deadline: now + Duration::minutes(1),
            },
            now,
        )
        .unwrap();
    record
        .record_awaited_completion(
            fence,
            "backend-execution",
            true,
            Some("Checked"),
            Some(json!({"backend": true})),
            None,
            now,
        )
        .unwrap();
    append_delivery_record(&store, 0, &record).await.unwrap();
    let (restored, _) = load_delivery_record(&store, persisted_intent)
        .await
        .unwrap();
    assert_eq!(restored, record);
}

#[tokio::test]
async fn turso_round_trips_awaited_execution_evidence() {
    let (_guard, _clock, _ids) = install_deterministic_context(415);
    let dir = tempfile::tempdir().unwrap();
    let db_url = format!("file:{}", dir.path().join("awaited.db").display());
    let store = temper_store_turso::TursoEventStore::new(&db_url, None)
        .await
        .unwrap();
    prove_awaited_evidence_round_trip(BoxedEventStore::new(store), "awaited-turso".to_string())
        .await;
}
