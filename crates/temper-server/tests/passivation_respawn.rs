//! Integration test: idle passivation and lazy respawn.

mod common;

use temper_runtime::persistence::{EventMetadata, EventStore, PersistenceEnvelope};
use temper_runtime::scheduler::{install_deterministic_context, sim_now, sim_uuid};
use temper_runtime::tenant::TenantId;
use temper_store_sim::SimEventStore;

#[tokio::test]
async fn passivation_does_not_overwrite_a_same_sequence_snapshot_rewrite() {
    let seed = 281;
    let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
    let sim_store = SimEventStore::no_faults(seed);
    let state = common::build_default_state_with_store(sim_store.clone(), "passivation-cas");
    let tenant = TenantId::default();
    let entity_id = "same-sequence-passivation";
    let actor_key = format!("{tenant}:Order:{entity_id}");
    let legacy_snapshot = |marker: &str| {
        serde_json::to_vec(&serde_json::json!({
            "entity_type": "Order",
            "entity_id": entity_id,
            "status": "Draft",
            "item_count": 0,
            "fields": {
                "Id": entity_id,
                "Status": "Draft",
                "Marker": marker
            }
        }))
        .expect("serialize legacy snapshot")
    };
    let captured_snapshot = legacy_snapshot("captured");
    sim_store
        .save_snapshot(&actor_key, 1, &captured_snapshot)
        .await
        .expect("seed captured snapshot");
    let timestamp = sim_now();
    sim_store
        .append(
            &actor_key,
            0,
            &[PersistenceEnvelope {
                sequence_nr: 1,
                event_type: "Temper.Internal.FieldUpdate.v1".to_string(),
                payload: serde_json::json!({
                    "schema": "temper.field-update.v1",
                    "fields": {"JournalMarker": "journal"},
                    "replace": false,
                    "idempotency_key": "passivation-cas-seed"
                }),
                metadata: EventMetadata {
                    event_id: sim_uuid(),
                    causation_id: sim_uuid(),
                    correlation_id: sim_uuid(),
                    timestamp,
                    actor_id: actor_key.clone(),
                },
            }],
        )
        .await
        .expect("seed equal-sequence journal");

    let hydrated = state
        .get_tenant_entity_state(&tenant, "Order", entity_id)
        .await
        .expect("hydrate captured snapshot");
    assert_eq!(hydrated.state.fields["Marker"], "captured");

    let replacement_snapshot = legacy_snapshot("replacement");
    sim_store
        .save_snapshot(&actor_key, 1, &replacement_snapshot)
        .await
        .expect("install concurrent same-sequence replacement");
    state
        .last_accessed
        .write()
        .expect("last-accessed lock")
        .insert(
            actor_key.clone(),
            sim_now() - chrono::Duration::seconds(600),
        );

    state.passivate_idle_actors().await;

    assert_eq!(
        sim_store
            .load_snapshot(&actor_key)
            .await
            .expect("load snapshot after passivation"),
        Some((1, replacement_snapshot)),
        "passivation must not replace a newer same-sequence snapshot source with stale actor state"
    );
}

