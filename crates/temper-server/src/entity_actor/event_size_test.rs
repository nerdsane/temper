//! One change larger than the server's event size limit is refused before
//! anything is written, in words that tell the caller what to keep elsewhere;
//! a change over the warning size is still written. Incident i8: an app wrote
//! 40 to 90 KB into every step until its Temper ran out of memory.

use super::*;
use std::collections::BTreeMap;
use std::time::Duration;
use temper_jit::table::TransitionTable;
use temper_runtime::ActorSystem;
use temper_runtime::persistence::{
    EventMetadata, PersistenceAppend, PersistenceEnvelope, PersistenceError,
};
use temper_store_sim::SimEventStore;

use crate::storage::{BackendLabel, BoxedEventStore, EventSizeLimits};

const ORDER_IOA: &str = include_str!("../../../../test-fixtures/specs/order.ioa.toml");

const LIMITS: EventSizeLimits = EventSizeLimits {
    warn_bytes: Some(1_000),
    max_bytes: Some(4_000),
};

fn spawn(
    system: &ActorSystem,
    id: &str,
    store: &Arc<SimEventStore>,
    limits: EventSizeLimits,
) -> temper_runtime::actor::ActorRef<EntityMsg> {
    let table = Arc::new(RwLock::new(TransitionTable::from_ioa_source(ORDER_IOA)));
    let journal = BoxedEventStore::from_arc(store.clone()).with_event_size_limits(limits);
    let actor = EntityActor::with_persistence(
        "Order",
        id,
        table,
        serde_json::json!({}),
        journal,
        BackendLabel::Sim,
    );
    system.spawn(actor, id)
}

