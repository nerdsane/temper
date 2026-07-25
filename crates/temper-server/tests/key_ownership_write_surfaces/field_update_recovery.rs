//! Recovery and replay boundaries for durable PATCH/PUT field updates.

use super::*;
use temper_runtime::persistence::{
    COMPOSITE_EVENT_TYPE, CompositeEvent, EntityKeyRow, EventMetadata, IndexReconciliation,
    PersistenceEnvelope,
};
use temper_runtime::scheduler::{sim_now, sim_uuid};
use temper_server::entity_actor::types::{EntityEvent, MAX_EVENTS_SINCE_SNAPSHOT};

const FIELD_UPDATE_COLLISION_IOA: &str = r#"
[automaton]
name = "CollisionDoc"
states = ["New", "Ready"]
initial = "New"

[[state]]
name = "Marker"
type = "string"
initial = ""

[[action]]
name = "Temper.Internal.FieldUpdate.v1"
kind = "input"
from = ["New"]
to = "Ready"
params = ["Marker"]
"#;

const BUDGET_DOC_IOA: &str = r#"
[automaton]
name = "BudgetDoc"
states = ["Ready"]
initial = "Ready"

[[state]]
name = "Value"
type = "string"
initial = ""

[[action]]
name = "Noop"
kind = "input"
from = ["Ready"]
to = "Ready"
params = ["Value"]
"#;

fn event_envelope(persistence_id: &str, event: EntityEvent) -> PersistenceEnvelope {
    PersistenceEnvelope {
        sequence_nr: 0,
        event_type: event.action.clone(),
        payload: serde_json::to_value(&event).expect("event serialization"),
        metadata: EventMetadata {
            event_id: sim_uuid(),
            causation_id: sim_uuid(),
            correlation_id: sim_uuid(),
            timestamp: event.timestamp,
            actor_id: persistence_id.to_string(),
        },
    }
}

/// A caller token identifies one exact PATCH/PUT intent. Reusing it for a
/// different field set or replacement mode must fail closed while an exact
/// retry remains successful.
#[tokio::test]
async fn field_update_idempotency_rejects_mismatched_live_intent() {
    let (_guard, _clock, _ids) = install_deterministic_context(292);
    let events = BoxedEventStore::new(SimEventStore::no_faults(292));
    let table = Arc::new(RwLock::new(TransitionTable::from_ioa_source(DOC_IOA)));
    let system = ActorSystem::new("arn238-field-update-live-intent");
    let actor = system.spawn(
        EntityActor::with_persistence(
            "Doc",
            "live-intent",
            table,
            serde_json::json!({}),
            events,
            BackendLabel::Sim,
        )
        .with_tenant("default"),
        "live-intent",
    );

    let token = "field-update-intent-live".to_string();
    let fields = serde_json::json!({
        "WorkspaceId": "ws",
        "Path": "/first",
        "Metadata": {"z": 1, "a": 2}
    });
    let first = update_with_idempotency(&actor, fields.clone(), false, token.clone()).await;
    assert!(first.success, "first PATCH failed: {:?}", first.error);

    let reordered_fields = serde_json::json!({
        "Metadata": {"a": 2, "z": 1},
        "Path": "/first",
        "WorkspaceId": "ws"
    });
    let exact_retry = update_with_idempotency(&actor, reordered_fields, false, token.clone()).await;
    assert!(
        exact_retry.success,
        "exact PATCH retry failed: {:?}",
        exact_retry.error
    );
    assert_eq!(exact_retry.state.sequence_nr, first.state.sequence_nr);

    let different_fields = update_with_idempotency(
        &actor,
        serde_json::json!({"WorkspaceId": "ws", "Path": "/second"}),
        false,
        token.clone(),
    )
    .await;
    assert!(
        !different_fields.success,
        "a reused token must not acknowledge a different PATCH"
    );
    assert_eq!(different_fields.state.fields["Path"], "/first");
    assert_eq!(different_fields.state.sequence_nr, first.state.sequence_nr);

    let different_semantics = update_with_idempotency(&actor, fields, true, token).await;
    assert!(
        !different_semantics.success,
        "a reused token must not acknowledge PATCH as PUT"
    );
    assert_eq!(different_semantics.state.fields["Path"], "/first");
    assert_eq!(
        different_semantics.state.sequence_nr,
        first.state.sequence_nr
    );
}

