//! Recovery regression for a terminal tombstone hidden behind a newer stale snapshot.

use super::*;
use temper_runtime::persistence::{EventMetadata, EventStore, PersistenceEnvelope};
use temper_runtime::scheduler::sim_now;
use temper_server::entity_actor::types::EntityEvent;
use temper_store_sim::SimFaultConfig;

const PERSISTENCE_ID: &str = "default:Doc:stale-snapshot";

/// A first journal generation can replace a snapshot-only migration generation
/// without advancing the aggregate sequence. Actor restart must follow the
/// journal just like key-authority reads and backfill do.
#[tokio::test]
async fn actor_recovery_prefers_equal_sequence_journal_generation_over_snapshot() {
    let (_guard, _clock, _ids) = install_deterministic_context(276);
    let sim = SimEventStore::no_faults(276);
    let events = BoxedEventStore::new(sim);
    let persistence_id = "default:Doc:equal-sequence-source";
    let snapshot = serde_json::json!({
        "entity_type": "Doc",
        "entity_id": "equal-sequence-source",
        "status": "Ready",
        "item_count": 0,
        "fields": {
            "Id": "equal-sequence-source",
            "Status": "Ready",
            "WorkspaceId": "ws",
            "Path": "/snapshot-generation"
        }
    });
    events
        .save_snapshot(
            persistence_id,
            1,
            &serde_json::to_vec(&snapshot).expect("serialize snapshot-only generation"),
        )
        .await
        .expect("seed snapshot-only generation");

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
                    "fields": {
                        "Path": "/journal-generation",
                        "JournalOnly": "must-survive"
                    },
                    "replace": false,
                    "idempotency_key": "equal-sequence-source"
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
        .expect("replace snapshot-only source with first journal generation");

    let table = Arc::new(RwLock::new(TransitionTable::from_ioa_source(DOC_IOA)));
    let restarted_system = ActorSystem::new("arn238-equal-sequence-source-restart");
    let restarted = restarted_system.spawn(
        EntityActor::with_persistence(
            "Doc",
            "equal-sequence-source",
            table,
            serde_json::json!({}),
            events,
            BackendLabel::Sim,
        )
        .with_tenant("default"),
        "equal-sequence-source",
    );
    let recovered = state(&restarted).await.state;
    assert_eq!(
        recovered.fields["Path"], "/journal-generation",
        "the first journal generation, not the equal-sequence snapshot, owns current state"
    );
    assert_eq!(recovered.fields["JournalOnly"], "must-survive");
    assert_eq!(
        recovered.status, "New",
        "the journal generation owns lifecycle rather than the snapshot status"
    );
    assert_eq!(recovered.fields["Status"], "New");
    assert_eq!(recovered.sequence_nr, 1);
}

