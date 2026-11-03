//! An actor that cannot read its journal tail after a snapshot does not start
//! from the snapshot: the read fails, and the next read after the store
//! recovers returns the state the whole journal describes.

use super::*;
use std::collections::BTreeMap;
use std::time::Duration;
use temper_jit::table::TransitionTable;
use temper_runtime::ActorSystem;
use temper_runtime::persistence::EventStore;
use temper_runtime::tenant::TenantId;
use temper_store_sim::SimEventStore;

use crate::storage::{BackendLabel, BoxedEventStore, StorageStack};
use crate::{ServerState, entity_actor::types::EntityResponse};

const ORDER_IOA: &str = include_str!("../../../../test-fixtures/specs/order.ioa.toml");
const MODEL_CSDL: &str = include_str!("../../../../test-fixtures/specs/model.csdl.xml");
const ENTITY_ID: &str = "tail-read";
const PERSISTENCE_ID: &str = "default:Order:tail-read";

async fn act(
    actor: &temper_runtime::actor::ActorRef<EntityMsg>,
    name: &str,
    params: serde_json::Value,
) -> EntityResponse {
    let response: EntityResponse = actor
        .ask(
            EntityMsg::Action {
                caller: None,
                name: name.into(),
                params,
                related: BTreeMap::new(),
                idempotency_key: None,
                expected_authorization_precondition: None,
            },
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert!(response.success, "{name}: {:?}", response.error);
    response
}

/// Journal: 1 Created, 2 AddItem, 3 CancelOrder. Snapshot at sequence 2,
/// so a reader that loads the snapshot must still read event 3 to be current.
async fn journal_with_event_after_snapshot(store: &Arc<SimEventStore>) {
    let table = TransitionTable::from_ioa_source(ORDER_IOA);
    let system = ActorSystem::new("tail-read-writer");
    let writer = system.spawn(
        EntityActor::with_persistence(
            "Order",
            ENTITY_ID,
            Arc::new(RwLock::new(table.clone())),
            serde_json::json!({}),
            BoxedEventStore::from_arc(store.clone()),
            BackendLabel::Sim,
        ),
        ENTITY_ID,
    );
    act(&writer, "AddItem", serde_json::json!({"ProductId": "p-1"})).await;

    let at_two = recover_entity_state_from_store(
        "default",
        "Order",
        ENTITY_ID,
        &table,
        &BoxedEventStore::from_arc(store.clone()),
        BackendLabel::Sim,
        &serde_json::json!({}),
        None,
    )
    .await
    .unwrap();
    assert_eq!((at_two.status.as_str(), at_two.sequence_nr), ("Draft", 2));
    let snapshot = EntityActor::serialize_snapshot_state(&at_two).unwrap();

    let cancelled = act(&writer, "CancelOrder", serde_json::json!({"Reason": "r"})).await;
    assert_eq!(cancelled.state.sequence_nr, 3);
    store
        .save_snapshot(PERSISTENCE_ID, 2, &snapshot)
        .await
        .unwrap();
}

/// A fresh server over the same store: what the next process sees after a
/// restart, with no actor resident for the entity.
fn restarted_server(store: &Arc<SimEventStore>) -> ServerState {
    ServerState::with_storage_stack(
        ActorSystem::new("tail-read-restart"),
        temper_spec::csdl::parse_csdl(MODEL_CSDL).unwrap(),
        MODEL_CSDL.into(),
        BTreeMap::from([("Order".to_string(), ORDER_IOA.to_string())]),
        StorageStack::from_sim((**store).clone(), None),
    )
    .unwrap()
}

#[tokio::test]
async fn an_unreadable_journal_tail_fails_the_read_and_the_next_read_is_current() {
    let store = Arc::new(SimEventStore::no_faults(57));
    journal_with_event_after_snapshot(&store).await;
    let state = restarted_server(&store);
    let tenant = TenantId::default();

    // More failures than the actor's start attempts, so every attempt meets one.
    store.fail_next_reads(PERSISTENCE_ID, 64);
    let while_unreadable = state
        .get_tenant_entity_state(&tenant, "Order", ENTITY_ID)
        .await;
    assert!(
        while_unreadable.is_err(),
        "a read must fail while the journal tail is unreadable, got {:?}",
        while_unreadable.map(|r| (r.state.status, r.state.sequence_nr))
    );

    store.fail_next_reads(PERSISTENCE_ID, 0);
    let recovered = state
        .get_tenant_entity_state(&tenant, "Order", ENTITY_ID)
        .await
        .expect("the next read after the store recovers hydrates again");
    assert_eq!(recovered.state.status, "Cancelled");
    assert_eq!(recovered.state.sequence_nr, 3);
}

/// A write owed to a record whose journal cannot be read is kept to try
/// again, whether its read meets the store itself (a record with no snapshot
/// and no actor) or an actor whose starts all fail and that then stops (a
/// record whose snapshot says it exists). Once reads return, the next read
/// starts a stopped actor again and reads the record.
#[tokio::test]
async fn a_record_the_store_cannot_read_keeps_a_write_owed_to_it() {
    const PLAIN_ID: &str = "no-snapshot";
    const PLAIN_PERSISTENCE_ID: &str = "default:Order:no-snapshot";
    let store = Arc::new(SimEventStore::no_faults(58));
    journal_with_event_after_snapshot(&store).await;
    let writer = ActorSystem::new("no-snapshot-writer").spawn(
        EntityActor::with_persistence(
            "Order",
            PLAIN_ID,
            Arc::new(RwLock::new(TransitionTable::from_ioa_source(ORDER_IOA))),
            serde_json::json!({}),
            BoxedEventStore::from_arc(store.clone()),
            BackendLabel::Sim,
        ),
        PLAIN_ID,
    );
    act(&writer, "AddItem", serde_json::json!({"ProductId": "p-1"})).await;
    let state = restarted_server(&store);
    let tenant = TenantId::default();
    let kept = |read: &Result<String, crate::state::DispatchError>| match read {
        Ok(_) => false,
        Err(error) => matches!(
            crate::state::owed_writes::Attempt::of_error(error),
            crate::state::owed_writes::Attempt::StoreFailed(_)
        ),
    };

    // More failures than the actor's start attempts, so every attempt meets one.
    store.fail_next_reads(PLAIN_PERSISTENCE_ID, 64);
    store.fail_next_reads(PERSISTENCE_ID, 64);
    for id in [PLAIN_ID, ENTITY_ID] {
        let unreadable = state
            .load_authz_resource_snapshot(&tenant, "Order", id)
            .await
            .map(|snapshot| snapshot.current_state.state.status);
        assert!(
            kept(&unreadable),
            "{id}: a read the store failed must keep the write, got {unreadable:?}"
        );
    }

    store.fail_next_reads(PLAIN_PERSISTENCE_ID, 0);
    store.fail_next_reads(PERSISTENCE_ID, 0);
    for (id, status) in [(PLAIN_ID, "Draft"), (ENTITY_ID, "Cancelled")] {
        let read = state
            .load_authz_resource_snapshot(&tenant, "Order", id)
            .await
            .unwrap_or_else(|error| panic!("{id}: the read after reads return: {error}"));
        assert!(read.exists, "{id}");
        assert_eq!(read.current_state.state.status, status, "{id}");
    }
}