async fn add_item(
    actor: &temper_runtime::actor::ActorRef<EntityMsg>,
    product: String,
) -> EntityResponse {
    actor
        .ask(
            EntityMsg::Action {
                caller: None,
                name: "AddItem".into(),
                params: serde_json::json!({ "ProductId": product }),
                related: BTreeMap::new(),
                idempotency_key: None,
                expected_authorization_precondition: None,
            },
            Duration::from_secs(5),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn a_change_over_the_limit_is_refused_and_never_written() {
    let store = Arc::new(SimEventStore::no_faults(11));
    let system = ActorSystem::new("event-size");
    let actor = spawn(&system, "o-1", &store, LIMITS);

    let small = add_item(&actor, "p-1".into()).await;
    assert!(small.success, "{:?}", small.error);
    let warned = add_item(&actor, "w".repeat(2_000)).await;
    assert!(
        warned.success,
        "over the warning size is written: {:?}",
        warned.error
    );

    let refused = add_item(&actor, "x".repeat(5_000)).await;
    assert!(!refused.success);
    let said = refused.error.unwrap_or_default();
    assert!(
        said.starts_with("refused: `AddItem` on default:Order:o-1 would record "),
        "{said}"
    );
    assert!(
        said.contains("over this server's limit of 4000 bytes"),
        "{said}"
    );
    assert!(
        said.contains("long lists as their own records")
            && said.contains("pictures or documents as files"),
        "the refusal says what to do: {said}"
    );
    assert_eq!(refused.state.item_count, 2, "nothing changed");

    let patched: EntityResponse = actor
        .ask(
            EntityMsg::UpdateFields {
                caller: None,
                fields: serde_json::json!({ "Notes": "y".repeat(5_000) }),
                replace: false,
                expected_precondition: None,
            },
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert!(!patched.success);
    assert!(
        patched.error.unwrap_or_default().starts_with("refused: "),
        "a field update is held to the same limit"
    );

    let journal = BoxedEventStore::from_arc(store.clone());
    let written = journal.read_events("default:Order:o-1", 0).await.unwrap();
    let adds = written.iter().filter(|e| e.event_type == "AddItem").count();
    assert_eq!(adds, 2, "only the two accepted changes were written");
    assert!(
        written
            .iter()
            .all(|e| serde_json::to_vec(&e.payload).unwrap().len() <= 4_000),
        "no event over the limit reached the journal"
    );
}

#[tokio::test]
async fn without_limits_a_large_change_is_written() {
    let store = Arc::new(SimEventStore::no_faults(12));
    let system = ActorSystem::new("event-size-off");
    let actor = spawn(&system, "o-2", &store, EventSizeLimits::default());
    let large = add_item(&actor, "x".repeat(5_000)).await;
    assert!(large.success, "{:?}", large.error);
}

fn envelope(persistence_id: &str, sequence_nr: u64, product: String) -> PersistenceEnvelope {
    PersistenceEnvelope {
        sequence_nr,
        event_type: "AddItem".into(),
        payload: serde_json::json!({ "action": "AddItem", "params": { "ProductId": product } }),
        metadata: EventMetadata {
            event_id: temper_runtime::scheduler::sim_uuid(),
            causation_id: temper_runtime::scheduler::sim_uuid(),
            correlation_id: temper_runtime::scheduler::sim_uuid(),
            timestamp: temper_runtime::scheduler::sim_now(),
            actor_id: persistence_id.into(),
        },
    }
}

#[tokio::test]
async fn a_batch_with_one_change_over_the_limit_writes_nothing() {
    let store = Arc::new(SimEventStore::no_faults(13));
    let journal = BoxedEventStore::from_arc(store.clone()).with_event_size_limits(LIMITS);
    let batch = [
        PersistenceAppend {
            persistence_id: "default:Order:b-1".into(),
            expected_sequence: 0,
            events: vec![envelope("default:Order:b-1", 1, "p".into())],
            state_timer: None,
            projection: None,
        },
        PersistenceAppend {
            persistence_id: "default:Order:b-2".into(),
            expected_sequence: 0,
            events: vec![envelope("default:Order:b-2", 1, "x".repeat(5_000))],
            state_timer: None,
            projection: None,
        },
    ];
    let refused = journal.append_batch(&batch).await.unwrap_err();
    assert!(
        matches!(&refused, PersistenceError::EventTooLarge { persistence_id, limit: 4_000, .. } if persistence_id == "default:Order:b-2"),
        "{refused}"
    );
    for id in ["default:Order:b-1", "default:Order:b-2"] {
        assert!(journal.read_events(id, 0).await.unwrap().is_empty(), "{id}");
    }
}

#[tokio::test]
async fn a_warning_size_above_the_limit_never_lets_a_change_past_the_limit() {
    let store = Arc::new(SimEventStore::no_faults(15));
    let journal = BoxedEventStore::from_arc(store).with_event_size_limits(EventSizeLimits {
        warn_bytes: Some(10_000),
        max_bytes: Some(4_000),
    });
    let refused = journal
        .append(
            "default:Order:w-1",
            0,
            &[envelope("default:Order:w-1", 1, "x".repeat(5_000))],
        )
        .await
        .unwrap_err();
    assert!(
        matches!(
            refused,
            PersistenceError::EventTooLarge { limit: 4_000, .. }
        ),
        "{refused}"
    );
}

/// A create's first event holds its initial fields. One too large is refused
/// before the entity's actor starts: the caller is told why, and nothing is left
/// behind, so the same id made again with small fields holds them.
#[tokio::test]
async fn an_oversized_create_is_refused_before_its_actor_starts() {
    const CSDL_XML: &str = include_str!("../../../../test-fixtures/specs/model.csdl.xml");
    let csdl = temper_spec::csdl::parse_csdl(CSDL_XML).unwrap();
    let mut registry = crate::registry::SpecRegistry::new();
    let tenant = temper_runtime::tenant::TenantId::default();
    registry.register_tenant(
        tenant.as_str(),
        csdl,
        CSDL_XML.to_string(),
        &[("Order", ORDER_IOA)],
    );
    let mut state =
        crate::ServerState::from_registry(ActorSystem::new("event-size-create"), registry);
    let store = SimEventStore::no_faults(16);
    state.set_storage_stack(
        crate::StorageStack::from_sim(store.clone(), None).with_event_size_limits(LIMITS),
    );

    let refused = state
        .get_or_create_tenant_entity(
            &tenant,
            "Order",
            "big",
            serde_json::json!({ "Notes": "x".repeat(5_000) }),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(
        refused.starts_with("refused: `Created` on default:Order:big would record "),
        "{refused}"
    );

    let made = state
        .get_or_create_tenant_entity(
            &tenant,
            "Order",
            "big",
            serde_json::json!({ "Notes": "fixed" }),
        )
        .await
        .unwrap();
    assert_eq!(made.state.fields["Notes"], "fixed");
    let journal = BoxedEventStore::new(store);
    let written = journal.read_events("default:Order:big", 0).await.unwrap();
    assert_eq!(
        written
            .iter()
            .map(|e| e.event_type.as_str())
            .collect::<Vec<_>>(),
        ["Created"]
    );
}
