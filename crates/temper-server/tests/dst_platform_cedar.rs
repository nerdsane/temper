//! DST Cedar policy lifecycle tests.
//!
//! Verifies that Cedar policies installed by OS apps survive restarts,
//! are isolated across tenants, and remain coherent with specs under
//! fault injection.

mod common;

use common::platform_harness::SimPlatformHarness;
use common::platform_invariants::*;
use temper_runtime::scheduler::install_deterministic_context;

const NUM_SEEDS: u64 = 50;

// =========================================================================
// Test: Cedar policies survive restart
// =========================================================================

#[tokio::test]
async fn dst_cedar_survives_restart() {
    for seed in 0..NUM_SEEDS {
        let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
        let mut harness = SimPlatformHarness::no_faults(seed);

        // Install PM app — it has Cedar policies.
        harness
            .install_os_app("cedar-test", "project-management")
            .await
            .unwrap_or_else(|e| panic!("seed {seed}: install PM failed: {e}"));

        // Restart — drops in-memory state, rebuilds from stores.
        harness.restart().await;

        // Cedar policies must survive the restart.
        assert_p6_cedar_spec_coherence(&harness)
            .await
            .unwrap_or_else(|e| panic!("seed {seed}: P6 failed after restart: {e}"));
        assert_p7_cedar_persistence(&harness)
            .await
            .unwrap_or_else(|e| panic!("seed {seed}: P7 failed after restart: {e}"));
    }
}

// =========================================================================
// Test: Cedar policies are isolated across tenants
// =========================================================================

#[tokio::test]
async fn dst_cedar_multi_tenant_isolation() {
    for seed in 0..NUM_SEEDS {
        let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
        let mut harness = SimPlatformHarness::no_faults(seed);

        // Install PM for tenant-a.
        harness
            .install_os_app("tenant-a", "project-management")
            .await
            .unwrap_or_else(|e| panic!("seed {seed}: install PM for tenant-a failed: {e}"));

        // Install temper-fs for tenant-b.
        harness
            .install_os_app("tenant-b", "temper-fs")
            .await
            .unwrap_or_else(|e| panic!("seed {seed}: install temper-fs for tenant-b failed: {e}"));

        // Both tenants should have coherent Cedar state.
        assert_p6_cedar_spec_coherence(&harness)
            .await
            .unwrap_or_else(|e| panic!("seed {seed}: P6 failed before restart: {e}"));
        assert_p7_cedar_persistence(&harness)
            .await
            .unwrap_or_else(|e| panic!("seed {seed}: P7 failed before restart: {e}"));

        // Restart — policies must survive independently.
        harness.restart().await;

        assert_p6_cedar_spec_coherence(&harness)
            .await
            .unwrap_or_else(|e| panic!("seed {seed}: P6 failed after restart: {e}"));
        assert_p7_cedar_persistence(&harness)
            .await
            .unwrap_or_else(|e| panic!("seed {seed}: P7 failed after restart: {e}"));
    }
}

// =========================================================================
// Test: Cedar under platform faults
// =========================================================================

#[tokio::test]
async fn dst_cedar_with_platform_faults() {
    use temper_server::platform_store::SimPlatformFaultConfig;

    for seed in 0..NUM_SEEDS {
        let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
        let mut harness = SimPlatformHarness::new(
            seed,
            temper_store_sim::SimFaultConfig::none(),
            SimPlatformFaultConfig::heavy(),
        );

        // Try to install PM — may fail due to policy write failures.
        let install_result = harness
            .install_os_app("cedar-fault", "project-management")
            .await;

        match install_result {
            Ok(_) => {
                // Install succeeded — disable faults for clean restart.
                let prev = harness.sim_platform_store.disable_faults();
                harness.restart().await;

                assert_p6_cedar_spec_coherence(&harness)
                    .await
                    .unwrap_or_else(|e| panic!("seed {seed}: P6 failed after restart: {e}"));
                assert_p7_cedar_persistence(&harness)
                    .await
                    .unwrap_or_else(|e| panic!("seed {seed}: P7 failed after restart: {e}"));
                harness.sim_platform_store.restore_faults(prev);
            }
            Err(_) => {
                // Install failed — disable faults and verify no partial state.
                let prev = harness.sim_platform_store.disable_faults();
                harness.restart().await;
                assert_p1_registry_store_consistency(&harness)
                    .await
                    .unwrap_or_else(|e| panic!("seed {seed}: P1 failed after failed install: {e}"));
                assert_p2_store_registry_consistency(&harness)
                    .await
                    .unwrap_or_else(|e| panic!("seed {seed}: P2 failed after failed install: {e}"));
                assert_p7_cedar_persistence(&harness)
                    .await
                    .unwrap_or_else(|e| panic!("seed {seed}: P7 failed after failed install: {e}"));
                harness.sim_platform_store.restore_faults(prev);
            }
        }
    }
}
