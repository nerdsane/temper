//! A write decided on a state its record has left is refused as
//! `RecordChanged`: nothing is written and the record is caught up, so its
//! route decides it again on the record as it is. Two actors on one journal
//! make a change the first has not seen, as another writer on a shared journal
//! would.

use super::*;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use temper_jit::table::TransitionTable;
use temper_runtime::ActorSystem;
use temper_runtime::actor::ActorRef;
use temper_runtime::persistence::EventStore as _;
use temper_store_sim::SimEventStore;

const ORDER_IOA: &str = include_str!("../../../../test-fixtures/specs/order.ioa.toml");
const ASK: Duration = Duration::from_secs(5);

fn order_actor(
    system: &ActorSystem,
    store: &Arc<SimEventStore>,
    entity_id: &str,
    name: &str,
) -> ActorRef<EntityMsg> {
    system.spawn(
        EntityActor::with_persistence(
            "Order",
            entity_id,
            Arc::new(RwLock::new(TransitionTable::from_ioa_source(ORDER_IOA))),
            serde_json::json!({"Customer": "Alice"}),
            crate::storage::BoxedEventStore::from_arc(store.clone()),
            crate::storage::BackendLabel::Sim,
        ),
        name,
    )
}

async fn update(actor: &ActorRef<EntityMsg>, fields: serde_json::Value) -> EntityResponse {
    actor
        .ask(
            EntityMsg::UpdateFields {
                caller: None,
                fields,
                replace: false,
                expected_precondition: None,
            },
            ASK,
        )
        .await
        .expect("field update answer")
}

/// Whether `answer` refuses a write decided on a state its record has left:
/// nothing written, with the typed reason `RecordChanged`.
fn record_changed(answer: &EntityResponse) -> bool {
    answer.refused && answer.refusal.as_ref().map(|refusal| refusal.code()) == Some("RecordChanged")
}

/// The actions the journal holds for `entity_id`, in order.
async fn journaled_actions(store: &SimEventStore, entity_id: &str) -> Vec<String> {
    store
        .read_events(&format!("default:Order:{entity_id}"), 0)
        .await
        .expect("read journal")
        .into_iter()
        .filter_map(|envelope| serde_json::from_value::<EntityEvent>(envelope.payload).ok())
        .map(|event| event.action)
        .collect()
}

#[tokio::test]
async fn an_action_decided_before_another_writer_moved_the_record_is_refused_not_applied() {
    let store = Arc::new(SimEventStore::no_faults(61));
    let entity_id = "record-changed-action";
    let system = ActorSystem::new("sim-record-changed-action");
    let holder = order_actor(&system, &store, entity_id, entity_id);
    let seeded = update(&holder, serde_json::json!({"Customer": "Seed"})).await;
    assert!(seeded.success);
    // What the caller's checks read: the record as this actor holds it.
    let decided_on = super::super::effects::entity_authorization_precondition(&seeded.state);

    let other = order_actor(&system, &store, entity_id, &format!("{entity_id}-other"));
    assert!(
        update(&other, serde_json::json!({"Region": "eu"}))
            .await
            .success
    );

    // The holder's memory still matches the decision, so only its append
    // finds the other write.
    let answer: EntityResponse = holder
        .ask(
            EntityMsg::Action {
                caller: None,
                name: "CancelOrder".to_string(),
                params: serde_json::json!({"Reason": "decided on the old record"}),
                related: std::collections::BTreeMap::new(),
                idempotency_key: Some(crate::idempotency::IdempotencyKey::Request(
                    "cancel-once".to_string(),
                )),
                expected_authorization_precondition: Some(decided_on),
            },
            ASK,
        )
        .await
        .expect("action answer");

    assert!(
        !answer.success && record_changed(&answer),
        "an action decided on a state the journal had left must be refused as \
         RecordChanged, not evaluated again on a state its caller never decided on: {answer:?}"
    );
    assert_eq!(answer.state.status, "Draft");
    assert_eq!(
        answer.state.fields["Region"], "eu",
        "the refusal carries the record as it is, for the route to decide on"
    );
    assert!(
        !journaled_actions(&store, entity_id)
            .await
            .iter()
            .any(|action| action == "CancelOrder"),
        "nothing was written"
    );
}

#[tokio::test]
async fn a_field_update_decided_before_another_writer_moved_the_record_is_refused_with_the_record_caught_up()
 {
    let store = Arc::new(SimEventStore::no_faults(67));
    let entity_id = "record-changed-fields";
    let system = ActorSystem::new("sim-record-changed-fields");
    let holder = order_actor(&system, &store, entity_id, entity_id);
    let seeded = update(&holder, serde_json::json!({"Customer": "Seed"})).await;
    assert!(seeded.success);
    let decided_on = super::super::effects::entity_authorization_precondition(&seeded.state);

    let other = order_actor(&system, &store, entity_id, &format!("{entity_id}-other"));
    assert!(
        update(&other, serde_json::json!({"Region": "eu"}))
            .await
            .success
    );

    let answer: EntityResponse = holder
        .ask(
            EntityMsg::UpdateFields {
                caller: None,
                fields: serde_json::json!({"Customer": "Bob"}),
                replace: false,
                expected_precondition: Some(decided_on),
            },
            ASK,
        )
        .await
        .expect("field update answer");

    assert!(!answer.success && record_changed(&answer), "{answer:?}");
    assert_eq!(answer.state.fields["Region"], "eu");
    assert_eq!(answer.state.fields["Customer"], "Seed");

    // Decided again on the record it was answered with, the update lands.
    let again = super::super::effects::entity_authorization_precondition(&answer.state);
    let landed: EntityResponse = holder
        .ask(
            EntityMsg::UpdateFields {
                caller: None,
                fields: serde_json::json!({"Customer": "Bob"}),
                replace: false,
                expected_precondition: Some(again),
            },
            ASK,
        )
        .await
        .expect("field update answer");
    assert!(landed.success, "{landed:?}");
    assert_eq!(landed.state.fields["Customer"], "Bob");
    assert_eq!(landed.state.fields["Region"], "eu");
}

