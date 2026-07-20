//! Regression coverage for the first journal write after snapshot-only recovery.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use temper_jit::table::TransitionTable;
use temper_runtime::ActorSystem;
use temper_runtime::scheduler::install_deterministic_context;
use temper_server::entity_actor::{EntityActor, EntityMsg, EntityResponse};
use temper_server::storage::{BackendLabel, BoxedEventStore};
use temper_store_sim::SimEventStore;

const COUNTER_IOA: &str = r#"
[automaton]
name = "Counter"
states = ["Ready"]
initial = "Ready"

[[state]]
name = "value"
type = "counter"
initial = "0"

[[action]]
name = "Increment"
kind = "input"
from = ["Ready"]
to = "Ready"
effect = [{ type = "increment", var = "value" }]
"#;

#[tokio::test]
async fn first_journal_write_materializes_snapshot_only_state_for_restart() {
    let (_guard, _clock, _ids) = install_deterministic_context(283);
    let sim = SimEventStore::no_faults(283);
    let events = BoxedEventStore::new(sim.clone());
    let persistence_id = "default:Counter:snapshot-only-counter";
    let snapshot = serde_json::to_vec(&serde_json::json!({
        "entity_type": "Counter",
        "entity_id": "snapshot-only-counter",
        "status": "Ready",
        "item_count": 0,
        "counters": {"value": 10},
        "booleans": {},
        "lists": {},
        "fields": {
            "Id": "snapshot-only-counter",
            "Status": "Ready"
        },
        "events": [],
        "total_event_count": 10,
        "events_since_snapshot": 0,
        "last_snapshot_sequence_nr": 5,
        "sequence_nr": 5,
        "processed_idempotency_keys": {}
    }))
    .expect("serialize snapshot-only state");
    events
        .save_snapshot(persistence_id, 5, &snapshot)
        .await
        .expect("seed snapshot-only generation");

    let table = Arc::new(RwLock::new(TransitionTable::from_ioa_source(COUNTER_IOA)));
    let first_system = ActorSystem::new("snapshot-only-materialization-first");
    let first = first_system.spawn(
        EntityActor::with_persistence(
            "Counter",
            "snapshot-only-counter",
            table.clone(),
            serde_json::json!({}),
            events.clone(),
            BackendLabel::Sim,
        )
        .with_tenant("default"),
        "snapshot-only-counter-first",
    );
    let updated: EntityResponse = first
        .ask(
            EntityMsg::Action {
                name: "Increment".to_string(),
                params: serde_json::json!({}),
                cross_entity_booleans: BTreeMap::new(),
                idempotency_key: Some("snapshot-only-increment".to_string()),
            },
            Duration::from_secs(5),
        )
        .await
        .expect("increment snapshot-only actor");
    assert!(updated.success, "increment failed: {:?}", updated.error);
    assert_eq!(updated.state.counters.get("value"), Some(&11));
    let journal = sim.dump_journal(persistence_id);
    assert_eq!(journal.len(), 2);
    assert_eq!(
        journal[0].event_type, "Temper.Internal.StateMaterialization.v1",
        "the first journal record must carry the complete snapshot-only baseline"
    );
    assert_eq!(journal[1].event_type, "Increment");
    assert!(
        events
            .load_snapshot(persistence_id)
            .await
            .expect("load snapshot after materialization")
            .is_none(),
        "the fenced first journal append must atomically retire the migration snapshot"
    );

    let restart_system = ActorSystem::new("snapshot-only-materialization-restart");
    let restarted = restart_system.spawn(
        EntityActor::with_persistence(
            "Counter",
            "snapshot-only-counter",
            table,
            serde_json::json!({}),
            events,
            BackendLabel::Sim,
        )
        .with_tenant("default"),
        "snapshot-only-counter-restarted",
    );
    let recovered: EntityResponse = restarted
        .ask(EntityMsg::GetState, Duration::from_secs(5))
        .await
        .expect("recover actor after first journal write");

    assert_eq!(
        recovered.state.counters.get("value"),
        Some(&11),
        "restart must replay the first journal delta from the complete snapshot-only baseline"
    );
}
