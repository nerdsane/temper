use temper_runtime::persistence::{
    EventMetadata, PersistenceAppend, PersistenceEnvelope, PersistenceError,
};
use temper_runtime::scheduler::{sim_now, sim_uuid};

use super::*;
use crate::storage::BoxedEventStore;
use crate::trigger::delivery::ReactionDeliveryStatus;

fn budgets() -> CollectionWorkflowBudgets {
    CollectionWorkflowBudgets {
        max_members: 4,
        max_concurrency: 2,
        max_attempts: 3,
    }
}

fn start(tenant: &str, source_id: &str, roster: &[&str]) -> CollectionWorkflowStart {
    CollectionWorkflowStart {
        tenant: tenant.to_string(),
        source_entity_type: "Batch".to_string(),
        source_entity_id: source_id.to_string(),
        declaration_name: "run_checks".to_string(),
        source_action: "StartChecks".to_string(),
        source_sequence: 1,
        schema_digest: "sha256:0123456789abcdef".to_string(),
        schema_pin: None,
        authority: serde_json::json!({"principal": "test-agent"}),
        roster: roster.iter().map(|value| (*value).to_string()).collect(),
        budgets: budgets(),
    }
}

fn source_append(
    tenant: &str,
    source_id: &str,
    expected_sequence: u64,
    event_type: &str,
) -> PersistenceAppend {
    let persistence_id = format!("{tenant}:Batch:{source_id}");
    let mut payload = serde_json::json!({"application": "evidence"});
    if event_type == "StartChecks" {
        let timeout = test_timeout_intent(tenant, source_id);
        crate::trigger::delivery::attach_intents(&mut payload, &[timeout]).unwrap();
    } else if event_type == "TimeoutChecks" {
        let timeout = test_timeout_intent(tenant, source_id);
        crate::trigger::delivery::attach_receipt(
            &mut payload,
            &crate::trigger::delivery::ReactionReceipt {
                delivery_id: timeout.delivery_id,
                fencing_token: 1,
                received_at: sim_now(),
                state_timeout_state: Some("Running".to_string()),
                schema_pin: None,
                collection: None,
                awaited_callback: None,
            },
        )
        .unwrap();
    }
    PersistenceAppend {
        persistence_id: persistence_id.clone(),
        expected_sequence,
        events: vec![PersistenceEnvelope {
            sequence_nr: expected_sequence + 1,
            event_type: event_type.to_string(),
            payload,
            metadata: EventMetadata {
                event_id: sim_uuid(),
                causation_id: sim_uuid(),
                correlation_id: sim_uuid(),
                timestamp: sim_now(),
                actor_id: persistence_id,
            },
        }],
        key_rows: Vec::new(),
        vector_rows: Vec::new(),
        reconcile_vectors: false,
    }
}

fn test_timeout_intent(
    tenant: &str,
    source_id: &str,
) -> crate::trigger::delivery::PersistedReactionIntent {
    use crate::trigger::delivery::{
        DeliveryKind, PersistedReactionIntent, StateTimeoutPrecondition,
    };
    let delivery_id = format!("timeout-{tenant}-{source_id}");
    let rule = crate::trigger::types::ReactionRule {
        name: "state-timeout:test".to_string(),
        when: crate::trigger::types::ReactionTrigger {
            entity_type: "Batch".to_string(),
            action: Some("StartChecks".to_string()),
            to_state: Some("Running".to_string()),
            guard: None,
        },
        then: crate::trigger::types::ReactionTarget {
            entity_type: "Batch".to_string(),
            action: "TimeoutChecks".to_string(),
            params: serde_json::json!({}),
            params_from: std::collections::BTreeMap::new(),
        },
        resolve_target: crate::trigger::types::TargetResolver::SameId,
        principal: None,
        drop_ok: false,
    };
    PersistedReactionIntent {
        kind: DeliveryKind::StateTimeout,
        root_delivery_id: delivery_id.clone(),
        delivery_id,
        tenant: tenant.to_string(),
        source_entity_type: "Batch".to_string(),
        source_entity_id: source_id.to_string(),
        source_action: "StartChecks".to_string(),
        source_sequence: 1,
        source_to_state: "Running".to_string(),
        source_fields: serde_json::json!({}),
        guard_passed: true,
        target_entity_id: Some(source_id.to_string()),
        trigger_name: "state-timeout:test".to_string(),
        trigger_index: 0,
        depth: 0,
        rule: serde_json::to_value(rule).unwrap(),
        authority: serde_json::json!({"principal": "timeout-scheduler"}),
        created_at: sim_now(),
        not_before: Some(sim_now() + chrono::Duration::seconds(60)),
        state_timeout: Some(StateTimeoutPrecondition {
            declaration_id: "timeout-declaration".to_string(),
            state: "Running".to_string(),
            clock_sequence: 1,
            schema_digest: "sha256:0123456789abcdef".to_string(),
            reset_on: Vec::new(),
            max_occurrences: 1,
            occurrence_ordinal: 1,
        }),
        collection: None,
        schema_pin: None,
    }
}

fn bind_test_timeout(record: &mut CollectionWorkflowRecordV1) -> String {
    let intent = test_timeout_intent(&record.tenant, &record.source_entity_id);
    let clock = intent.state_timeout.as_ref().unwrap();
    let delivery_id = intent.delivery_id.clone();
    record
        .bind_timeout(CollectionTimeoutBinding {
            delivery_id: delivery_id.clone(),
            timeout_action: "TimeoutChecks".to_string(),
            state: clock.state.clone(),
            deadline: intent.not_before.unwrap(),
            declaration_id: clock.declaration_id.clone(),
            clock_sequence: clock.clock_sequence,
            schema_digest: clock.schema_digest.clone(),
        })
        .unwrap();
    delivery_id
}

mod execution;
mod model;
mod parity;
mod persistence;
mod restart_dst;
