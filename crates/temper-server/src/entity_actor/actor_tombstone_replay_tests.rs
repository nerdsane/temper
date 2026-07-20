//! Backward-compatible terminal tombstone replay.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use temper_jit::table::TransitionTable;
use temper_runtime::ActorSystem;
use temper_runtime::persistence::{EventMetadata, EventStore, PersistenceEnvelope};
use temper_runtime::scheduler::{install_deterministic_context, sim_now, sim_uuid};
use temper_store_sim::SimEventStore;

use super::{EntityActor, EntityMsg, EntityResponse};

#[tokio::test]
async fn legacy_action_named_delete_is_terminal_when_payload_targets_deleted() {
    let (_guard, _clock, _ids) = install_deterministic_context(248);
    let table = Arc::new(RwLock::new(TransitionTable::from_ioa_source(
        r#"
[automaton]
name = "TimedTask"
states = ["Running", "TimedOut", "Deleted"]
initial = "Running"
allow_indefinite_states = ["TimedOut", "Deleted"]

[[action]]
name = "TimeoutFail"
kind = "internal"
from = ["Running"]
to = "TimedOut"
"#,
    )));
    let store = Arc::new(SimEventStore::no_faults(248));
    let persistence_id = "default:TimedTask:legacy-delete";
    let event =
        |event_type: &str, action: &str, from_status: &str, to_status: &str| PersistenceEnvelope {
            sequence_nr: 0,
            event_type: event_type.to_string(),
            payload: serde_json::json!({
                "action": action,
                "from_status": from_status,
                "to_status": to_status,
                "timestamp": sim_now(),
                "params": {}
            }),
            metadata: EventMetadata {
                event_id: sim_uuid(),
                causation_id: sim_uuid(),
                correlation_id: sim_uuid(),
                timestamp: sim_now(),
                actor_id: persistence_id.to_string(),
            },
        };
    store
        .append(
            persistence_id,
            0,
            &[
                event("Created", "Created", "", "Running"),
                event("Delete", "Delete", "Running", "Deleted"),
                event("TimeoutFail", "TimeoutFail", "Running", "TimedOut"),
            ],
        )
        .await
        .expect("seed legacy tombstone followed by an invalid later tail");

    let actor = EntityActor::with_persistence(
        "TimedTask",
        "legacy-delete",
        table,
        serde_json::json!({}),
        crate::storage::BoxedEventStore::from_arc(store),
        crate::storage::BackendLabel::Sim,
    );
    let actor_ref = ActorSystem::new("legacy-tombstone-replay").spawn(actor, "legacy-delete");
    let response: EntityResponse = actor_ref
        .ask(EntityMsg::GetState, Duration::from_secs(1))
        .await
        .expect("legacy tombstone replays");

    assert_eq!(response.state.status, "Deleted");
    assert_eq!(
        response.state.sequence_nr, 2,
        "replay must stop at the first durable transition into Deleted"
    );
}

#[tokio::test]
async fn action_named_deleted_is_not_terminal_when_payload_targets_live_state() {
    let (_guard, _clock, _ids) = install_deterministic_context(251);
    let table = Arc::new(RwLock::new(TransitionTable::from_ioa_source(
        r#"
[automaton]
name = "TimedTask"
states = ["Running", "TimedOut", "Deleted"]
initial = "Running"
allow_indefinite_states = ["TimedOut", "Deleted"]

[[action]]
name = "TimeoutFail"
kind = "internal"
from = ["Running"]
to = "TimedOut"
"#,
    )));
    let store = Arc::new(SimEventStore::no_faults(251));
    let persistence_id = "default:TimedTask:live-action-named-deleted";
    let event =
        |event_type: &str, action: &str, from_status: &str, to_status: &str| PersistenceEnvelope {
            sequence_nr: 0,
            event_type: event_type.to_string(),
            payload: serde_json::json!({
                "action": action,
                "from_status": from_status,
                "to_status": to_status,
                "timestamp": sim_now(),
                "params": {}
            }),
            metadata: EventMetadata {
                event_id: sim_uuid(),
                causation_id: sim_uuid(),
                correlation_id: sim_uuid(),
                timestamp: sim_now(),
                actor_id: persistence_id.to_string(),
            },
        };
    store
        .append(
            persistence_id,
            0,
            &[
                event("Created", "Created", "", "Running"),
                event("Transitioned", "Deleted", "Running", "Running"),
                event("TimeoutFail", "TimeoutFail", "Running", "TimedOut"),
            ],
        )
        .await
        .expect("seed a live action named Deleted followed by a valid tail");

    let actor = EntityActor::with_persistence(
        "TimedTask",
        "live-action-named-deleted",
        table,
        serde_json::json!({}),
        crate::storage::BoxedEventStore::from_arc(store),
        crate::storage::BackendLabel::Sim,
    );
    let actor_ref =
        ActorSystem::new("live-deleted-action-replay").spawn(actor, "live-action-named-deleted");
    let response: EntityResponse = actor_ref
        .ask(EntityMsg::GetState, Duration::from_secs(1))
        .await
        .expect("live action replay succeeds");

    assert_eq!(response.state.status, "TimedOut");
    assert_eq!(
        response.state.sequence_nr, 3,
        "an action label alone must not truncate authoritative replay"
    );
}