#[tokio::test]
async fn a_delete_decided_before_another_writer_moved_the_record_is_refused_with_the_record_caught_up()
 {
    let store = Arc::new(SimEventStore::no_faults(73));
    let entity_id = "record-changed-delete-race";
    let system = ActorSystem::new("sim-record-changed-delete-race");
    let holder = order_actor(&system, &store, entity_id, entity_id);
    let seeded = update(&holder, serde_json::json!({"Customer": "Seed"})).await;
    assert!(seeded.success);
    let decided_on = super::super::effects::entity_authorization_precondition(&seeded.state);

    let other = order_actor(&system, &store, entity_id, &format!("{entity_id}-other"));
    assert!(
        update(&other, serde_json::json!({"Region": "eu"}))
            .await
            .success
    );

    let delete = |precondition: String| EntityMsg::Delete {
        caller: None,
        expected_authorization_precondition: Some(precondition),
    };
    let answer: EntityResponse = holder
        .ask(delete(decided_on), ASK)
        .await
        .expect("delete answer");
    assert!(
        !answer.success && record_changed(&answer),
        "a delete whose append finds another writer's change must be refused as \
         RecordChanged, with the record caught up, not answered from a memory behind \
         its journal: {answer:?}"
    );
    assert_eq!(
        answer.state.fields["Region"], "eu",
        "the refusal carries the record as it is, for the route to decide on"
    );
    assert!(
        !journaled_actions(&store, entity_id)
            .await
            .iter()
            .any(|action| action == "Deleted"),
        "nothing was written"
    );

    // Decided again on the record it was answered with, the delete lands.
    let again = super::super::effects::entity_authorization_precondition(&answer.state);
    let landed: EntityResponse = holder.ask(delete(again), ASK).await.expect("delete answer");
    assert!(landed.success, "{landed:?}");
    assert_eq!(landed.state.status, "Deleted");
}

#[tokio::test]
async fn a_delete_that_finds_its_record_deleted_writes_nothing() {
    let store = Arc::new(SimEventStore::no_faults(71));
    let entity_id = "record-changed-delete";
    let system = ActorSystem::new("sim-record-changed-delete");
    let actor = order_actor(&system, &store, entity_id, entity_id);
    assert!(
        update(&actor, serde_json::json!({"Customer": "Seed"}))
            .await
            .success
    );

    for _ in 0..2 {
        let answer: EntityResponse = actor
            .ask(
                EntityMsg::Delete {
                    caller: None,
                    expected_authorization_precondition: None,
                },
                ASK,
            )
            .await
            .expect("delete answer");
        assert!(answer.success, "{answer:?}");
        assert_eq!(answer.state.status, "Deleted");
    }

    let tombstones = journaled_actions(&store, entity_id)
        .await
        .into_iter()
        .filter(|action| action == "Deleted")
        .count();
    assert_eq!(tombstones, 1, "a deleted record takes no second tombstone");
}

/// A spec whose own move reaches a state named `Deleted`, which is not the
/// kernel's tombstone.
const SOFT_DELETE_IOA: &str = r#"
[automaton]
name = "Note"
states = ["Open", "Deleted"]
initial = "Open"
allow_indefinite_states = ["Open", "Deleted"]

[[action]]
name = "Delete"
kind = "input"
from = ["Open"]
to = "Deleted"
"#;

#[tokio::test]
async fn a_delete_on_a_record_its_spec_moved_to_deleted_still_writes_a_tombstone() {
    let store = Arc::new(SimEventStore::no_faults(73));
    let entity_id = "soft-deleted";
    let system = ActorSystem::new("sim-soft-deleted");
    let actor = system.spawn(
        EntityActor::with_persistence(
            "Note",
            entity_id,
            Arc::new(RwLock::new(TransitionTable::from_ioa_source(
                SOFT_DELETE_IOA,
            ))),
            serde_json::json!({}),
            crate::storage::BoxedEventStore::from_arc(store.clone()),
            crate::storage::BackendLabel::Sim,
        ),
        entity_id,
    );
    let moved: EntityResponse = actor
        .ask(
            EntityMsg::Action {
                caller: None,
                name: "Delete".to_string(),
                params: serde_json::json!({}),
                related: std::collections::BTreeMap::new(),
                idempotency_key: Some(crate::idempotency::IdempotencyKey::Request(
                    "soft-delete".to_string(),
                )),
                expected_authorization_precondition: None,
            },
            ASK,
        )
        .await
        .expect("soft delete answer");
    assert!(moved.success, "{moved:?}");
    assert_eq!(moved.state.status, "Deleted");

    let deleted: EntityResponse = actor
        .ask(
            EntityMsg::Delete {
                caller: None,
                expected_authorization_precondition: None,
            },
            ASK,
        )
        .await
        .expect("delete answer");
    assert!(deleted.success, "{deleted:?}");

    let envelopes = store
        .read_events(&format!("default:Note:{entity_id}"), 0)
        .await
        .expect("read journal");
    assert!(
        envelopes.iter().any(|envelope| envelope.is_tombstone()),
        "a record its spec moved to `Deleted` is not deleted until the kernel's tombstone is written"
    );
}