/// Durable replay must retain the request-intent binding across actor restart.
#[tokio::test]
async fn field_update_idempotency_rejects_mismatched_intent_after_restart() {
    let (_guard, _clock, _ids) = install_deterministic_context(293);
    let events = BoxedEventStore::new(SimEventStore::no_faults(293));
    let table = Arc::new(RwLock::new(TransitionTable::from_ioa_source(DOC_IOA)));
    let system = ActorSystem::new("arn238-field-update-restart-intent");
    let actor = system.spawn(
        EntityActor::with_persistence(
            "Doc",
            "restart-intent",
            table.clone(),
            serde_json::json!({}),
            events.clone(),
            BackendLabel::Sim,
        )
        .with_tenant("default"),
        "restart-intent",
    );
    let token = "field-update-intent-restart".to_string();
    let fields = serde_json::json!({
        "WorkspaceId": "ws",
        "Path": "/first",
        "Metadata": {"z": 1, "a": 2}
    });
    let first = update_with_idempotency(&actor, fields.clone(), false, token.clone()).await;
    assert!(first.success, "first PATCH failed: {:?}", first.error);
    drop(actor);
    drop(system);

    let restarted = ActorSystem::new("arn238-field-update-restart-intent-recovered");
    let recovered = restarted.spawn(
        EntityActor::with_persistence(
            "Doc",
            "restart-intent",
            table,
            serde_json::json!({}),
            events,
            BackendLabel::Sim,
        )
        .with_tenant("default"),
        "restart-intent",
    );
    let reordered_fields = serde_json::json!({
        "Metadata": {"a": 2, "z": 1},
        "Path": "/first",
        "WorkspaceId": "ws"
    });
    let exact_retry =
        update_with_idempotency(&recovered, reordered_fields, false, token.clone()).await;
    assert!(
        exact_retry.success,
        "exact PATCH retry failed after restart: {:?}",
        exact_retry.error
    );
    assert_eq!(exact_retry.state.sequence_nr, first.state.sequence_nr);

    let mismatched = update_with_idempotency(
        &recovered,
        serde_json::json!({"WorkspaceId": "ws", "Path": "/after-restart"}),
        false,
        token,
    )
    .await;
    assert!(
        !mismatched.success,
        "durable replay must reject a mismatched token reuse"
    );
    assert_eq!(mismatched.state.fields["Path"], "/first");
    assert_eq!(mismatched.state.sequence_nr, first.state.sequence_nr);
}

/// Replay must bind a field-update key to its exact envelope even when an
/// internal audit envelope consumed the preceding journal sequence.
#[tokio::test]
async fn field_update_idempotency_uses_exact_sequence_after_internal_envelope() {
    let (_guard, _clock, _ids) = install_deterministic_context(294);
    let sim = SimEventStore::no_faults(294);
    let events = BoxedEventStore::new(sim.clone());
    let persistence_id = "default:Doc:internal-before-field-update";
    let metadata = |event_id| EventMetadata {
        event_id,
        causation_id: sim_uuid(),
        correlation_id: sim_uuid(),
        timestamp: sim_now(),
        actor_id: persistence_id.to_string(),
    };
    let token = "field-update-after-internal".to_string();
    let fields = serde_json::json!({"WorkspaceId": "ws", "Path": "/first"});
    events
        .append(
            persistence_id,
            0,
            &[
                PersistenceEnvelope {
                    sequence_nr: 0,
                    event_type: COMPOSITE_EVENT_TYPE.to_string(),
                    payload: serde_json::to_value(CompositeEvent {
                        tenant: "default".to_string(),
                        parent_entity_type: "Doc".to_string(),
                        parent_entity_id: "internal-before-field-update".to_string(),
                        parent_action: "Audit".to_string(),
                        composite_idempotency_key: "internal-audit".to_string(),
                        intent_hash: "audit-intent".to_string(),
                        sub_writes: vec![],
                    })
                    .expect("serialize composite audit"),
                    metadata: metadata(sim_uuid()),
                },
                PersistenceEnvelope {
                    sequence_nr: 0,
                    event_type: "Temper.Internal.FieldUpdate.v1".to_string(),
                    payload: serde_json::json!({
                        "schema": "temper.field-update.v1",
                        "fields": fields.clone(),
                        "replace": false,
                        "idempotency_key": token.clone()
                    }),
                    metadata: metadata(sim_uuid()),
                },
            ],
        )
        .await
        .expect("seed internal and field-update envelopes");

    let table = Arc::new(RwLock::new(TransitionTable::from_ioa_source(DOC_IOA)));
    let system = ActorSystem::new("arn238-field-update-exact-sequence");
    let actor = system.spawn(
        EntityActor::with_persistence(
            "Doc",
            "internal-before-field-update",
            table,
            serde_json::json!({}),
            events,
            BackendLabel::Sim,
        )
        .with_tenant("default"),
        "internal-before-field-update",
    );
    let exact_retry = update_with_idempotency(&actor, fields, false, token).await;
    assert!(
        exact_retry.success,
        "exact retry must load the field-update envelope: {:?}",
        exact_retry.error
    );
    assert_eq!(exact_retry.state.sequence_nr, 2);
    assert_eq!(sim.dump_journal(persistence_id).len(), 2);
}

