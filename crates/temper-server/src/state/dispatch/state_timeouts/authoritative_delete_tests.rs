//! Authoritative tombstone discovery versus live timeout ownership.

use temper_runtime::ActorSystem;
use temper_runtime::persistence::{EventMetadata, EventStore, PersistenceEnvelope};
use temper_runtime::scheduler::{install_deterministic_context, sim_now, sim_uuid};
use temper_runtime::tenant::TenantId;
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;

use crate::registry::SpecRegistry;
use crate::state::ServerState;
use crate::storage::StorageStack;

const TICKET_CSDL: &str = include_str!("../../../../../../test-fixtures/specs/model.csdl.xml");

const TIMED_TICKET_IOA: &str = r#"
[automaton]
name = "Ticket"
states = ["Open", "TimedOut", "Deleted"]
initial = "Open"
allow_indefinite_states = ["TimedOut", "Deleted"]

[[action]]
name = "TimeoutFail"
kind = "internal"
from = ["Open"]
to = "TimedOut"

[[state_timeout]]
state = "Open"
after_seconds = 60
on_timeout = "TimeoutFail"
"#;

#[tokio::test(start_paused = true)]
async fn authoritative_legacy_tombstone_retires_live_timeout_owner() {
    let (_guard, _clock, _ids) = install_deterministic_context(247);
    let tenant = TenantId::default();
    let entity_id = "external-delete-retires-timeout";
    let persistence_id = format!("{tenant}:Ticket:{entity_id}");
    let store = SimEventStore::no_faults(247);
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        tenant.as_str(),
        parse_csdl(TICKET_CSDL).expect("ticket CSDL parses"),
        TICKET_CSDL.to_string(),
        &[("Ticket", TIMED_TICKET_IOA)],
    );
    let mut server =
        ServerState::from_registry(ActorSystem::new("authoritative-delete-timeout"), registry);
    server.set_storage_stack(StorageStack::from_sim(store.clone(), None));

    let created = server
        .get_or_create_tenant_entity(&tenant, "Ticket", entity_id, serde_json::json!({}))
        .await
        .expect("create timed entity");
    for _ in 0..64 {
        if server.state_timeout_tracker.pending_snapshot() == vec![("Ticket".to_string(), 1)] {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(
        server.state_timeout_tracker.pending_snapshot(),
        vec![("Ticket".to_string(), 1)],
        "the resident Open actor owns a live timeout before external deletion"
    );

    store
        .append(
            &persistence_id,
            created.state.sequence_nr,
            &[
                PersistenceEnvelope {
                    sequence_nr: 0,
                    event_type: "Delete".to_string(),
                    payload: serde_json::json!({
                        "action": "Delete",
                        "from_status": "Open",
                        "to_status": "Deleted",
                        "timestamp": sim_now(),
                        "params": {}
                    }),
                    metadata: EventMetadata {
                        event_id: sim_uuid(),
                        causation_id: sim_uuid(),
                        correlation_id: sim_uuid(),
                        timestamp: sim_now(),
                        actor_id: persistence_id.clone(),
                    },
                },
                PersistenceEnvelope {
                    sequence_nr: 0,
                    event_type: "Transitioned".to_string(),
                    payload: serde_json::json!({
                        "action": "InvalidLegacyTail",
                        "from_status": "Open",
                        "to_status": "Open",
                        "timestamp": sim_now(),
                        "params": {}
                    }),
                    metadata: EventMetadata {
                        event_id: sim_uuid(),
                        causation_id: sim_uuid(),
                        correlation_id: sim_uuid(),
                        timestamp: sim_now(),
                        actor_id: persistence_id.clone(),
                    },
                },
            ],
        )
        .await
        .expect("another replica appends a legacy composite tombstone");

    assert!(
        !server
            .ensure_entity_loaded(&tenant, "Ticket", entity_id)
            .await,
        "authoritative tombstone discovery must report the entity absent"
    );
    for _ in 0..64 {
        if server.state_timeout_tracker.size() == 0 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(
        server.state_timeout_tracker.pending_snapshot(),
        vec![("Ticket".to_string(), 0)],
        "tombstone discovery must cancel the live timer immediately"
    );
    assert_eq!(
        server.state_timeout_tracker.size(),
        0,
        "tombstone discovery must reclaim its exact inactive fence after eviction"
    );
    assert!(!server.entity_exists(&tenant, "Ticket", entity_id));
    assert!(
        !server
            .actor_registry
            .read()
            .expect("actor registry lock")
            .contains_key(&persistence_id)
    );
}

#[tokio::test(start_paused = true)]
async fn cold_legacy_tombstone_with_a_later_tail_never_materializes() {
    let (_guard, _clock, _ids) = install_deterministic_context(252);
    let tenant = TenantId::default();
    let entity_id = "cold-external-delete-with-tail";
    let persistence_id = format!("{tenant}:Ticket:{entity_id}");
    let store = SimEventStore::no_faults(252);
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        tenant.as_str(),
        parse_csdl(TICKET_CSDL).expect("ticket CSDL parses"),
        TICKET_CSDL.to_string(),
        &[("Ticket", TIMED_TICKET_IOA)],
    );
    let mut server =
        ServerState::from_registry(ActorSystem::new("cold-authoritative-delete"), registry);
    server.set_storage_stack(StorageStack::from_sim(store.clone(), None));

    let envelope = |event_type: &str, to_status: &str| PersistenceEnvelope {
        sequence_nr: 0,
        event_type: event_type.to_string(),
        payload: serde_json::json!({
            "action": event_type,
            "from_status": "Open",
            "to_status": to_status,
            "timestamp": sim_now(),
            "params": {}
        }),
        metadata: EventMetadata {
            event_id: sim_uuid(),
            causation_id: sim_uuid(),
            correlation_id: sim_uuid(),
            timestamp: sim_now(),
            actor_id: persistence_id.clone(),
        },
    };
    store
        .append(
            &persistence_id,
            0,
            &[
                envelope("Created", "Open"),
                envelope("Delete", "Deleted"),
                envelope("InvalidLegacyTail", "Open"),
            ],
        )
        .await
        .expect("seed terminal history with corrupt tail");

    assert!(
        !server
            .ensure_entity_loaded(&tenant, "Ticket", entity_id)
            .await
    );
    assert!(!server.entity_exists(&tenant, "Ticket", entity_id));
    assert!(
        !server
            .actor_registry
            .read()
            .expect("actor registry lock")
            .contains_key(&persistence_id)
    );
    assert_eq!(server.state_timeout_tracker.size(), 0);
}
