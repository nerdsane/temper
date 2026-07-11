use super::*;
use temper_runtime::ActorSystem;
use temper_runtime::persistence::{EventMetadata, EventStore, PersistenceEnvelope};
use temper_runtime::scheduler::{sim_now, sim_uuid};
use temper_store_sim::SimEventStore;

use crate::registry::SpecRegistry;

fn event(event_type: &str, payload: serde_json::Value) -> PersistenceEnvelope {
    PersistenceEnvelope {
        sequence_nr: 0,
        event_type: event_type.to_string(),
        payload,
        metadata: EventMetadata {
            event_id: sim_uuid(),
            causation_id: sim_uuid(),
            correlation_id: sim_uuid(),
            timestamp: sim_now(),
            actor_id: "entity-enumeration-test".to_string(),
        },
    }
}

#[tokio::test]
async fn classifies_tombstones_and_resurrection_from_only_the_tail() {
    let store = SimEventStore::no_faults(192);
    store
        .append(
            "tenant:Order:deleted",
            0,
            &[
                event("Created", serde_json::json!({})),
                event("Deleted", serde_json::json!({})),
            ],
        )
        .await
        .unwrap();
    store
        .append(
            "tenant:Order:payload-deleted",
            0,
            &[event(
                "EntityEvent",
                serde_json::json!({ "action": "Deleted" }),
            )],
        )
        .await
        .unwrap();
    store
        .append(
            "tenant:Order:resurrected",
            0,
            &[
                event("Deleted", serde_json::json!({})),
                event("Created", serde_json::json!({})),
            ],
        )
        .await
        .unwrap();

    let candidates = vec![
        ("Order".to_string(), "deleted".to_string()),
        ("Order".to_string(), "payload-deleted".to_string()),
        ("Order".to_string(), "resurrected".to_string()),
    ];
    let live = live_entity_candidates(
        &BoxedEventStore::new(store),
        &TenantId::new("tenant"),
        &candidates,
    )
    .await
    .unwrap();

    assert_eq!(live, vec![("Order".to_string(), "resurrected".to_string())]);
}

#[tokio::test]
async fn missing_journal_candidate_fails_closed() {
    let result = live_entity_candidates(
        &BoxedEventStore::new(SimEventStore::no_faults(194)),
        &TenantId::new("tenant"),
        &[("Order".to_string(), "missing".to_string())],
    )
    .await;

    assert!(matches!(result, Err(PersistenceError::Storage(_))));
}

#[tokio::test]
async fn latest_event_failure_rejects_the_complete_candidate_set() {
    let store = SimEventStore::no_faults(193);
    store
        .append(
            "tenant:Order:uncertain",
            0,
            &[event("Created", serde_json::json!({}))],
        )
        .await
        .unwrap();
    store.fail_next_reads("tenant:Order:uncertain", 1);

    let result = live_entity_candidates(
        &BoxedEventStore::new(store),
        &TenantId::new("tenant"),
        &[("Order".to_string(), "uncertain".to_string())],
    )
    .await;

    assert!(result.is_err());
}

#[test]
fn concurrent_index_mutation_invalidates_scan_publication() {
    let state = ServerState::from_registry(
        ActorSystem::new("entity-enumeration-epoch"),
        SpecRegistry::new(),
    );
    let tenant = TenantId::new("tenant");
    let epoch_key = ServerState::type_entity_index_epoch_key(&tenant, "Order");
    let captured = state.capture_entity_index_epoch(&epoch_key).unwrap();
    state
        .mutate_entity_index(&tenant, "Order", |index| {
            index
                .entry("tenant:Order".to_string())
                .or_default()
                .insert("concurrent".to_string());
        })
        .unwrap();

    let published = state
        .publish_entity_scan(
            &epoch_key,
            captured,
            std::slice::from_ref(&epoch_key),
            |index, hydrated| {
                index.clear();
                hydrated.insert("tenant:Order".to_string());
            },
        )
        .unwrap();

    assert!(published.is_none());
    assert_eq!(
        state.list_entity_ids(&TenantId::new("tenant"), "Order"),
        vec!["concurrent".to_string()]
    );
    assert!(state.entity_index_hydrated.read().unwrap().is_empty());
}

#[test]
fn unrelated_tenant_mutation_does_not_invalidate_typed_scan() {
    let state = ServerState::from_registry(
        ActorSystem::new("entity-enumeration-scope"),
        SpecRegistry::new(),
    );
    let tenant = TenantId::new("tenant");
    let other = TenantId::new("other");
    let epoch_key = ServerState::type_entity_index_epoch_key(&tenant, "Order");
    let captured = state.capture_entity_index_epoch(&epoch_key).unwrap();
    state
        .mutate_entity_index(&other, "Order", |index| {
            index
                .entry("other:Order".to_string())
                .or_default()
                .insert("unrelated".to_string());
        })
        .unwrap();

    let published = state
        .publish_entity_scan(
            &epoch_key,
            captured,
            std::slice::from_ref(&epoch_key),
            |index, _| {
                index
                    .entry("tenant:Order".to_string())
                    .or_default()
                    .insert("scanned".to_string());
            },
        )
        .unwrap();

    assert!(published.is_some());
    assert_eq!(state.list_entity_ids(&tenant, "Order"), vec!["scanned"]);
}