/// A same-sequence snapshot rewrite changes the legacy field baseline without
/// advancing the journal. The next actor write must detect that source change,
/// recover it, and derive key ownership from the replacement fields.
#[tokio::test]
async fn field_update_retries_when_snapshot_generation_changes_without_journal_advance() {
    let (_guard, _clock, _ids) = install_deterministic_context(280);
    let sim = SimEventStore::no_faults(280);
    let events = BoxedEventStore::new(sim.clone());
    let persistence_id = "default:Doc:snapshot-generation-race";
    let legacy_snapshot = |workspace: &str| {
        serde_json::to_vec(&serde_json::json!({
            "entity_type": "Doc",
            "entity_id": "snapshot-generation-race",
            "status": "Ready",
            "item_count": 0,
            "fields": {
                "Id": "snapshot-generation-race",
                "Status": "Ready",
                "WorkspaceId": workspace,
                "Path": "/journal"
            }
        }))
        .expect("serialize legacy snapshot")
    };
    events
        .save_snapshot(persistence_id, 1, &legacy_snapshot("ws-before"))
        .await
        .expect("seed first snapshot baseline");
    let timestamp = sim_now();
    events
        .append(
            persistence_id,
            0,
            &[PersistenceEnvelope {
                sequence_nr: 1,
                event_type: "Temper.Internal.FieldUpdate.v1".to_string(),
                payload: serde_json::json!({
                    "schema": "temper.field-update.v1",
                    "fields": {"Path": "/journal"},
                    "replace": false,
                    "idempotency_key": "snapshot-generation-seed"
                }),
                metadata: EventMetadata {
                    event_id: sim_uuid(),
                    causation_id: sim_uuid(),
                    correlation_id: sim_uuid(),
                    timestamp,
                    actor_id: persistence_id.to_string(),
                },
            }],
        )
        .await
        .expect("seed equal-sequence journal generation");

    let table = Arc::new(RwLock::new(TransitionTable::from_ioa_source(DOC_IOA)));
    let system = ActorSystem::new("arn238-snapshot-generation-race");
    let actor = system.spawn(
        EntityActor::with_persistence(
            "Doc",
            "snapshot-generation-race",
            table,
            serde_json::json!({}),
            events.clone(),
            BackendLabel::Sim,
        )
        .with_tenant("default"),
        "snapshot-generation-race",
    );
    let before_rewrite = state(&actor).await.state;
    assert_eq!(before_rewrite.fields["WorkspaceId"], "ws-before");

    events
        .save_snapshot(persistence_id, 1, &legacy_snapshot("ws-after"))
        .await
        .expect("rewrite the captured snapshot generation");

    let updated = update(&actor, serde_json::json!({"Path": "/after"}), false).await;
    assert!(
        updated.success,
        "field update retry failed: {:?}",
        updated.error
    );
    assert_eq!(
        updated.state.fields["WorkspaceId"], "ws-after",
        "the append must recover the replacement snapshot before deriving state"
    );
    assert_eq!(updated.state.fields["Path"], "/after");
    assert_eq!(
        events
            .lookup_by_key(
                "default",
                "Doc",
                "path",
                &doc_key_hash("ws-after", "/after"),
            )
            .await
            .expect("lookup replacement ownership"),
        Some("snapshot-generation-race".to_string())
    );
    assert_eq!(
        events
            .lookup_by_key(
                "default",
                "Doc",
                "path",
                &doc_key_hash("ws-before", "/after"),
            )
            .await
            .expect("lookup stale ownership"),
        None
    );
}

