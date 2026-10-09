//! Regression coverage for #519: user-reachable states must not kill an actor.
//!
//! Exercise the production actor with a deterministic journal, including recovery.
//! The generic runtime must not invent a domain-specific item-count ceiling.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use temper_jit::table::TransitionTable;
use temper_runtime::ActorSystem;
use temper_runtime::actor::{ActorRef, SystemSignal};
use temper_runtime::scheduler::install_deterministic_context;
use temper_server::entity_actor::types::MAX_EVENTS_SINCE_SNAPSHOT;
use temper_server::entity_actor::{EntityActor, EntityMsg, EntityResponse};
use temper_server::storage::{BackendLabel, BoxedEventStore};
use temper_store_sim::SimEventStore;

const ORDER_IOA: &str = include_str!("../../../test-fixtures/specs/order.ioa.toml");
const ASK_BUDGET: Duration = Duration::from_secs(5);

fn spawn_order(system: &ActorSystem, store: Option<Arc<SimEventStore>>) -> ActorRef<EntityMsg> {
    let table = Arc::new(RwLock::new(TransitionTable::from_ioa_source(ORDER_IOA)));
    let actor = match store {
        Some(store) => EntityActor::with_persistence(
            "Order",
            "admission",
            table,
            serde_json::json!({}),
            BoxedEventStore::from_arc(store),
            BackendLabel::Sim,
        ),
        None => EntityActor::new("Order", "admission", table, serde_json::json!({})),
    };
    system.spawn(actor, "admission")
}

async fn action(actor: &ActorRef<EntityMsg>, name: &str) -> EntityResponse {
    actor
        .ask(
            EntityMsg::Action {
                reply_mode: temper_server::idempotency::ActionReplyMode::DirectCore,
                name: name.to_string(),
                params: serde_json::json!({}),
                related: BTreeMap::new(),
                idempotency_key: None,
                expected_authorization_precondition: None,
            },
            ASK_BUDGET,
        )
        .await
        .expect("an action must return a response, not terminate the actor")
}

#[tokio::test]
async fn dst_actor_admission_item_count_is_governed_by_the_spec() {
    for seed in 0..8 {
        let (_guard, _clock, _ids) = install_deterministic_context(seed);
        let system = ActorSystem::new("item-count-admission");
        let actor = spawn_order(&system, None);
        // The fixture declares an unbounded counter, not a 1,000-item invariant.
        for expected in 1..=1_002 {
            let response = action(&actor, "AddItem").await;
            assert!(response.success, "seed {seed}: {:?}", response.error);
            assert_eq!(response.state.item_count, expected, "seed {seed}");
        }
        let cancelled = action(&actor, "CancelOrder").await;
        assert!(cancelled.success, "seed {seed}: {:?}", cancelled.error);
        assert_eq!(cancelled.state.status, "Cancelled");
        actor.stop().expect("stop the test actor");
    }
}

#[tokio::test]
async fn dst_actor_admission_large_counter_remains_usable_after_recovery() {
    for seed in 0..8 {
        let (_guard, _clock, _ids) = install_deterministic_context(seed);
        let system = ActorSystem::new("large-counter-recovery");
        let store = Arc::new(SimEventStore::no_faults(seed));
        let actor = spawn_order(&system, Some(store.clone()));
        for expected in 1..=1_001 {
            let response = action(&actor, "AddItem").await;
            assert!(response.success, "seed {seed}: {:?}", response.error);
            assert_eq!(response.state.item_count, expected);
        }
        actor
            .signal(SystemSignal::Restart)
            .expect("restart through the production actor lifecycle");
        let recovered: EntityResponse = actor
            .ask(EntityMsg::GetState, ASK_BUDGET)
            .await
            .expect("snapshot and journal tail recover");
        assert_eq!(recovered.state.item_count, 1_001, "seed {seed}");
        let before = store.dump_journal("default:Order:admission").len();
        let removed = action(&actor, "RemoveItem").await;
        assert!(removed.success, "seed {seed}: {:?}", removed.error);
        assert_eq!(removed.state.item_count, 1_000);
        assert_eq!(
            store.dump_journal("default:Order:admission").len(),
            before + 1
        );
        actor.stop().expect("stop the test actor");
    }
}

#[tokio::test]
async fn dst_actor_admission_event_budget_returns_a_refusal() {
    let (_guard, _clock, _ids) = install_deterministic_context(519);
    let system = ActorSystem::new("event-budget-admission");
    let actor = spawn_order(&system, None);
    for event in 0..MAX_EVENTS_SINCE_SNAPSHOT {
        let name = if event % 2 == 0 {
            "AddItem"
        } else {
            "RemoveItem"
        };
        let response = action(&actor, name).await;
        assert!(response.success, "event {event}: {:?}", response.error);
    }
    let before: EntityResponse = actor
        .ask(EntityMsg::GetState, ASK_BUDGET)
        .await
        .expect("state at the event budget");
    let refused = action(&actor, "AddItem").await;
    assert!(!refused.success);
    assert!(
        refused
            .error
            .as_deref()
            .is_some_and(|e| e.contains("Event budget exhausted"))
    );
    assert_eq!(
        refused.state.total_event_count,
        before.state.total_event_count
    );
    assert_eq!(refused.state.item_count, before.state.item_count);
    let after: EntityResponse = actor
        .ask(EntityMsg::GetState, ASK_BUDGET)
        .await
        .expect("budget exhaustion does not kill the actor");
    assert_eq!(
        after.state.total_event_count,
        before.state.total_event_count
    );
    actor.stop().expect("stop the test actor");
}

#[tokio::test]
async fn dst_actor_admission_tombstone_rejects_actions_without_stopping() {
    for seed in 0..8 {
        let (_guard, _clock, _ids) = install_deterministic_context(seed);
        let system = ActorSystem::new("tombstone-admission");
        let store = Arc::new(SimEventStore::no_faults(seed));
        let actor = spawn_order(&system, Some(store.clone()));
        assert!(action(&actor, "AddItem").await.success);
        let deleted: EntityResponse = actor
            .ask(
                EntityMsg::Delete {
                    expected_authorization_precondition: None,
                },
                ASK_BUDGET,
            )
            .await
            .expect("delete response");
        assert!(deleted.success, "seed {seed}: {:?}", deleted.error);
        let before = store.dump_journal("default:Order:admission").len();

        for recovered in [false, true] {
            if recovered {
                actor
                    .signal(SystemSignal::Restart)
                    .expect("restart the deleted entity");
            }
            for name in ["AddItem", "CancelOrder", "UnknownAction"] {
                let response = action(&actor, name).await;
                assert!(!response.success, "seed {seed}: {name} after delete");
                assert_eq!(response.state.status, "Deleted");
                assert_eq!(response.state.sequence_nr, deleted.state.sequence_nr);
                assert_eq!(response.state.item_count, deleted.state.item_count);
                assert!(response.error.is_some());
                assert!(response.custom_effects.is_empty());
                assert!(response.scheduled_actions.is_empty());
                assert!(response.spawn_requests.is_empty());
            }
            let current: EntityResponse = actor
                .ask(EntityMsg::GetState, ASK_BUDGET)
                .await
                .expect("rejected actions leave the actor alive and readable");
            assert_eq!(current.state.status, "Deleted");
            assert_eq!(store.dump_journal("default:Order:admission").len(), before);
        }
        actor.stop().expect("stop the test actor");
    }
}
