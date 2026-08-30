//! DST proof that source transitions and reaction intents commit atomically.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use temper_jit::table::TransitionTable;
use temper_runtime::ActorSystem;
use temper_runtime::scheduler::install_deterministic_context;
use temper_server::storage::{BackendLabel, BoxedEventStore};
use temper_server::trigger::delivery::{
    ReactionCommitContext, extract_intents, stable_delivery_id,
};
use temper_server::trigger::{ReactionRule, ReactionTarget, ReactionTrigger, TargetResolver};
use temper_server::{EntityActor, EntityMsg, EntityResponse};
use temper_store_sim::SimEventStore;

const ORDER_IOA: &str = include_str!("../../../test-fixtures/specs/order.ioa.toml");

fn order_table() -> Arc<RwLock<TransitionTable>> {
    Arc::new(RwLock::new(TransitionTable::from_ioa_source(ORDER_IOA)))
}

#[tokio::test]
async fn source_event_atomically_contains_bound_reaction_intent() {
    let (_guard, _clock, _id_gen) = install_deterministic_context(414);
    let store_inner = SimEventStore::no_faults(414);
    let store = BoxedEventStore::new(store_inner.clone());
    let system = ActorSystem::new("dst-reaction-intent");
    let actor = EntityActor::with_persistence(
        "Order",
        "order-414",
        order_table(),
        serde_json::json!({}),
        store,
        BackendLabel::Sim,
    )
    .with_tenant("default");
    let actor_ref = system.spawn(actor, "order-414");
    let rule = ReactionRule {
        name: "create-payment".to_string(),
        when: ReactionTrigger {
            entity_type: "Order".to_string(),
            action: Some("AddItem".to_string()),
            to_state: Some("Draft".to_string()),
            guard: None,
        },
        then: ReactionTarget {
            entity_type: "Payment".to_string(),
            action: "Create".to_string(),
            params: serde_json::json!({}),
            params_from: BTreeMap::new(),
        },
        resolve_target: TargetResolver::SameId,
        principal: None,
        drop_ok: false,
    };

    let response: EntityResponse = actor_ref
        .ask(
            EntityMsg::Action {
                name: "AddItem".to_string(),
                params: serde_json::json!({"ProductId": "product-7", "Quantity": 1}),
                cross_entity_booleans: BTreeMap::new(),
                idempotency_key: Some("source-action-414".to_string()),
                expected_sequence: None,
                reaction_context: Some(Box::new(ReactionCommitContext {
                    rules: vec![rule],
                    authority: serde_json::json!({"principal": {"id": "User::alice"}}),
                    depth: 0,
                    root_delivery_id: None,
                    expected_source_sequence: 0,
                    resolved_guards: BTreeMap::new(),
                    receipt: None,
                })),
                kernel_metadata: None,
                expected_authorization_precondition: None,
            },
            Duration::from_secs(5),
        )
        .await
        .expect("actor should respond");
    assert!(response.success);

    let journal = store_inner.dump_journal("default:Order:order-414");
    let source_event = journal
        .iter()
        .find(|event| event.event_type == "AddItem")
        .expect("source event must be durable");
    let intents = extract_intents(&source_event.payload).expect("intent payload must decode");
    assert_eq!(intents.len(), 1);
    assert_eq!(intents[0].source_sequence, source_event.sequence_nr);
    assert_eq!(intents[0].source_to_state, "Draft");
    assert_eq!(intents[0].source_fields["ProductId"], "product-7");
    assert_eq!(
        intents[0].delivery_id,
        stable_delivery_id(
            "default",
            "Order",
            "order-414",
            "AddItem",
            source_event.sequence_nr,
            "create-payment",
            0,
        )
    );
}
