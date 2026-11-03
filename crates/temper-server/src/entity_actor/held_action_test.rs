//! DST: an action held to the state its caller read is never applied to a
//! newer one.
//!
//! Two actors on two nodes serve the same order over one journal. Node A
//! commits a seeded number of `AddItem`s that node B has not replayed. Node
//! B's caller read the order from node B and sends `AddItem` held to that
//! read. The journal append conflicts. B must refuse the action as stale and
//! answer with the record as the journal holds it, not replay, re-evaluate and
//! commit on a version its caller never read.

use super::*;
use std::collections::BTreeMap;
use std::time::Duration;

use temper_jit::table::TransitionTable;
use temper_runtime::ActorSystem;
use temper_runtime::scheduler::{DeterministicRng, install_deterministic_context};
use temper_store_sim::SimEventStore;

const ORDER_IOA: &str = include_str!("../../../../test-fixtures/specs/order.ioa.toml");
const SEEDS: u64 = 16;

fn node(
    name: &str,
    store: &Arc<SimEventStore>,
    entity_id: &str,
) -> (ActorSystem, temper_runtime::actor::ActorRef<EntityMsg>) {
    let actor = EntityActor::with_persistence(
        "Order",
        entity_id,
        Arc::new(RwLock::new(TransitionTable::from_ioa_source(ORDER_IOA))),
        serde_json::json!({}),
        crate::storage::BoxedEventStore::from_arc(store.clone()),
        crate::storage::BackendLabel::Sim,
    )
    .with_tenant("default");
    let system = ActorSystem::new(name);
    let actor_ref = system.spawn(actor, entity_id);
    (system, actor_ref)
}

async fn ask(actor: &temper_runtime::actor::ActorRef<EntityMsg>, msg: EntityMsg) -> EntityResponse {
    actor
        .ask(msg, Duration::from_secs(5))
        .await
        .expect("actor answers")
}

fn add_item(held_to: Option<String>) -> EntityMsg {
    EntityMsg::Action {
        name: "AddItem".to_string(),
        params: serde_json::json!({}),
        related: BTreeMap::new(),
        idempotency_key: None,
        expected_authorization_precondition: held_to,
        caller: None,
    }
}

#[tokio::test]
async fn a_held_action_is_refused_as_stale_when_the_journal_moved_on() {
    for seed in 0..SEEDS {
        let (_guard, _clock, _ids) = install_deterministic_context(seed);
        let mut rng = DeterministicRng::new(seed);
        let store = Arc::new(SimEventStore::no_faults(seed));
        let entity_id = format!("order-two-nodes-{seed}");
        let persistence_id = format!("default:Order:{entity_id}");

        let (_system_a, node_a) = node("node-a", &store, &entity_id);
        ask(&node_a, EntityMsg::GetState).await;
        let (_system_b, node_b) = node("node-b", &store, &entity_id);
        let read = ask(&node_b, EntityMsg::GetState).await.state;

        let ahead = 1 + rng.next_bound(3);
        for _ in 0..ahead {
            assert!(ask(&node_a, add_item(None)).await.success);
        }
        let journal_before = store.dump_journal(&persistence_id).len();

        let held_to = crate::entity_actor::effects::entity_authorization_precondition(&read);
        let reply = ask(&node_b, add_item(Some(held_to))).await;
        let trace = format!(
            "seed {seed}: node A {ahead} ahead, node B read version {}; reply {:?} at version {}",
            read.sequence_nr, reply.error, reply.state.sequence_nr
        );

        assert!(
            !reply.success,
            "no action commits against a version its caller did not read; {trace}"
        );
        assert_eq!(
            reply.error.as_deref(),
            Some(ACTION_RECORD_CHANGED),
            "the refusal is the stale refusal; {trace}"
        );
        assert_eq!(
            store.dump_journal(&persistence_id).len(),
            journal_before,
            "the refused action wrote nothing; {trace}"
        );
        assert_eq!(
            reply.state.sequence_nr,
            u64::try_from(journal_before).expect("journal length fits"),
            "the refusal answers with the record at its current version; {trace}"
        );
        assert_eq!(reply.state.item_count, ahead, "{trace}");
    }
}
