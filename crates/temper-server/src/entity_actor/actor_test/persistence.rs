use super::*;

#[test]
fn event_budget_workspace_id_uses_workspace_entity_id_or_field() {
    let workspace_state = EntityState {
        entity_type: "Workspace".to_string(),
        entity_id: "ws-1".to_string(),
        status: "Active".to_string(),
        item_count: 0,
        counters: BTreeMap::new(),
        booleans: BTreeMap::new(),
        lists: BTreeMap::new(),
        fields: serde_json::json!({"WorkspaceId": "ignored"}),
        events: std::collections::VecDeque::new(),
        state_timeout_clock_reset_at: None,
        state_timeout_clock_reset_version: None,
        total_event_count: 0,
        events_since_snapshot: 0,
        last_snapshot_sequence_nr: 0,
        sequence_nr: 0,
        processed_idempotency_keys: BTreeMap::new(),
    };
    assert_eq!(event_budget_workspace_id(&workspace_state), "ws-1");

    let file_state = EntityState {
        entity_type: "File".to_string(),
        entity_id: "fl-1".to_string(),
        status: "Ready".to_string(),
        item_count: 0,
        counters: BTreeMap::new(),
        booleans: BTreeMap::new(),
        lists: BTreeMap::new(),
        fields: serde_json::json!({"workspace_id": "ws-2"}),
        events: std::collections::VecDeque::new(),
        state_timeout_clock_reset_at: None,
        state_timeout_clock_reset_version: None,
        total_event_count: 0,
        events_since_snapshot: 0,
        last_snapshot_sequence_nr: 0,
        sequence_nr: 0,
        processed_idempotency_keys: BTreeMap::new(),
    };
    assert_eq!(event_budget_workspace_id(&file_state), "ws-2");
}