#[tokio::test]
async fn delete_retries_against_a_same_sequence_snapshot_rewrite() {
    let (_guard, _clock, _ids) = install_deterministic_context(287);
    let sim = SimEventStore::no_faults(287);
    let events = BoxedEventStore::new(sim.clone());
    let persistence_id = "default:Doc:delete-snapshot-generation-race";
    let legacy_snapshot = |workspace: &str| {
        serde_json::to_vec(&serde_json::json!({
            "entity_type": "Doc",
            "entity_id": "delete-snapshot-generation-race",
            "status": "Ready",
            "item_count": 0,
            "fields": {
                "Id": "delete-snapshot-generation-race",
                "Status": "Ready",
                "WorkspaceId": workspace,
                "Path": "/delete-me"
            }
        }))
        .expect("serialize legacy snapshot")
    };
    let captured = legacy_snapshot("ws-before-delete");
    events
        .save_snapshot(persistence_id, 1, &captured)
        .await
        .expect("seed captured snapshot");
    let key_set_signature = {
        let table = TransitionTable::from_ioa_source(DOC_IOA);
        declared_key_set_signature(&table.keys)
    };
    events
        .append_with_index_rows(
            persistence_id,
            0,
            &[PersistenceEnvelope {
                sequence_nr: 0,
                event_type: "Temper.Internal.FieldUpdate.v1".to_string(),
                payload: serde_json::json!({
                    "schema": "temper.field-update.v1",
                    "fields": {"Path": "/delete-me"},
                    "replace": false,
                    "idempotency_key": "delete-source-seed"
                }),
                metadata: EventMetadata {
                    event_id: sim_uuid(),
                    causation_id: sim_uuid(),
                    correlation_id: sim_uuid(),
                    timestamp: sim_now(),
                    actor_id: persistence_id.to_string(),
                },
            }],
            &[EntityKeyRow {
                key_name: "path".to_string(),
                key_hash: doc_key_hash("ws-before-delete", "/delete-me"),
            }],
            &[],
            IndexReconciliation {
                keys: true,
                key_set_signature: Some(key_set_signature),
                vectors: false,
                snapshot_source: Default::default(),
            },
        )
        .await
        .expect("seed equal-sequence journal and key row");

    let table = Arc::new(RwLock::new(TransitionTable::from_ioa_source(DOC_IOA)));
    let system = ActorSystem::new("arn238-delete-snapshot-generation-race");
    let actor = system.spawn(
        EntityActor::with_persistence(
            "Doc",
            "delete-snapshot-generation-race",
            table,
            serde_json::json!({}),
            events.clone(),
            BackendLabel::Sim,
        )
        .with_tenant("default"),
        "delete-snapshot-generation-race",
    );
    assert_eq!(
        state(&actor).await.state.fields["WorkspaceId"],
        "ws-before-delete"
    );
    events
        .save_snapshot(persistence_id, 1, &legacy_snapshot("ws-after-delete"))
        .await
        .expect("replace captured snapshot generation");

    let deleted: EntityResponse = actor
        .ask(EntityMsg::Delete, Duration::from_secs(5))
        .await
        .expect("delete response");
    assert!(deleted.success, "delete failed: {:?}", deleted.error);
    assert_eq!(deleted.state.status, "Deleted");
    assert_eq!(deleted.state.sequence_nr, 2);
    assert_eq!(deleted.state.fields["WorkspaceId"], "ws-after-delete");
    assert_eq!(sim.dump_journal(persistence_id).len(), 2);
    assert_eq!(
        events
            .lookup_by_key(
                "default",
                "Doc",
                "path",
                &doc_key_hash("ws-before-delete", "/delete-me"),
            )
            .await
            .expect("lookup purged key"),
        None
    );
}