async fn persist_tombstone_behind_stale_live_snapshot(
    events: &BoxedEventStore,
    table: &Arc<RwLock<TransitionTable>>,
) -> u64 {
    let live_system = ActorSystem::new("arn238-tombstone-snapshot-live");
    let live_actor = live_system.spawn(
        EntityActor::with_persistence(
            "Doc",
            "stale-snapshot",
            table.clone(),
            serde_json::json!({}),
            events.clone(),
            BackendLabel::Sim,
        )
        .with_tenant("default"),
        "stale-snapshot",
    );
    assert!(
        action(
            &live_actor,
            "Create",
            serde_json::json!({"WorkspaceId": "ws", "Path": "/deleted"}),
        )
        .await
        .success
    );
    let mut stale_live_snapshot = state(&live_actor).await.state;
    let live_sequence = stale_live_snapshot.sequence_nr;
    let tombstone_sequence = live_sequence + 1;
    let stale_snapshot_sequence = tombstone_sequence + 1;
    drop(live_actor);
    drop(live_system);

    let deleted_at = sim_now();
    events
        .append(
            PERSISTENCE_ID,
            live_sequence,
            &[PersistenceEnvelope {
                sequence_nr: tombstone_sequence,
                event_type: "Delete".to_string(),
                payload: serde_json::to_value(EntityEvent {
                    action: "Delete".to_string(),
                    from_status: "Ready".to_string(),
                    to_status: "Deleted".to_string(),
                    timestamp: deleted_at,
                    params: serde_json::json!({}),
                    idempotency_key: None,
                })
                .expect("serialize tombstone"),
                metadata: EventMetadata {
                    event_id: sim_uuid(),
                    causation_id: sim_uuid(),
                    correlation_id: sim_uuid(),
                    timestamp: deleted_at,
                    actor_id: PERSISTENCE_ID.to_string(),
                },
            }],
        )
        .await
        .expect("append action-named tombstone");

    stale_live_snapshot.sequence_nr = stale_snapshot_sequence;
    stale_live_snapshot.last_snapshot_sequence_nr = stale_snapshot_sequence;
    stale_live_snapshot.events_since_snapshot = 0;
    events
        .save_snapshot(
            PERSISTENCE_ID,
            stale_snapshot_sequence,
            &serde_json::to_vec(&stale_live_snapshot).expect("serialize stale live snapshot"),
        )
        .await
        .expect("persist newer stale live snapshot");

    tombstone_sequence
}

/// A tombstone is terminal durable history. A newer stale snapshot must not let a
/// restarted actor reappear live, even when the domain action is named `Delete`
/// rather than the legacy canonical `Deleted` event.
#[tokio::test]
async fn actor_recovery_replays_older_tombstone_before_newer_live_snapshot() {
    let (_guard, _clock, _ids) = install_deterministic_context(246);
    let sim = SimEventStore::no_faults(246);
    let events = BoxedEventStore::new(sim.clone());
    let table = Arc::new(RwLock::new(TransitionTable::from_ioa_source(DOC_IOA)));
    let tombstone_sequence = persist_tombstone_behind_stale_live_snapshot(&events, &table).await;

    let restarted_system = ActorSystem::new("arn238-tombstone-snapshot-restart");
    let restarted = restarted_system.spawn(
        EntityActor::with_persistence(
            "Doc",
            "stale-snapshot",
            table,
            serde_json::json!({}),
            events,
            BackendLabel::Sim,
        )
        .with_tenant("default"),
        "stale-snapshot",
    );
    let recovered = state(&restarted).await.state;
    assert_eq!(recovered.status, "Deleted");
    assert_eq!(
        recovered.sequence_nr, tombstone_sequence,
        "the terminal journal boundary, not the stale snapshot, is authoritative"
    );
    assert_eq!(
        recovered.last_snapshot_sequence_nr, 0,
        "a rejected snapshot is not an accepted actor snapshot boundary"
    );
}

/// A fault-truncated replay must fail closed at the exact tombstone boundary;
/// after the transient fault clears, a fresh recovery reaches `Deleted`.
#[tokio::test]
async fn actor_recovery_never_accepts_live_state_when_tombstone_replay_truncates() {
    let (_guard, _clock, _ids) = install_deterministic_context(247);
    let sim = SimEventStore::no_faults(247);
    let events = BoxedEventStore::new(sim.clone());
    let table = Arc::new(RwLock::new(TransitionTable::from_ioa_source(DOC_IOA)));
    persist_tombstone_behind_stale_live_snapshot(&events, &table).await;
    sim.restore_faults(SimFaultConfig {
        write_failure_prob: 0.0,
        concurrency_violation_prob: 0.0,
        read_truncation_prob: 1.0,
        snapshot_failure_prob: 0.0,
    });

    let truncated_system = ActorSystem::new("arn238-tombstone-truncated-replay");
    let truncated = truncated_system.spawn(
        EntityActor::with_persistence(
            "Doc",
            "stale-snapshot",
            table.clone(),
            serde_json::json!({}),
            events.clone(),
            BackendLabel::Sim,
        )
        .with_tenant("default"),
        "stale-snapshot-truncated",
    );
    let truncated_result = truncated
        .ask::<EntityResponse>(EntityMsg::GetState, Duration::from_secs(1))
        .await;
    assert!(
        truncated_result.is_err(),
        "recovery must not serve a live prefix when replay stops before a known tombstone"
    );
    drop(truncated);
    drop(truncated_system);

    sim.disable_faults();
    let recovered_system = ActorSystem::new("arn238-tombstone-replay-retry");
    let recovered = recovered_system.spawn(
        EntityActor::with_persistence(
            "Doc",
            "stale-snapshot",
            table,
            serde_json::json!({}),
            events,
            BackendLabel::Sim,
        )
        .with_tenant("default"),
        "stale-snapshot-retry",
    );
    assert_eq!(state(&recovered).await.state.status, "Deleted");
}