#[tokio::test]
async fn passivated_snapshot_only_actor_does_not_claim_journal_provenance() {
    let seed = 278;
    let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
    let sim_store = SimEventStore::no_faults(seed);
    let state = common::build_default_state_with_store(sim_store.clone(), "passivation-source");
    let tenant = TenantId::default();
    let entity_id = "snapshot-only-passivation";
    let actor_key = format!("{tenant}:Order:{entity_id}");
    let snapshot = serde_json::json!({
        "entity_type": "Order",
        "entity_id": entity_id,
        "status": "Draft",
        "item_count": 0,
        "fields": {
            "Id": entity_id,
            "Status": "Draft",
            "Marker": "snapshot-generation"
        }
    });
    sim_store
        .save_snapshot(
            &actor_key,
            1,
            &serde_json::to_vec(&snapshot).expect("serialize snapshot-only generation"),
        )
        .await
        .expect("seed snapshot-only generation");

    let hydrated = state
        .get_tenant_entity_state(&tenant, "Order", entity_id)
        .await
        .expect("hydrate snapshot-only actor");
    assert_eq!(hydrated.state.fields["Marker"], "snapshot-generation");
    {
        let mut last_accessed = state.last_accessed.write().expect("last-accessed lock");
        last_accessed.insert(
            actor_key.clone(),
            sim_now() - chrono::Duration::seconds(600),
        );
    }
    state.passivate_idle_actors().await;
    assert!(
        !state
            .actor_registry
            .read()
            .expect("actor registry lock")
            .contains_key(&actor_key),
        "fixture must passivate the snapshot-only actor"
    );

    let timestamp = sim_now();
    sim_store
        .append(
            &actor_key,
            0,
            &[PersistenceEnvelope {
                sequence_nr: 1,
                event_type: "Temper.Internal.FieldUpdate.v1".to_string(),
                payload: serde_json::json!({
                    "schema": "temper.field-update.v1",
                    "fields": {"Marker": "journal-generation"},
                    "replace": false,
                    "idempotency_key": "passivation-source-replacement"
                }),
                metadata: EventMetadata {
                    event_id: sim_uuid(),
                    causation_id: sim_uuid(),
                    correlation_id: sim_uuid(),
                    timestamp,
                    actor_id: actor_key.clone(),
                },
            }],
        )
        .await
        .expect("replace snapshot-only source with first journal generation");

    let recovered = state
        .get_tenant_entity_state(&tenant, "Order", entity_id)
        .await
        .expect("respawn after equal-sequence journal replacement");
    assert_eq!(
        recovered.state.fields["Marker"], "journal-generation",
        "passivation must not forge journal provenance for snapshot-only state"
    );
}

#[tokio::test]
async fn passivated_actor_respawns_with_correct_state() {
    let seed = 42;
    let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
    let sim_store = SimEventStore::no_faults(seed);
    let state = common::build_default_state_with_store(sim_store.clone(), "passivation-test");

    let tenant = TenantId::default();
    let entity_id = format!("o-passive-{seed}");

    let r = common::dispatch(
        &state,
        &tenant,
        "Order",
        &entity_id,
        "AddItem",
        serde_json::json!({}),
    )
    .await
    .expect("AddItem should succeed");
    assert!(r.success);

    let r = common::dispatch(
        &state,
        &tenant,
        "Order",
        &entity_id,
        "SubmitOrder",
        serde_json::json!({}),
    )
    .await
    .expect("SubmitOrder should succeed");
    assert!(r.success);
    assert_eq!(r.state.status, "Submitted");

    let actor_key = format!("{tenant}:Order:{entity_id}");
    assert!(
        state
            .actor_registry
            .read()
            .unwrap()
            .contains_key(&actor_key)
    );

    // Force this actor to appear idle beyond the default timeout (300s).
    {
        let mut last_accessed = state.last_accessed.write().unwrap();
        last_accessed.insert(
            actor_key.clone(),
            sim_now() - chrono::Duration::seconds(600),
        );
    }

    state.passivate_idle_actors().await;

    assert!(
        !state
            .actor_registry
            .read()
            .unwrap()
            .contains_key(&actor_key),
        "actor should be removed from registry after passivation"
    );

    let snapshot = sim_store
        .load_snapshot(&actor_key)
        .await
        .expect("snapshot lookup should succeed");
    assert!(snapshot.is_some(), "passivation should persist a snapshot");

    let recovered = state
        .get_tenant_entity_state(&tenant, "Order", &entity_id)
        .await
        .expect("lazy respawn should rebuild actor state");

    assert_eq!(recovered.state.status, "Submitted");
    assert_eq!(recovered.state.item_count, 1);
    assert!(recovered.state.total_event_count >= 3); // Created + AddItem + SubmitOrder
}
