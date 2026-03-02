//! DST actor lifecycle tests.
//!
//! Verifies entity actor lifecycle with simulated persistence:
//! - Create → dispatch → persist → crash → respawn → replay → continue
//! - Full lifecycle through all states with persistence

mod common;

use temper_runtime::scheduler::install_deterministic_context;
use temper_runtime::tenant::TenantId;
use temper_store_sim::SimEventStore;

// =========================================================================
// Test: Full order lifecycle through dispatch_tenant_action
// =========================================================================

#[tokio::test]
async fn dst_full_lifecycle_through_dispatch() {
    for seed in 0..100 {
        let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
        let (state, _sim_store) = common::build_default_state(seed, "dst-lifecycle");
        let tenant = TenantId::default();
        let eid = format!("o-life-{seed}");

        let actions_and_expected = [
            ("AddItem", "Draft"),
            ("SubmitOrder", "Submitted"),
            ("ConfirmOrder", "Confirmed"),
            ("ProcessOrder", "Processing"),
            ("ShipOrder", "Shipped"),
            ("DeliverOrder", "Delivered"),
        ];

        for (action, expected_status) in &actions_and_expected {
            let r = common::dispatch(
                &state,
                &tenant,
                "Order",
                &eid,
                action,
                serde_json::json!({}),
            )
            .await
            .unwrap_or_else(|e| panic!("seed {seed}: {action} failed: {e}"));
            assert!(
                r.success,
                "seed {seed}: {action} not successful: {:?}",
                r.error
            );
            assert_eq!(
                &r.state.status, expected_status,
                "seed {seed}: after {action}"
            );
        }
    }
}

// =========================================================================
// Test: Crash and recovery through ServerState dispatch
// =========================================================================

#[tokio::test]
async fn dst_lifecycle_crash_and_recovery() {
    for seed in 0..50 {
        let shared_store = SimEventStore::no_faults(seed);
        let tenant = TenantId::default();
        let eid = format!("o-crash-{seed}");

        // Phase 1: Create and advance to Confirmed.
        {
            let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
            let state = common::build_default_state_with_store(shared_store.clone(), "dst-crash-1");

            for action in &["AddItem", "SubmitOrder", "ConfirmOrder"] {
                let r = common::dispatch(
                    &state,
                    &tenant,
                    "Order",
                    &eid,
                    action,
                    serde_json::json!({}),
                )
                .await
                .unwrap_or_else(|e| panic!("seed {seed}: {action} failed: {e}"));
                assert!(r.success, "seed {seed}: {action} failed: {:?}", r.error);
            }
        }
        // ServerState dropped — simulates crash.

        // Phase 2: Respawn with same SimEventStore.
        {
            let (_guard2, _clock2, _id_gen2) = install_deterministic_context(seed + 10000);
            let state = common::build_default_state_with_store(shared_store.clone(), "dst-crash-2");

            // Continue from Confirmed → Processing.
            let r = common::dispatch(
                &state,
                &tenant,
                "Order",
                &eid,
                "ProcessOrder",
                serde_json::json!({}),
            )
            .await
            .unwrap_or_else(|e| panic!("seed {seed}: ProcessOrder post-recovery failed: {e}"));
            assert!(
                r.success,
                "seed {seed}: ProcessOrder after recovery failed: {:?}",
                r.error
            );
            assert_eq!(r.state.status, "Processing");
        }
    }
}

// =========================================================================
// Test: Persistence stores correct number of events
// =========================================================================

#[tokio::test]
async fn dst_lifecycle_event_count() {
    let (_guard, _clock, _id_gen) = install_deterministic_context(42);
    let (state, sim_store) = common::build_default_state(42, "dst-lifecycle");
    let tenant = TenantId::default();

    for action in &["AddItem", "SubmitOrder", "ConfirmOrder"] {
        common::dispatch(
            &state,
            &tenant,
            "Order",
            "o-count",
            action,
            serde_json::json!({}),
        )
        .await
        .unwrap();
    }

    // The SimEventStore should have events for this entity.
    // persistence_id format: "tenant:EntityType:EntityId"
    let events = sim_store.dump_journal("default:Order:o-count");
    // At minimum: Created (bootstrap) + AddItem + SubmitOrder + ConfirmOrder = 4
    assert!(
        events.len() >= 4,
        "Expected at least 4 events, got {}",
        events.len()
    );
}
