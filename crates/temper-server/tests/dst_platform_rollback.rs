//! DST rollback/fault injection tests.
//!
//! Verifies that the platform handles store failures gracefully:
//! failed installs leave no partial state, and failed dispatches
//! do not corrupt persisted data.

mod common;

use common::platform_harness::SimPlatformHarness;
use common::platform_invariants::*;
use temper_runtime::scheduler::install_deterministic_context;
use temper_server::platform_store::SimPlatformFaultConfig;
use temper_store_sim::SimFaultConfig;

const NUM_SEEDS: u64 = 50;

// =========================================================================
// Test: Failed install is atomic — no partial registry or Cedar state
// =========================================================================

#[tokio::test]
async fn dst_rollback_install_failure_is_atomic() {
    for seed in 0..NUM_SEEDS {
        let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
        let mut harness = SimPlatformHarness::new(
            seed,
            SimFaultConfig::none(),
            SimPlatformFaultConfig::heavy(),
        );

        // Try installing PM app — some installs will fail due to heavy faults.
        let install_result = harness
            .install_app("rollback-test", "project-management")
            .await;

        match install_result {
            Ok(_) => {
                // Install succeeded — disable faults for clean restart.
                let prev = harness.sim_platform_store.disable_faults();
                harness.restart().await;

                assert_boot_invariants(&harness).await.unwrap_or_else(|e| {
                    panic!("seed {seed}: boot invariants failed after successful install: {e}")
                });
                harness.sim_platform_store.restore_faults(prev);
            }
            Err(_) => {
                // Install failed — disable faults and verify no partial state.
                let prev = harness.sim_platform_store.disable_faults();
                assert_p7_cedar_persistence(&harness)
                    .await
                    .unwrap_or_else(|e| panic!("seed {seed}: P7 failed after failed install: {e}"));
                assert_p2_store_registry_consistency(&harness)
                    .await
                    .unwrap_or_else(|e| panic!("seed {seed}: P2 failed after failed install: {e}"));
                harness.sim_platform_store.restore_faults(prev);
            }
        }
    }
}

// =========================================================================
// Test: Dispatch with event store faults — only persisted state visible
// =========================================================================

#[tokio::test]
async fn dst_rollback_dispatch_with_store_faults() {
    for seed in 0..NUM_SEEDS {
        let (_guard, _clock, _id_gen) = install_deterministic_context(seed);

        // Phase 1: Install PM app without faults so we have a clean baseline.
        let tenant = "rollback-dispatch";

        // Create a harness with event store faults and dispatch actions.
        let mut faulty_harness = SimPlatformHarness::new(
            seed,
            SimFaultConfig::heavy(),
            SimPlatformFaultConfig::none(),
        );

        // Re-install PM on the faulty harness (no platform faults, so this succeeds).
        faulty_harness
            .install_app(tenant, "project-management")
            .await
            .unwrap_or_else(|e| panic!("seed {seed}: install PM on faulty harness failed: {e}"));

        // Dispatch several actions — some will fail due to event store faults.
        let mut success_count = 0;
        for i in 0..5 {
            let eid = format!("issue-fault-{seed}-{i}");

            let result = faulty_harness
                .dispatch(tenant, "Issue", &eid, "MoveToTriage", serde_json::json!({}))
                .await;

            if let Ok(r) = &result
                && r.success
            {
                success_count += 1;
            }
            // Failures are expected — event store faults will cause some to fail.
        }

        // Restart — only successfully persisted state should be visible.
        faulty_harness.restart().await;

        // Boot invariants must hold — registry and Cedar are consistent.
        assert_boot_invariants(&faulty_harness)
            .await
            .unwrap_or_else(|e| {
                panic!(
                    "seed {seed}: boot invariants failed after faulty dispatches \
                     ({success_count} succeeded): {e}"
                )
            });

        // Data invariants must hold — index agrees with store, no orphans.
        assert_data_invariants(&faulty_harness)
            .await
            .unwrap_or_else(|e| {
                panic!(
                    "seed {seed}: data invariants failed after faulty dispatches \
                     ({success_count} succeeded): {e}"
                )
            });
    }
}
