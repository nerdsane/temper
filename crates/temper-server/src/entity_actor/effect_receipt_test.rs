//! Restart and spec-evolution regressions for durable effect receipts.

#![cfg(feature = "sim")]

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use temper_jit::table::TransitionTable;
use temper_runtime::ActorSystem;
use temper_runtime::scheduler::install_deterministic_context;
use temper_store_sim::SimEventStore;

use super::{EntityActor, EntityMsg, EntityResponse};
use crate::storage::{BackendLabel, BoxedEventStore};

fn replay_probe_table(with_later_effect: bool) -> Arc<RwLock<TransitionTable>> {
    let source = if with_later_effect {
        r#"
[automaton]
name = "ReplayProbe"
version = "2.0.0"
states = ["Active", "Done"]
initial = "Active"

[[state]]
name = "effect_permitted"
type = "bool"
initial = "false"

[[action]]
name = "Apply"
kind = "input"
from = ["Active"]
to = "Done"
params = ["Value"]
guard = "is_true effect_permitted"
effect = [{ type = "trigger", name = "late_effect" }]

[[integration]]
name = "late_effect"
trigger = "late_effect"
type = "wasm"
module = "late_effect"
"#
    } else {
        r#"
[automaton]
name = "ReplayProbe"
version = "1.0.0"
states = ["Active", "Done"]
initial = "Active"

[[action]]
name = "Apply"
kind = "input"
from = ["Active"]
to = "Done"
params = ["Value"]
"#
    };
    Arc::new(RwLock::new(TransitionTable::from_ioa_source(source)))
}

#[tokio::test]
async fn present_empty_receipt_survives_later_effect_bearing_spec() {
    let (_guard, _clock, _ids) = install_deterministic_context(346);
    let store = BoxedEventStore::new(SimEventStore::no_faults(346));
    let params = serde_json::json!({"Value": "original"});

    {
        let system = ActorSystem::new("effect-receipt-before-restart");
        let actor = EntityActor::with_persistence(
            "ReplayProbe",
            "receipt",
            replay_probe_table(false),
            serde_json::json!({}),
            store.clone(),
            BackendLabel::Sim,
        );
        let actor_ref = system.spawn(actor, "effect-receipt-before-restart");
        let first: EntityResponse = actor_ref
            .ask(
                EntityMsg::Action {
                    name: "Apply".into(),
                    params: params.clone(),
                    cross_entity_booleans: BTreeMap::new(),
                    idempotency_key: Some("receipt-key".into()),
                },
                Duration::from_secs(1),
            )
            .await
            .expect("first action response");
        assert!(first.success);
        assert!(first.custom_effects.is_empty());
    }

    let stored = store
        .read_events("default:ReplayProbe:receipt", 0)
        .await
        .expect("stored events");
    let original = stored
        .iter()
        .find(|event| event.event_type == "Apply")
        .expect("persisted Apply event");
    assert_eq!(original.payload["idempotency_key"], "receipt-key");
    assert!(
        original.payload.get("effect_receipt_version").is_some(),
        "effect-free transitions must persist an explicit receipt"
    );
    assert!(
        original.payload.get("custom_effects").is_none(),
        "empty output stays compact while the receipt marker preserves presence"
    );

    let system = ActorSystem::new("effect-receipt-after-restart");
    let actor = EntityActor::with_persistence(
        "ReplayProbe",
        "receipt",
        replay_probe_table(true),
        serde_json::json!({}),
        store,
        BackendLabel::Sim,
    );
    let actor_ref = system.spawn(actor, "effect-receipt-after-restart");
    let duplicate: EntityResponse = actor_ref
        .ask(
            EntityMsg::Action {
                name: "Apply".into(),
                params,
                cross_entity_booleans: BTreeMap::new(),
                idempotency_key: Some("receipt-key".into()),
            },
            Duration::from_secs(1),
        )
        .await
        .expect("duplicate response");

    assert!(duplicate.success);
    assert!(duplicate.custom_effects.is_empty());
    assert!(duplicate.scheduled_actions.is_empty());
    assert!(duplicate.spawn_requests.is_empty());
    assert_eq!(
        duplicate.state.events.len(),
        2,
        "duplicate must not execute a second transition"
    );
    let replayed = duplicate
        .state
        .events
        .iter()
        .find(|event| event.action == "Apply")
        .expect("hydrated Apply event");
    assert!(replayed.effect_receipt_version.is_some());
    assert!(replayed.custom_effects.is_empty());
}
