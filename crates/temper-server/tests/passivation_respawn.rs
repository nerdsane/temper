//! Integration test: idle passivation and lazy respawn.

mod common;

use temper_runtime::persistence::EventStore;
use temper_runtime::scheduler::{install_deterministic_context, sim_now};
use temper_runtime::tenant::TenantId;
use temper_store_sim::{SimEventStore, SimFaultConfig};

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
    let updated = state
        .update_tenant_entity_fields(
            &tenant,
            "Order",
            &entity_id,
            serde_json::json!({"note": "snapshotted"}),
            false,
        )
        .await
        .expect("field update should succeed");
    assert!(updated.success);

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
    assert_eq!(recovered.state.fields["note"], "snapshotted");
    assert!(recovered.state.total_event_count >= 3); // Created + AddItem + SubmitOrder
}

#[tokio::test]
async fn snapshot_failure_retains_actor_and_acknowledged_field_state() {
    let seed = 213;
    let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
    let sim_store = SimEventStore::new(
        seed,
        SimFaultConfig {
            snapshot_failure_prob: 1.0,
            ..SimFaultConfig::none()
        },
    );
    let state = common::build_default_state_with_store(sim_store.clone(), "passivation-failure");
    let tenant = TenantId::default();
    let entity_id = format!("o-snapshot-failure-{seed}");

    let created = common::dispatch(
        &state,
        &tenant,
        "Order",
        &entity_id,
        "AddItem",
        serde_json::json!({}),
    )
    .await
    .expect("AddItem should succeed");
    assert!(created.success);
    let updated = state
        .update_tenant_entity_fields(
            &tenant,
            "Order",
            &entity_id,
            serde_json::json!({"note": "acknowledged"}),
            false,
        )
        .await
        .expect("field update should succeed");
    assert!(updated.success);

    let actor_key = format!("{tenant}:Order:{entity_id}");
    state.last_accessed.write().unwrap().insert(
        actor_key.clone(),
        sim_now() - chrono::Duration::seconds(600),
    );
    state.passivate_idle_actors().await;

    assert!(
        state
            .actor_registry
            .read()
            .unwrap()
            .contains_key(&actor_key),
        "failed snapshot must retain the only actor holding acknowledged state"
    );
    assert!(
        sim_store
            .load_snapshot(&actor_key)
            .await
            .expect("snapshot lookup")
            .is_none()
    );
    let current = state
        .get_tenant_entity_state(&tenant, "Order", &entity_id)
        .await
        .expect("retained actor state");
    assert_eq!(current.state.fields["note"], "acknowledged");
}