/// A real intervening journal append must make PATCH rebuild from the durable
/// stream before retrying. The retried update keeps the other writer's fields,
/// moves exact key ownership, and leaves the actor fresh for the next message.
#[tokio::test]
async fn field_update_replays_real_concurrent_append_before_retry() {
    let (_guard, _clock, _ids) = install_deterministic_context(240);
    let sim = SimEventStore::no_faults(240);
    let events = BoxedEventStore::new(sim.clone());
    let table = Arc::new(RwLock::new(TransitionTable::from_ioa_source(DOC_IOA)));
    let system = ActorSystem::new("arn238-field-update-real-race");
    let actor = system.spawn(
        EntityActor::with_persistence(
            "Doc",
            "doc-race",
            table.clone(),
            serde_json::json!({}),
            events.clone(),
            BackendLabel::Sim,
        )
        .with_tenant("default"),
        "doc-race",
    );

    assert!(
        action(
            &actor,
            "Create",
            serde_json::json!({"WorkspaceId": "ws", "Path": "/before"}),
        )
        .await
        .success
    );
    let before_race = state(&actor).await.state;
    let persistence_id = "default:Doc:doc-race";
    let external_event = EntityEvent {
        action: "Rekey".to_string(),
        from_status: "Ready".to_string(),
        to_status: "Ready".to_string(),
        timestamp: sim_now(),
        params: serde_json::json!({
            "WorkspaceId": "ws",
            "Path": "/external",
            "ExternalOnly": "must-survive"
        }),
        idempotency_key: None,
    };
    let key_set_signature = {
        let table = table.read().expect("table lock");
        declared_key_set_signature(&table.keys)
    };
    events
        .append_with_index_rows(
            persistence_id,
            before_race.sequence_nr,
            &[event_envelope(persistence_id, external_event)],
            &[EntityKeyRow {
                key_name: "path".to_string(),
                key_hash: doc_key_hash("ws", "/external"),
            }],
            &[],
            IndexReconciliation {
                keys: true,
                key_set_signature: Some(key_set_signature),
                vectors: false,
                snapshot_source: Default::default(),
            },
        )
        .await
        .expect("intervening writer append");

    let retried = update(&actor, serde_json::json!({"Path": "/after-race"}), false).await;
    assert!(retried.success, "PATCH retry failed: {:?}", retried.error);
    assert_eq!(retried.state.sequence_nr, before_race.sequence_nr + 2);
    assert_eq!(retried.state.fields["ExternalOnly"], "must-survive");
    assert_eq!(retried.state.fields["Path"], "/after-race");
    assert_eq!(
        events
            .lookup_by_key("default", "Doc", "path", &doc_key_hash("ws", "/external"),)
            .await
            .expect("external key lookup"),
        None
    );
    assert_eq!(
        events
            .lookup_by_key("default", "Doc", "path", &doc_key_hash("ws", "/after-race"),)
            .await
            .expect("retried key lookup"),
        Some("doc-race".to_string())
    );

    let follow_up = update(
        &actor,
        serde_json::json!({"Path": "/after-follow-up"}),
        false,
    )
    .await;
    assert!(
        follow_up.success,
        "actor remained stale after retry: {:?}",
        follow_up.error
    );
    assert_eq!(follow_up.state.sequence_nr, before_race.sequence_nr + 3);
    assert_eq!(follow_up.state.fields["ExternalOnly"], "must-survive");
}

