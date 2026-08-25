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
    PersistenceAppend {
        persistence_id: persistence_id.clone(),
        expected_sequence,
        events: vec![PersistenceEnvelope {
            sequence_nr: expected_sequence + 1,
            event_type: event_type.to_string(),
            payload: serde_json::json!({"application": "evidence"}),
            metadata: EventMetadata {
                event_id: sim_uuid(),
                causation_id: sim_uuid(),
                correlation_id: sim_uuid(),
                timestamp: sim_now(),
                actor_id: persistence_id,
            },
        }],
    }
}

mod model;
mod parity;
mod persistence;
mod restart_dst;
