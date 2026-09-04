//! Integration test: idle passivation and lazy respawn.

mod common;

use temper_runtime::persistence::EventStore;
use temper_runtime::scheduler::{install_deterministic_context, sim_now};
use temper_runtime::tenant::TenantId;
use temper_store_sim::SimEventStore;

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

/// ARN-462: one passivation tick must not snapshot-and-stop every idle actor.
/// Production traces did 430 / 735 sequential GetState + snapshot writes on the
/// request pool. Remainder stay idle for the next tick. Actors that are
/// processed still get a snapshot (ADR-0048).
#[tokio::test]
async fn passivate_tick_does_not_snapshot_every_idle_actor() {
    let seed = 7;
    let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
    let sim_store = SimEventStore::no_faults(seed);
    let state = common::build_default_state_with_store(sim_store.clone(), "passivation-budget");

    let tenant = TenantId::default();
    const IDLE_COUNT: usize = 48;
    let mut actor_keys = Vec::with_capacity(IDLE_COUNT);
    for index in 0..IDLE_COUNT {
        let entity_id = format!("o-idle-{index:02}");
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
        actor_keys.push(format!("{tenant}:Order:{entity_id}"));
    }

    {
        let mut last_accessed = state.last_accessed.write().unwrap();
        for key in &actor_keys {
            last_accessed.insert(key.clone(), sim_now() - chrono::Duration::seconds(600));
        }
    }

    state.passivate_idle_actors().await;

    let remaining = {
        let registry = state.actor_registry.read().unwrap();
        actor_keys
            .iter()
            .filter(|key| registry.contains_key(*key))
            .count()
    };
    let passivated = IDLE_COUNT - remaining;
    assert!(
        passivated > 0,
        "at least one idle actor should be passivated this tick"
    );
    assert!(
        remaining > 0,
        "one tick must not drain every idle actor (passivated {passivated} of {IDLE_COUNT})"
    );
    assert!(
        passivated <= temper_server::state::PASSIVATE_IDLE_ACTORS_PER_TICK,
        "passivated {passivated} exceeds PASSIVATE_IDLE_ACTORS_PER_TICK"
    );

    let mut snapshotted = 0usize;
    for key in &actor_keys {
        if remaining_registry_has(&state, key) {
            continue;
        }
        let snapshot = sim_store
            .load_snapshot(key)
            .await
            .expect("snapshot lookup should succeed");
        assert!(
            snapshot.is_some(),
            "passivated actor {key} must still receive a snapshot (ADR-0048)"
        );
        snapshotted += 1;
    }
    assert_eq!(snapshotted, passivated);
}

fn remaining_registry_has(state: &temper_server::ServerState, actor_key: &str) -> bool {
    state.actor_registry.read().unwrap().contains_key(actor_key)
}