/// If catch-up discovers that another writer committed the terminal tombstone,
/// PATCH must reject instead of appending an unreplayable suffix after `Deleted`.
#[tokio::test]
async fn field_update_rejects_concurrent_delete_and_restarts_at_tombstone() {
    let (_guard, _clock, _ids) = install_deterministic_context(244);
    let sim = SimEventStore::no_faults(244);
    let events = BoxedEventStore::new(sim.clone());
    let table = Arc::new(RwLock::new(TransitionTable::from_ioa_source(DOC_IOA)));
    let system = ActorSystem::new("arn238-field-update-delete-race");
    let actor = system.spawn(
        EntityActor::with_persistence(
            "Doc",
            "doc-deleted",
            table.clone(),
            serde_json::json!({}),
            events.clone(),
            BackendLabel::Sim,
        )
        .with_tenant("default"),
        "doc-deleted",
    );
    assert!(
        action(
            &actor,
            "Create",
            serde_json::json!({"WorkspaceId": "ws", "Path": "/deleted"}),
        )
        .await
        .success
    );
    let created = state(&actor).await.state;
    let persistence_id = "default:Doc:doc-deleted";
    let tombstone = EntityEvent {
        action: "Deleted".to_string(),
        from_status: created.status.clone(),
        to_status: "Deleted".to_string(),
        timestamp: sim_now(),
        params: serde_json::json!({}),
        idempotency_key: None,
    };
    let signature = declared_key_set_signature(&table.read().expect("table lock").keys);
    events
        .append_with_index_rows(
            persistence_id,
            created.sequence_nr,
            &[event_envelope(persistence_id, tombstone)],
            &[],
            &[],
            IndexReconciliation {
                keys: true,
                key_set_signature: Some(signature),
                vectors: false,
                snapshot_source: Default::default(),
            },
        )
        .await
        .expect("external tombstone append");
    let tombstone_sequence = created.sequence_nr + 1;
    assert_eq!(
        sim.dump_journal(persistence_id).len(),
        tombstone_sequence as usize,
        "precondition: the external writer committed only the tombstone"
    );

    let rejected = update(
        &actor,
        serde_json::json!({"Path": "/must-not-append"}),
        false,
    )
    .await;
    assert!(!rejected.success);
    assert_eq!(
        rejected.error.as_deref(),
        Some("cannot update a deleted entity")
    );
    assert_eq!(rejected.state.status, "Deleted");
    assert_eq!(rejected.state.sequence_nr, tombstone_sequence);
    assert_eq!(
        sim.dump_journal(persistence_id).len(),
        tombstone_sequence as usize,
        "rejected PATCH must not append after the tombstone"
    );

    drop(actor);
    drop(system);
    let restarted = ActorSystem::new("arn238-field-update-delete-race-restart");
    let recovered = restarted.spawn(
        EntityActor::with_persistence(
            "Doc",
            "doc-deleted",
            table,
            serde_json::json!({}),
            events,
            BackendLabel::Sim,
        )
        .with_tenant("default"),
        "doc-deleted",
    );
    let recovered = state(&recovered).await;
    assert_eq!(recovered.state.status, "Deleted");
    assert_eq!(recovered.state.sequence_nr, tombstone_sequence);
    assert_eq!(
        sim.dump_journal(persistence_id).len(),
        tombstone_sequence as usize
    );
}

/// The private PATCH/PUT event type is not a reserved spec action name. A
/// normal action with that exact name carries an EntityEvent payload and must
/// replay through the transition table after restart.
#[tokio::test]
async fn internal_field_update_event_type_does_not_shadow_spec_action() {
    let (_guard, _clock, _ids) = install_deterministic_context(241);
    let sim = SimEventStore::no_faults(241);
    let events = BoxedEventStore::new(sim);
    let table = Arc::new(RwLock::new(TransitionTable::from_ioa_source(
        FIELD_UPDATE_COLLISION_IOA,
    )));
    let system = ActorSystem::new("arn238-field-update-action-collision");
    let actor = system.spawn(
        EntityActor::with_persistence(
            "CollisionDoc",
            "collision",
            table.clone(),
            serde_json::json!({}),
            events.clone(),
            BackendLabel::Sim,
        )
        .with_tenant("default"),
        "collision",
    );

    let dispatched = action(
        &actor,
        "Temper.Internal.FieldUpdate.v1",
        serde_json::json!({"Marker": "ordinary-action"}),
    )
    .await;
    assert!(dispatched.success, "collision action failed");
    assert_eq!(dispatched.state.status, "Ready");
    drop(actor);
    drop(system);

    let restarted = ActorSystem::new("arn238-field-update-action-collision-restart");
    let recovered = restarted.spawn(
        EntityActor::with_persistence(
            "CollisionDoc",
            "collision",
            table,
            serde_json::json!({}),
            events,
            BackendLabel::Sim,
        )
        .with_tenant("default"),
        "collision",
    );
    let recovered = state(&recovered).await;
    assert_eq!(recovered.state.status, "Ready");
    assert_eq!(recovered.state.fields["Marker"], "ordinary-action");
}

