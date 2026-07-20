use super::*;

/// Verify that replay skips events whose payload cannot be deserialized against
/// the current `EntityEvent` schema (schema evolution resilience).
///
/// The actor must reach a consistent final state using only the events that
/// parsed successfully, and must NOT panic on the schema-mismatched event.
#[cfg(feature = "sim")]
#[tokio::test]
async fn replay_skips_schema_mismatched_events() {
    use temper_runtime::persistence::EventStore;
    use temper_store_sim::SimEventStore;

    let store = Arc::new(SimEventStore::no_faults(42));
    let pid = "default:Order:schema-evo-1";

    // Event 1: valid CancelOrder — parseable as EntityEvent.
    let good_env = PersistenceEnvelope {
        sequence_nr: 0, // overwritten by SimEventStore to 1
        event_type: "CancelOrder".to_string(),
        payload: serde_json::json!({
            "action": "CancelOrder",
            "from_status": "Draft",
            "to_status": "Cancelled",
            "timestamp": "2024-01-01T00:00:00Z",
            "params": {}
        }),
        metadata: EventMetadata {
            event_id: sim_uuid(),
            causation_id: sim_uuid(),
            correlation_id: sim_uuid(),
            timestamp: sim_now(),
            actor_id: pid.to_string(),
        },
    };

    // Event 2: schema-mismatched — "action" is an integer, not a String.
    // Simulates a legacy event written under a previous schema version.
    let bad_env = PersistenceEnvelope {
        sequence_nr: 0, // overwritten by SimEventStore to 2
        event_type: "LegacyAction".to_string(),
        payload: serde_json::json!({
            "action": 999,
            "unknown_legacy_field": "leftover_from_old_schema"
        }),
        metadata: EventMetadata {
            event_id: sim_uuid(),
            causation_id: sim_uuid(),
            correlation_id: sim_uuid(),
            timestamp: sim_now(),
            actor_id: pid.to_string(),
        },
    };

    store.append(pid, 0, &[good_env]).await.unwrap();
    store.append(pid, 1, &[bad_env]).await.unwrap();

    let system = ActorSystem::new("sim-replay-schema");
    let actor = EntityActor::with_persistence(
        "Order",
        "schema-evo-1",
        order_table(),
        serde_json::json!({}),
        crate::storage::BoxedEventStore::from_arc(store),
        crate::storage::BackendLabel::Sim,
    );
    let actor_ref = system.spawn(actor, "schema-evo-1");

    let response: EntityResponse = actor_ref
        .ask(EntityMsg::GetState, Duration::from_secs(5))
        .await
        .unwrap();

    // Actor started cleanly despite the bad event.
    assert!(response.success);
    // The valid CancelOrder event was applied → status is Cancelled.
    assert_eq!(response.state.status, "Cancelled");
    // Both sequence numbers consumed (bad event's seq_nr was still advanced).
    assert_eq!(response.state.sequence_nr, 2);
    // Only the good event contributed to total_event_count.
    assert_eq!(response.state.total_event_count, 1);
}

/// A committed cross-entity-guarded transition must survive replay.
///
/// Regression: `File.StreamUpdated` carries a `cross_entity_state` guard on the
/// owning Workspace. The guard's boolean is pre-resolved at dispatch time and
/// injected into the eval context, but replay rebuilds the context *without*
/// the related entity in scope — so re-evaluating the guard during replay sees
/// the cross-entity precondition as unsatisfied. Replay must NOT re-gate a
/// durably-stored event: it must honor the stored `to_status` and re-apply the
/// transition's effects, or a File that committed `Created -> Ready` would
/// silently rehydrate back to `Created` (losing `has_content` and the version
/// bump). This proves the stored history wins over a replay-time guard miss.
#[tokio::test]
async fn replay_honors_committed_cross_entity_guarded_transition() {
    use temper_runtime::persistence::EventStore;
    use temper_store_sim::SimEventStore;

    // A minimal File-shaped automaton whose advancing action is gated on a
    // cross-entity Workspace status that replay cannot reconstruct.
    let file_table = Arc::new(RwLock::new(TransitionTable::from_ioa_source(
        r#"
[automaton]
name = "File"
states = ["Created", "Ready"]
initial = "Created"

[[state]]
name = "version_count"
type = "counter"
initial = "0"

[[state]]
name = "has_content"
type = "bool"
initial = "false"

[[action]]
name = "Create"
kind = "input"
from = ["Created"]
to = "Created"
params = ["workspace_id"]

[[action]]
name = "StreamUpdated"
kind = "input"
from = ["Created", "Ready"]
to = "Ready"
params = ["size_bytes"]
guard = [
  { type = "cross_entity_state", entity_type = "Workspace", entity_id_source = "workspace_id", forbidden_status = ["Frozen", "Archived"] },
]
effect = [
  { type = "increment", var = "version_count" },
  { type = "set_bool", var = "has_content", value = "true" },
]
"#,
    )));

    let store = Arc::new(SimEventStore::no_faults(7));
    let pid = "default:File:fl-replay-1";

    let event = |action: &str, from: &str, to: &str| PersistenceEnvelope {
        sequence_nr: 0,
        event_type: action.to_string(),
        payload: serde_json::json!({
            "action": action,
            "from_status": from,
            "to_status": to,
            "timestamp": "2024-01-01T00:00:00Z",
            "params": {}
        }),
        metadata: EventMetadata {
            event_id: sim_uuid(),
            causation_id: sim_uuid(),
            correlation_id: sim_uuid(),
            timestamp: sim_now(),
            actor_id: pid.to_string(),
        },
    };

    // The committed history: a File that was created, then advanced to Ready by
    // a guarded StreamUpdated. No Workspace entity is in scope at replay time.
    store
        .append(pid, 0, &[event("Created", "", "Created")])
        .await
        .unwrap();
    store
        .append(pid, 1, &[event("Create", "Created", "Created")])
        .await
        .unwrap();
    store
        .append(pid, 2, &[event("StreamUpdated", "Created", "Ready")])
        .await
        .unwrap();

    let system = ActorSystem::new("sim-replay-cross-entity");
    let actor = EntityActor::with_persistence(
        "File",
        "fl-replay-1",
        file_table,
        serde_json::json!({}),
        crate::storage::BoxedEventStore::from_arc(store),
        crate::storage::BackendLabel::Sim,
    );
    let actor_ref = system.spawn(actor, "fl-replay-1");

    let response: EntityResponse = actor_ref
        .ask(EntityMsg::GetState, Duration::from_secs(5))
        .await
        .unwrap();

    assert!(response.success);
    // The committed StreamUpdated transition survives replay despite the guard
    // being unsatisfiable without the Workspace in scope.
    assert_eq!(
        response.state.status, "Ready",
        "a committed cross-entity-guarded transition must not be dropped on replay"
    );
    // Its effects were re-applied: content flag set, version bumped.
    assert_eq!(response.state.booleans.get("has_content"), Some(&true));
    assert_eq!(response.state.counters.get("version_count"), Some(&1));
}