/// The first terminal journal event is irreversible even if a legacy writer
/// left a later non-terminal suffix. An already-indexed entity must be removed
/// from memory without treating the final envelope as renewed liveness.
#[tokio::test]
async fn ensure_loaded_rejects_indexed_entity_with_suffix_after_tombstone() {
    let (_guard, _clock, _ids) = install_deterministic_context(301);
    let tenant = TenantId::default();
    let sim = SimEventStore::no_faults(301);
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        tenant.as_str(),
        parse_csdl(CSDL_XML).expect("CSDL parse"),
        CSDL_XML.to_string(),
        &[("Doc", DOC_IOA)],
    );
    let mut server = ServerState::from_registry(
        ActorSystem::new("arn238-terminal-suffix-ensure-loaded"),
        registry,
    );
    server.set_storage_stack(StorageStack::from_sim(sim.clone(), None));
    let created = server
        .get_or_create_tenant_entity(
            &tenant,
            "Doc",
            "terminal-suffix",
            serde_json::json!({"WorkspaceId": "ws", "Path": "/terminal"}),
        )
        .await
        .expect("create indexed entity");
    assert!(server.entity_exists(&tenant, "Doc", "terminal-suffix"));

    let persistence_id = "default:Doc:terminal-suffix";
    let timestamp = sim_now();
    sim.append(
        persistence_id,
        created.state.sequence_nr,
        &[
            PersistenceEnvelope {
                sequence_nr: 0,
                event_type: "Delete".to_string(),
                payload: serde_json::to_value(EntityEvent {
                    action: "Delete".to_string(),
                    from_status: "Ready".to_string(),
                    to_status: "Deleted".to_string(),
                    timestamp,
                    params: serde_json::json!({}),
                    idempotency_key: None,
                })
                .expect("serialize terminal event"),
                metadata: EventMetadata {
                    event_id: sim_uuid(),
                    causation_id: sim_uuid(),
                    correlation_id: sim_uuid(),
                    timestamp,
                    actor_id: persistence_id.to_string(),
                },
            },
            PersistenceEnvelope {
                sequence_nr: 0,
                event_type: "LegacySuffix".to_string(),
                payload: serde_json::to_value(EntityEvent {
                    action: "LegacySuffix".to_string(),
                    from_status: "Deleted".to_string(),
                    to_status: "Ready".to_string(),
                    timestamp,
                    params: serde_json::json!({}),
                    idempotency_key: None,
                })
                .expect("serialize legacy suffix"),
                metadata: EventMetadata {
                    event_id: sim_uuid(),
                    causation_id: sim_uuid(),
                    correlation_id: sim_uuid(),
                    timestamp,
                    actor_id: persistence_id.to_string(),
                },
            },
        ],
    )
    .await
    .expect("seed legacy suffix after terminal event");

    assert!(
        !server
            .ensure_entity_loaded(&tenant, "Doc", "terminal-suffix")
            .await,
        "the first terminal boundary must override a later legacy suffix"
    );
    assert!(!server.entity_exists(&tenant, "Doc", "terminal-suffix"));
}
