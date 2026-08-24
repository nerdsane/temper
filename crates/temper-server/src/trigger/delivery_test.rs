use super::{
    DeliveryKind, PersistedReactionIntent, REACTION_INTENTS_FIELD, ReactionDeliveryRecord,
    ReactionDeliveryStatus, ReactionReceipt, append_delivery_record, attach_intents,
    attach_receipt, delivery_journal_id, extract_intents, extract_receipt, load_delivery_record,
    stable_delivery_id, state_timeout_intents,
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
        schema_pin: None,
    }
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
    };

    attach_receipt(&mut payload, &receipt).unwrap();
    assert_eq!(extract_receipt(&payload).unwrap(), Some(receipt));
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

    for expected in 1..=3 {
        assert_eq!(delivery.request_manual_retry().unwrap(), expected);
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