/// PATCH/PUT and terminal Delete share the bounded raw replay-tail budget with
/// spec actions. A legacy snapshot must not reset that reconstructed budget; at
/// the exact cap both writes refuse to append and restart remains bounded.
#[tokio::test]
async fn field_update_respects_replay_tail_budget_and_restart_boundary() {
    let (_guard, _clock, _ids) = install_deterministic_context(242);
    let sim = SimEventStore::no_faults(242);
    let events = BoxedEventStore::new(sim.clone());
    let persistence_id = "default:BudgetDoc:budget";
    let envelopes = (0..MAX_EVENTS_SINCE_SNAPSHOT)
        .map(|index| {
            event_envelope(
                persistence_id,
                EntityEvent {
                    action: "Noop".to_string(),
                    from_status: "Ready".to_string(),
                    to_status: "Ready".to_string(),
                    timestamp: sim_now(),
                    params: serde_json::json!({"Value": index.to_string()}),
                    idempotency_key: None,
                },
            )
        })
        .collect::<Vec<_>>();
    events
        .append(persistence_id, 0, &envelopes)
        .await
        .expect("seed replay-tail boundary");
    let legacy_snapshot = serde_json::to_vec(&serde_json::json!({
        "entity_type": "BudgetDoc",
        "entity_id": "budget",
        "status": "Ready",
        "item_count": 0,
        "counters": {},
        "booleans": {},
        "lists": {},
        "fields": {"Id": "budget", "Status": "Ready", "Value": "legacy"},
        "events": [],
        "total_event_count": 5,
        "events_since_snapshot": 0,
        "last_snapshot_sequence_nr": 5,
        "sequence_nr": 5,
        "processed_idempotency_keys": {}
    }))
    .expect("serialize legacy snapshot");
    events
        .save_snapshot(persistence_id, 5, &legacy_snapshot)
        .await
        .expect("seed untrusted legacy snapshot");

    let table = Arc::new(RwLock::new(TransitionTable::from_ioa_source(
        BUDGET_DOC_IOA,
    )));
    let system = ActorSystem::new("arn238-field-update-budget");
    let actor = system.spawn(
        EntityActor::with_persistence(
            "BudgetDoc",
            "budget",
            table.clone(),
            serde_json::json!({}),
            events.clone(),
            BackendLabel::Sim,
        )
        .with_tenant("default"),
        "budget",
    );
    let at_boundary = state(&actor).await;
    assert_eq!(
        at_boundary.state.events_since_snapshot,
        MAX_EVENTS_SINCE_SNAPSHOT
    );

    let rejected = update(
        &actor,
        serde_json::json!({"Value": "must-not-append"}),
        false,
    )
    .await;
    assert!(!rejected.success);
    assert!(
        rejected
            .error
            .as_deref()
            .is_some_and(|error| error.contains("Event budget exhausted"))
    );
    assert_eq!(
        sim.dump_journal(persistence_id).len(),
        MAX_EVENTS_SINCE_SNAPSHOT
    );
    let rejected_delete: temper_server::entity_actor::EntityResponse = actor
        .ask(EntityMsg::Delete, Duration::from_secs(5))
        .await
        .expect("delete response");
    assert!(!rejected_delete.success);
    assert!(
        rejected_delete
            .error
            .as_deref()
            .is_some_and(|error| error.contains("Event budget exhausted"))
    );
    assert_ne!(rejected_delete.state.status, "Deleted");
    assert_eq!(
        sim.dump_journal(persistence_id).len(),
        MAX_EVENTS_SINCE_SNAPSHOT
    );
    drop(actor);
    drop(system);

    let restarted = ActorSystem::new("arn238-field-update-budget-restart");
    let recovered = restarted.spawn(
        EntityActor::with_persistence(
            "BudgetDoc",
            "budget",
            table,
            serde_json::json!({}),
            events,
            BackendLabel::Sim,
        )
        .with_tenant("default"),
        "budget",
    );
    let recovered = state(&recovered).await;
    assert_eq!(
        recovered.state.events_since_snapshot,
        MAX_EVENTS_SINCE_SNAPSHOT
    );
    assert_eq!(
        recovered.state.sequence_nr,
        MAX_EVENTS_SINCE_SNAPSHOT as u64
    );
}
