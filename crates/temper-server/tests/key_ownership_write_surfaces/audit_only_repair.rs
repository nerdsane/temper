use temper_runtime::persistence::{
    COMPOSITE_EVENT_TYPE, CompositeEvent, EntityKeyRow, EventMetadata, KeyIndexBackfillFence,
    PersistenceEnvelope, STATE_MATERIALIZATION_EVENT_TYPE, STATE_MATERIALIZATION_SCHEMA,
};
use temper_runtime::scheduler::sim_now;

use super::*;

fn audit_envelope(persistence_id: &str, entity_id: &str) -> PersistenceEnvelope {
    PersistenceEnvelope {
        sequence_nr: 0,
        event_type: COMPOSITE_EVENT_TYPE.to_string(),
        payload: serde_json::to_value(CompositeEvent {
            tenant: "default".to_string(),
            parent_entity_type: "Doc".to_string(),
            parent_entity_id: entity_id.to_string(),
            parent_action: "Audit".to_string(),
            composite_idempotency_key: format!("audit-{entity_id}"),
            intent_hash: String::new(),
            sub_writes: Vec::new(),
        })
        .expect("serialize composite audit"),
        metadata: EventMetadata {
            event_id: sim_uuid(),
            causation_id: sim_uuid(),
            correlation_id: sim_uuid(),
            timestamp: sim_now(),
            actor_id: persistence_id.to_string(),
        },
    }
}

fn materialization_envelope(
    persistence_id: &str,
    entity_id: &str,
    workspace_id: &str,
    path: &str,
) -> PersistenceEnvelope {
    PersistenceEnvelope {
        sequence_nr: 0,
        event_type: STATE_MATERIALIZATION_EVENT_TYPE.to_string(),
        payload: serde_json::json!({
            "schema": STATE_MATERIALIZATION_SCHEMA,
            "state": {
                "entity_type": "Doc",
                "entity_id": entity_id,
                "status": "Ready",
                "item_count": 0,
                "counters": {},
                "booleans": {},
                "lists": {},
                "fields": {
                    "Id": entity_id,
                    "Status": "Ready",
                    "WorkspaceId": workspace_id,
                    "Path": path,
                },
                "events": [],
                "total_event_count": 0,
                "events_since_snapshot": 0,
                "last_snapshot_sequence_nr": 0,
                "sequence_nr": 0,
                "processed_idempotency_keys": {},
            }
        }),
        metadata: EventMetadata {
            event_id: sim_uuid(),
            causation_id: sim_uuid(),
            correlation_id: sim_uuid(),
            timestamp: sim_now(),
            actor_id: persistence_id.to_string(),
        },
    }
}

async fn seed_stale_key(
    events: &BoxedEventStore,
    entity_id: &str,
    sequence_nr: u64,
    contract_revision: u64,
    key_hash: &str,
) {
    events
        .backfill_entity_keys(
            "default",
            "Doc",
            entity_id,
            sequence_nr,
            KeyIndexBackfillFence {
                key_set_signature: "v3:stale-contract",
                contract_revision,
                expected_journal_sequence: sequence_nr,
                expected_entity_live: true,
                expected_snapshot: None,
            },
            &[EntityKeyRow {
                key_name: "path".to_string(),
                key_hash: key_hash.to_string(),
            }],
        )
        .await
        .expect("seed stale key row");
}

#[tokio::test]
async fn key_repair_purges_audit_only_phantom_and_preserves_materialized_owner() {
    let (_guard, _clock, _ids) = install_deterministic_context(24_257);
    let tenant = TenantId::default();
    let sim = SimEventStore::no_faults(24_257);
    let events = BoxedEventStore::new(sim.clone());
    let csdl = parse_csdl(CSDL_XML).expect("CSDL parse");
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        tenant.as_str(),
        csdl,
        CSDL_XML.to_string(),
        &[("Doc", DATA_ONLY_DOC_IOA)],
    );
    let mut server =
        ServerState::from_registry(ActorSystem::new("arn238-audit-only-key-repair"), registry);
    server.set_storage_stack(StorageStack::from_sim(sim.clone(), None));

    let phantom_id = "audit-only-phantom";
    let phantom_pid = format!("default:Doc:{phantom_id}");
    sim.append(&phantom_pid, 0, &[audit_envelope(&phantom_pid, phantom_id)])
        .await
        .expect("seed audit-only phantom");

    let owner_id = "materialized-owner";
    let owner_pid = format!("default:Doc:{owner_id}");
    sim.append(
        &owner_pid,
        0,
        &[
            materialization_envelope(&owner_pid, owner_id, "ws-live", "/owned"),
            audit_envelope(&owner_pid, owner_id),
        ],
    )
    .await
    .expect("seed materialized owner with audit tail");

    let stale_revision = events
        .begin_key_index_backfill("default", "Doc", "v3:stale-contract")
        .await
        .expect("begin stale key contract");
    let phantom_stale_hash = doc_key_hash("ws-stale", "/phantom");
    let owner_stale_hash = doc_key_hash("ws-stale", "/owner");
    seed_stale_key(&events, phantom_id, 1, stale_revision, &phantom_stale_hash).await;
    seed_stale_key(&events, owner_id, 2, stale_revision, &owner_stale_hash).await;

    server.populate_key_index_from_snapshots(&tenant).await;

    assert_eq!(
        events
            .lookup_by_key("default", "Doc", "path", &phantom_stale_hash)
            .await
            .expect("lookup phantom stale key"),
        None,
        "recognized audit-only phantoms must be source-fenced to zero key rows"
    );
    assert_eq!(
        events
            .lookup_by_key("default", "Doc", "path", &owner_stale_hash)
            .await
            .expect("lookup materialized stale key"),
        None,
        "materialized owners must replace stale claims with current fields"
    );
    assert_eq!(
        events
            .lookup_by_key("default", "Doc", "path", &doc_key_hash("ws-live", "/owned"),)
            .await
            .expect("lookup materialized current key"),
        Some(owner_id.to_string()),
        "valid state materialization must remain a live declared-key owner"
    );
    let current_signature =
        declared_key_set_signature(&TransitionTable::from_ioa_source(DATA_ONLY_DOC_IOA).keys);
    assert_eq!(
        events
            .key_index_backfilled_types("default")
            .await
            .expect("load repaired coverage"),
        vec![("Doc".to_string(), current_signature)],
        "coverage must publish after the phantom is purged and the owner is repaired"
    );
}