#[test]
fn current_snapshot_round_trip_preserves_state_timeout_clock_anchor() {
    let table = TransitionTable::from_ioa_source(
        r#"
[automaton]
name = "TimedTask"
states = ["Running", "TimedOut"]
initial = "Running"
allow_indefinite_states = ["TimedOut"]

[[action]]
name = "TimeoutFail"
kind = "internal"
from = ["Running"]
to = "TimedOut"

[[state_timeout]]
state = "Running"
after_seconds = 60
on_timeout = "TimeoutFail"
"#,
    );
    let reset_at = chrono::DateTime::<chrono::Utc>::from_timestamp(1_700_000_000, 0).unwrap();
    let mut state = EntityActor::build_initial_state(
        "TimedTask",
        "snapshot-anchor",
        &table,
        &serde_json::json!({}),
    );
    let created = EntityEvent {
        action: "Created".to_string(),
        from_status: String::new(),
        to_status: "Running".to_string(),
        timestamp: reset_at,
        params: serde_json::json!({"Id": "snapshot-anchor"}),
        idempotency_key: None,
    };
    EntityActor::update_state_timeout_clock(&table, &mut state, &created);
    state.push_event_bounded(created);
    state.sequence_nr = 1;

    let snapshot = EntityActor::serialize_snapshot_state(&state).expect("serialize snapshot");
    let payload: serde_json::Value =
        serde_json::from_slice(&snapshot).expect("snapshot JSON remains readable");
    assert_eq!(
        payload.get("state_timeout_clock_reset_at"),
        Some(&serde_json::json!(reset_at))
    );
    assert_eq!(
        payload.get("state_timeout_clock_reset_version"),
        Some(&serde_json::json!(1))
    );
    assert_eq!(
        payload.get(STATE_TIMEOUT_CLOCK_SNAPSHOT_AUTHORITY_KEY),
        Some(&serde_json::Value::Bool(true))
    );
    assert!(
        payload.get("events").is_none(),
        "hot events remain excluded"
    );

    let mut restored = EntityActor::build_initial_state(
        "TimedTask",
        "snapshot-anchor",
        &table,
        &serde_json::json!({}),
    );
    assert_eq!(
        EntityActor::apply_snapshot_bytes(&mut restored, 1, &snapshot),
        Some(true),
        "a current snapshot carries an authoritative timeout-clock checkpoint"
    );
    assert_eq!(restored.state_timeout_clock_reset_at, Some(reset_at));
    assert_eq!(restored.state_timeout_clock_reset_version, Some(1));
    assert!(restored.events.is_empty());

    let mut false_marker: serde_json::Value =
        serde_json::from_slice(&snapshot).expect("snapshot remains JSON");
    false_marker
        .as_object_mut()
        .expect("snapshot object")
        .insert(
            STATE_TIMEOUT_CLOCK_SNAPSHOT_AUTHORITY_KEY.to_string(),
            serde_json::Value::Bool(false),
        );
    let false_marker = serde_json::to_vec(&false_marker).expect("encode false-marker fixture");
    assert_eq!(
        EntityActor::apply_snapshot_bytes(&mut restored, 1, &false_marker),
        None,
        "current snapshots cannot downgrade their clock provenance"
    );

    let mut half_pair: serde_json::Value =
        serde_json::from_slice(&snapshot).expect("snapshot remains JSON");
    half_pair
        .as_object_mut()
        .expect("snapshot object")
        .remove("state_timeout_clock_reset_version");
    let half_pair = serde_json::to_vec(&half_pair).expect("encode half-pair fixture");
    assert_eq!(
        EntityActor::apply_snapshot_bytes(&mut restored, 1, &half_pair),
        None,
        "an authoritative snapshot must carry a complete clock pair"
    );

    let mut ahead_of_boundary: serde_json::Value =
        serde_json::from_slice(&snapshot).expect("snapshot remains JSON");
    ahead_of_boundary
        .as_object_mut()
        .expect("snapshot object")
        .insert(
            "state_timeout_clock_reset_version".to_string(),
            serde_json::json!(2),
        );
    let ahead_of_boundary =
        serde_json::to_vec(&ahead_of_boundary).expect("encode deferred-head fixture");
    assert_eq!(
        EntityActor::apply_snapshot_bytes(&mut restored, 1, &ahead_of_boundary),
        Some(true),
        "clock versions ahead of a replacement boundary require the captured journal head"
    );
    let head_error = EntityActor::validate_snapshot_timeout_clock_against_journal_head(
        "default:TimedTask:clock-head-validation",
        &restored,
        1,
        true,
    )
    .expect_err("a clock identity cannot exceed the proven journal head");
    assert!(
        head_error
            .to_string()
            .contains("journal_head_sequence_nr=1"),
        "unexpected head-validation error: {head_error}"
    );
    EntityActor::validate_snapshot_timeout_clock_against_journal_head(
        "default:TimedTask:clock-head-validation",
        &restored,
        2,
        true,
    )
    .expect("a repaired clock identity may exceed its older replacement boundary");
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn tombstone_replay_clears_state_timeout_clock_anchor() {
    use temper_runtime::persistence::EventStore;
    use temper_store_sim::SimEventStore;

    let table = Arc::new(RwLock::new(TransitionTable::from_ioa_source(
        r#"
[automaton]
name = "TimedTask"
states = ["Running", "TimedOut"]
initial = "Running"
allow_indefinite_states = ["TimedOut"]

[[action]]
name = "TimeoutFail"
kind = "internal"
from = ["Running"]
to = "TimedOut"

[[state_timeout]]
state = "Running"
after_seconds = 60
on_timeout = "TimeoutFail"
"#,
    )));
    let store = Arc::new(SimEventStore::no_faults(204));
    let persistence_id = "default:TimedTask:tombstoned";
    let reset_at = sim_now();
    let envelope = |event_type: &str, payload: serde_json::Value| PersistenceEnvelope {
        sequence_nr: 0,
        event_type: event_type.to_string(),
        payload,
        metadata: EventMetadata {
            event_id: sim_uuid(),
            causation_id: sim_uuid(),
            correlation_id: sim_uuid(),
            timestamp: reset_at,
            actor_id: persistence_id.to_string(),
        },
    };
    store
        .append(
            persistence_id,
            0,
            &[
                envelope(
                    "Created",
                    serde_json::json!({
                        "action": "Created",
                        "from_status": "",
                        "to_status": "Running",
                        "timestamp": reset_at,
                        "params": {}
                    }),
                ),
                envelope(
                    "Deleted",
                    serde_json::json!({
                        "action": "Deleted",
                        "from_status": "Running",
                        "to_status": "Deleted",
                        "timestamp": reset_at,
                        "params": {}
                    }),
                ),
            ],
        )
        .await
        .expect("seed tombstoned timed entity");

    let actor = EntityActor::with_persistence(
        "TimedTask",
        "tombstoned",
        table,
        serde_json::json!({}),
        crate::storage::BoxedEventStore::from_arc(store),
        crate::storage::BackendLabel::Sim,
    );
    let system = ActorSystem::new("timeout-tombstone-replay");
    let actor_ref = system.spawn(actor, "tombstoned");
    let response: EntityResponse = actor_ref
        .ask(EntityMsg::GetState, Duration::from_secs(1))
        .await
        .expect("tombstoned actor hydrates");

    assert_eq!(response.state.status, "Deleted");
    assert_eq!(response.state.state_timeout_clock_reset_at, None);
    assert_eq!(response.state.state_timeout_clock_reset_version, None);
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn replayed_same_timestamp_reset_uses_the_tail_envelope_version() {
    use temper_runtime::persistence::{COMPOSITE_EVENT_TYPE, EventStore};
    use temper_store_sim::SimEventStore;

    let (_guard, _clock, _ids) = temper_runtime::scheduler::install_deterministic_context(219);
    let table = Arc::new(RwLock::new(TransitionTable::from_ioa_source(
        r#"
[automaton]
name = "TimedTask"
states = ["Running", "TimedOut"]
initial = "Running"
allow_indefinite_states = ["TimedOut"]

[[action]]
name = "Progress"
kind = "input"
from = ["Running"]
to = "Running"

[[action]]
name = "TimeoutFail"
kind = "internal"
from = ["Running"]
to = "TimedOut"

[[state_timeout]]
state = "Running"
after_seconds = 60
on_timeout = "TimeoutFail"
reset_on = ["Progress"]
"#,
    )));
    let store = Arc::new(SimEventStore::no_faults(219));
    let persistence_id = "default:TimedTask:replay-reset-version";
    let reset_at = sim_now();
    let envelope = |event_type: &str, payload: serde_json::Value| PersistenceEnvelope {
        sequence_nr: 0,
        event_type: event_type.to_string(),
        payload,
        metadata: EventMetadata {
            event_id: sim_uuid(),
            causation_id: sim_uuid(),
            correlation_id: sim_uuid(),
            timestamp: reset_at,
            actor_id: persistence_id.to_string(),
        },
    };

    let markers: Vec<_> = (0..100)
        .map(|_| envelope(COMPOSITE_EVENT_TYPE, serde_json::json!({})))
        .collect();
    store
        .append(persistence_id, 0, &markers)
        .await
        .expect("seed journal head ahead of the domain event count");

    let mut snapshot_state = EntityActor::build_initial_state(
        "TimedTask",
        "replay-reset-version",
        &table.read().expect("table lock"),
        &serde_json::json!({}),
    );
    snapshot_state.state_timeout_clock_reset_at = Some(reset_at);
    snapshot_state.state_timeout_clock_reset_version = Some(100);
    snapshot_state.total_event_count = 10;
    snapshot_state.sequence_nr = 100;
    snapshot_state.last_snapshot_sequence_nr = 100;
    let snapshot = EntityActor::serialize_snapshot_state(&snapshot_state)
        .expect("snapshot with skewed sequence/count encodes");
    store
        .save_snapshot(persistence_id, 100, &snapshot)
        .await
        .expect("seed snapshot at journal head");
    store
        .append(
            persistence_id,
            100,
            &[envelope(
                "Progress",
                serde_json::json!({
                    "action": "Progress",
                    "from_status": "Running",
                    "to_status": "Running",
                    "timestamp": reset_at,
                    "params": {}
                }),
            )],
        )
        .await
        .expect("append same-timestamp reset after snapshot");

    let expected_timeout = table
        .read()
        .expect("table lock")
        .state_timeouts
        .first()
        .expect("timed table has a declaration")
        .clone();

    let actor = EntityActor::with_persistence(
        "TimedTask",
        "replay-reset-version",
        table,
        serde_json::json!({}),
        crate::storage::BoxedEventStore::from_arc(store),
        crate::storage::BackendLabel::Sim,
    );
    let system = ActorSystem::new("replay-reset-version");
    let actor_ref = system.spawn(actor, "replay-reset-version");
    let recovered: EntityResponse = actor_ref
        .ask(EntityMsg::GetState, Duration::from_secs(1))
        .await
        .expect("snapshot and reset tail hydrate");
    assert_eq!(recovered.state.state_timeout_clock_reset_at, Some(reset_at));
    assert_eq!(
        recovered.state.state_timeout_clock_reset_version,
        Some(101),
        "the current replay envelope, not prior count/sequence metadata, owns the reset"
    );

    let stale: EntityResponse = actor_ref
        .ask(
            EntityMsg::Action {
                name: "TimeoutFail".into(),
                params: serde_json::json!({}),
                cross_entity_booleans: BTreeMap::new(),
                idempotency_key: None,
                state_timeout_precondition: Some(Box::new(
                    crate::entity_actor::StateTimeoutPrecondition {
                        expected_timeout,
                        expected_state: "Running".into(),
                        expected_reset_at: Some(reset_at),
                        expected_reset_version: Some(100),
                    },
                )),
            },
            Duration::from_secs(1),
        )
        .await
        .expect("stale precondition receives a benign actor reply");
    assert!(!stale.success);
    assert_eq!(stale.state.status, "Running");
    assert_eq!(stale.state.sequence_nr, 101);
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn queued_snapshot_only_advances_replay_boundary_after_write_applies() {
    use temper_store_sim::SimEventStore;

    let store = Arc::new(SimEventStore::no_faults(43));
    let boxed_store = crate::storage::BoxedEventStore::from_arc(store);
    let snapshot_queue = SnapshotWriteQueue::start(boxed_store.clone());
    let persistence_id = "default:Order:queued-snapshot-1";
    let mut state = EntityState {
        entity_type: "Order".to_string(),
        entity_id: "queued-snapshot-1".to_string(),
        status: "Draft".to_string(),
        item_count: 0,
        counters: BTreeMap::new(),
        booleans: BTreeMap::new(),
        lists: BTreeMap::new(),
        fields: serde_json::json!({
            "Id": "queued-snapshot-1",
            "Status": "Draft"
        }),
        events: std::collections::VecDeque::new(),
        state_timeout_clock_reset_at: None,
        state_timeout_clock_reset_version: None,
        total_event_count: 100,
        events_since_snapshot: 100,
        last_snapshot_sequence_nr: 0,
        sequence_nr: 100,
        processed_idempotency_keys: BTreeMap::new(),
    };

    EntityActor::maybe_save_snapshot(
        &boxed_store,
        Some(&snapshot_queue),
        persistence_id,
        &mut state,
    )
    .await
    .expect("snapshot enqueue should succeed");

    assert_eq!(snapshot_queue.pending_sequence(persistence_id), Some(100));
    assert_eq!(state.last_snapshot_sequence_nr, 0);
    assert_eq!(state.events_since_snapshot, 100);

    for _ in 0..20 {
        if snapshot_queue.applied_sequence(persistence_id) == Some(100) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(snapshot_queue.applied_sequence(persistence_id), Some(100));

    state.sequence_nr = 101;
    state.total_event_count = 101;
    state.events_since_snapshot = 101;
    EntityActor::maybe_save_snapshot(
        &boxed_store,
        Some(&snapshot_queue),
        persistence_id,
        &mut state,
    )
    .await
    .expect("snapshot boundary observation should succeed");

    assert_eq!(state.last_snapshot_sequence_nr, 100);
    assert_eq!(state.events_since_snapshot, 1);
}
