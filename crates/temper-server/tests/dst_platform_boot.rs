//! Boot-cycle DST test suite.
//!
//! Tests the full platform lifecycle: install OS app -> create entities ->
//! dispatch actions -> restart -> verify invariants. Uses the
//! `SimPlatformHarness` with production code paths and simulated storage.
//!
//! FoundationDB pattern: same code, simulated I/O, multi-seed coverage.

mod common;

use temper_runtime::scheduler::install_deterministic_context;
use temper_server::platform_store::SimPlatformFaultConfig;
use temper_store_sim::SimFaultConfig;

use common::platform_harness::SimPlatformHarness;
use common::platform_invariants::*;

const NUM_SEEDS: u64 = 50;
const TENANT: &str = "test-tenant";

// =========================================================================
// Test 1: Full lifecycle — install, dispatch, restart, verify invariants
// =========================================================================

#[tokio::test]
async fn dst_boot_cycle_full_lifecycle() {
    for seed in 0..NUM_SEEDS {
        let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
        let mut harness = SimPlatformHarness::no_faults(seed);

        // Install the project-management OS app.
        let entity_types = harness
            .install_os_app(TENANT, "project-management")
            .await
            .unwrap_or_else(|e| panic!("seed {seed}: install_os_app failed: {e}"));
        assert!(
            !entity_types.is_empty(),
            "seed {seed}: no entity types installed"
        );

        // Create an Issue entity by dispatching an action.
        // Initial state is Backlog; SetDescription is valid from Backlog (self-loop).
        let entity_id = format!("issue-{seed}");
        let r = harness
            .dispatch(
                TENANT,
                "Issue",
                &entity_id,
                "SetDescription",
                serde_json::json!({"description": "Boot cycle test issue"}),
            )
            .await;
        assert!(
            r.as_ref().map(|r| r.success).unwrap_or(false),
            "seed {seed}: SetDescription failed: {r:?}"
        );

        // Restart — drops in-memory state, rebuilds from durable stores.
        harness.restart().await;

        // Boot invariants: registry-store consistency, Cedar, installed apps.
        assert_boot_invariants(&harness)
            .await
            .unwrap_or_else(|e| panic!("seed {seed}: boot invariants failed: {e}"));

        // Bootstrap idempotence: no duplicate specs.
        assert_p12_bootstrap_idempotence(&harness, TENANT)
            .await
            .unwrap_or_else(|e| panic!("seed {seed}: P12 failed: {e}"));

        // Data invariants: index-store agreement, tombstone, isolation, initial state.
        assert_data_invariants(&harness)
            .await
            .unwrap_or_else(|e| panic!("seed {seed}: data invariants failed: {e}"));
    }
}

// =========================================================================
// Test 2: Boot cycle with event-store faults
// =========================================================================

#[tokio::test]
async fn dst_boot_cycle_with_store_faults() {
    for seed in 0..NUM_SEEDS {
        let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
        let mut harness = SimPlatformHarness::new(
            seed,
            SimFaultConfig::heavy(),
            SimPlatformFaultConfig::none(),
        );

        // Install PM app — no platform faults, so this should succeed.
        let install_result = harness.install_os_app(TENANT, "project-management").await;
        if install_result.is_err() {
            // Failed install should leave no orphaned state.
            let prev_event = harness.sim_event_store.disable_faults();
            harness.restart().await;
            assert_boot_invariants(&harness).await.unwrap_or_else(|e| {
                panic!("seed {seed}: boot invariants failed after failed install: {e}")
            });
            assert_data_invariants(&harness).await.unwrap_or_else(|e| {
                panic!("seed {seed}: data invariants failed after failed install: {e}")
            });
            harness.sim_event_store.restore_faults(prev_event);
            continue; // skip dispatch phase — no app installed
        }

        // Attempt to create an Issue. Event-store faults may cause failure.
        let entity_id = format!("issue-fault-{seed}");
        let _r = harness
            .dispatch(
                TENANT,
                "Issue",
                &entity_id,
                "SetDescription",
                serde_json::json!({"description": "Fault test"}),
            )
            .await;
        // Dispatch may fail due to injected write faults — that's expected.

        // Restart — only successfully persisted state should be visible.
        harness.restart().await;

        // After restart, invariants must hold for whatever was persisted.
        assert_boot_invariants(&harness).await.unwrap_or_else(|e| {
            panic!("seed {seed}: boot invariants failed after store faults: {e}")
        });

        assert_data_invariants(&harness).await.unwrap_or_else(|e| {
            panic!("seed {seed}: data invariants failed after store faults: {e}")
        });
    }
}

// =========================================================================
// Test 3: Boot cycle with platform-store faults
// =========================================================================

#[tokio::test]
async fn dst_boot_cycle_with_platform_faults() {
    for seed in 0..NUM_SEEDS {
        let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
        let mut harness = SimPlatformHarness::new(
            seed,
            SimFaultConfig::none(),
            SimPlatformFaultConfig::heavy(),
        );

        // OS app install may fail due to spec/policy write faults.
        let install_result = harness.install_os_app(TENANT, "project-management").await;

        if install_result.is_err() {
            // Install failed due to platform faults — disable faults for clean restart.
            let prev = harness.sim_platform_store.disable_faults();
            harness.restart().await;

            assert_boot_invariants(&harness).await.unwrap_or_else(|e| {
                panic!("seed {seed}: boot invariants failed after failed install: {e}")
            });
            assert_data_invariants(&harness).await.unwrap_or_else(|e| {
                panic!("seed {seed}: data invariants failed after failed install: {e}")
            });
            harness.sim_platform_store.restore_faults(prev);
            continue;
        }

        // Install succeeded despite faults. Dispatch an action.
        let entity_id = format!("issue-pfault-{seed}");
        let _r = harness
            .dispatch(
                TENANT,
                "Issue",
                &entity_id,
                "SetDescription",
                serde_json::json!({"description": "Platform fault test"}),
            )
            .await;

        // Disable faults for clean restart and invariant checks.
        let prev = harness.sim_platform_store.disable_faults();
        harness.restart().await;

        assert_boot_invariants(&harness).await.unwrap_or_else(|e| {
            panic!("seed {seed}: boot invariants failed after platform faults: {e}")
        });

        assert_p12_bootstrap_idempotence(&harness, TENANT)
            .await
            .unwrap_or_else(|e| panic!("seed {seed}: P12 failed after platform faults: {e}"));
        harness.sim_platform_store.restore_faults(prev);
    }
}

// =========================================================================
// Test 4: Bootstrap idempotence — install twice, no duplicates
// =========================================================================

#[tokio::test]
async fn dst_boot_cycle_idempotent() {
    for seed in 0..NUM_SEEDS {
        let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
        let mut harness = SimPlatformHarness::no_faults(seed);

        // First install.
        let types_1 = harness
            .install_os_app(TENANT, "project-management")
            .await
            .unwrap_or_else(|e| panic!("seed {seed}: first install failed: {e}"));

        // Restart.
        harness.restart().await;

        // Second install of the same app — should be idempotent.
        let types_2 = harness
            .install_os_app(TENANT, "project-management")
            .await
            .unwrap_or_else(|e| panic!("seed {seed}: second install failed: {e}"));

        // Both installs should return the same entity types.
        assert_eq!(
            types_1, types_2,
            "seed {seed}: entity types differ across idempotent installs"
        );

        // P12: no duplicate specs in the store.
        assert_p12_bootstrap_idempotence(&harness, TENANT)
            .await
            .unwrap_or_else(|e| panic!("seed {seed}: P12 idempotence failed: {e}"));

        // Boot invariants still hold.
        assert_boot_invariants(&harness)
            .await
            .unwrap_or_else(|e| panic!("seed {seed}: boot invariants failed: {e}"));
    }
}

// =========================================================================
// Test 5: Multi-tenant — independent apps survive restart
// =========================================================================

#[tokio::test]
async fn dst_boot_cycle_multi_tenant() {
    for seed in 0..NUM_SEEDS {
        let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
        let mut harness = SimPlatformHarness::no_faults(seed);

        let tenant_a = "tenant-a";
        let tenant_b = "tenant-b";

        // Install PM for tenant-a.
        let types_a = harness
            .install_os_app(tenant_a, "project-management")
            .await
            .unwrap_or_else(|e| panic!("seed {seed}: install PM for tenant-a failed: {e}"));
        assert!(
            !types_a.is_empty(),
            "seed {seed}: no entity types for tenant-a"
        );

        // Install temper-fs for tenant-b.
        let types_b = harness
            .install_os_app(tenant_b, "temper-fs")
            .await
            .unwrap_or_else(|e| panic!("seed {seed}: install temper-fs for tenant-b failed: {e}"));
        assert!(
            !types_b.is_empty(),
            "seed {seed}: no entity types for tenant-b"
        );

        // Dispatch an action on each tenant to create entities.
        let _r_a = harness
            .dispatch(
                tenant_a,
                "Issue",
                &format!("issue-a-{seed}"),
                "SetDescription",
                serde_json::json!({"description": "Tenant A issue"}),
            )
            .await;

        // For temper-fs, find a valid entity type and action.
        // Use the first entity type returned from install.
        let fs_entity_type = &types_b[0];
        let _r_b = harness
            .dispatch(
                tenant_b,
                fs_entity_type,
                &format!("fs-b-{seed}"),
                "SetDescription",
                serde_json::json!({"description": "Tenant B entity"}),
            )
            .await;
        // Dispatch on tenant-b may fail if the entity type doesn't support
        // SetDescription — that's fine, we care about tenant isolation.

        // Restart and verify both tenants restored independently.
        harness.restart().await;

        assert_boot_invariants(&harness)
            .await
            .unwrap_or_else(|e| panic!("seed {seed}: boot invariants failed: {e}"));

        // P14: tenant isolation — no cross-tenant entity leakage.
        assert_p14_tenant_isolation(&harness)
            .await
            .unwrap_or_else(|e| panic!("seed {seed}: P14 tenant isolation failed: {e}"));

        // P12: no duplicate specs per tenant.
        assert_p12_bootstrap_idempotence(&harness, tenant_a)
            .await
            .unwrap_or_else(|e| panic!("seed {seed}: P12 failed for tenant-a: {e}"));
        assert_p12_bootstrap_idempotence(&harness, tenant_b)
            .await
            .unwrap_or_else(|e| panic!("seed {seed}: P12 failed for tenant-b: {e}"));
    }
}

// =========================================================================
// Test 6: Determinism canary — same seed twice yields identical state
// =========================================================================

#[tokio::test]
async fn dst_boot_cycle_determinism_canary() {
    for seed in 0..10 {
        let mut results = Vec::new();

        for _run in 0..2 {
            let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
            let mut harness = SimPlatformHarness::no_faults(seed);

            harness
                .install_os_app(TENANT, "project-management")
                .await
                .unwrap_or_else(|e| panic!("seed {seed}: install failed: {e}"));

            // Create an entity and dispatch an action.
            let entity_id = format!("issue-det-{seed}");
            let r = harness
                .dispatch(
                    TENANT,
                    "Issue",
                    &entity_id,
                    "SetDescription",
                    serde_json::json!({"description": "Determinism canary"}),
                )
                .await;
            let dispatch_ok = r.as_ref().map(|r| r.success).unwrap_or(false);

            // Restart.
            harness.restart().await;

            // Capture observable state for comparison.
            let registry = harness.platform_state.registry.read().unwrap(); // ci-ok: infallible lock
            let entity_type_count = registry
                .entity_types(&temper_runtime::tenant::TenantId::new(TENANT))
                .len();
            drop(registry);

            let index = harness.platform_state.server.entity_index.read().unwrap(); // ci-ok: infallible lock
            let index_key = format!("{TENANT}:Issue");
            let entity_count = index.get(&index_key).map(|ids| ids.len()).unwrap_or(0);
            drop(index);

            results.push((dispatch_ok, entity_type_count, entity_count));
        }

        assert_eq!(
            results[0], results[1],
            "seed {seed}: determinism violation — run 0: {:?}, run 1: {:?}",
            results[0], results[1]
        );
    }
}

// =========================================================================
// Test 7: Boot cycle with combined event-store AND platform-store faults
// =========================================================================

#[tokio::test]
async fn dst_boot_cycle_combined_faults() {
    for seed in 0..NUM_SEEDS {
        let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
        let mut harness = SimPlatformHarness::new(
            seed,
            SimFaultConfig::heavy(),
            SimPlatformFaultConfig::heavy(),
        );

        // OS app install may fail due to faults on either store layer.
        let install_result = harness.install_os_app(TENANT, "project-management").await;

        if install_result.is_err() {
            // Install failed due to combined faults — disable faults for clean restart.
            let prev_event = harness.sim_event_store.disable_faults();
            let prev_plat = harness.sim_platform_store.disable_faults();
            harness.restart().await;

            assert_boot_invariants(&harness).await.unwrap_or_else(|e| {
                panic!("seed {seed}: boot invariants failed after failed install: {e}")
            });
            assert_data_invariants(&harness).await.unwrap_or_else(|e| {
                panic!("seed {seed}: data invariants failed after failed install: {e}")
            });
            harness.sim_event_store.restore_faults(prev_event);
            harness.sim_platform_store.restore_faults(prev_plat);
            continue;
        }

        // Install succeeded despite faults. Dispatch several actions — some will
        // fail due to event-store or platform-store faults.
        let mut success_count = 0;
        for i in 0..5 {
            let entity_id = format!("issue-combined-{seed}-{i}");
            let r = harness
                .dispatch(
                    TENANT,
                    "Issue",
                    &entity_id,
                    "SetDescription",
                    serde_json::json!({"description": "Combined fault test"}),
                )
                .await;
            if let Ok(r) = &r
                && r.success
            {
                success_count += 1;
            }
            // Failures are expected — faults on both layers.
        }
        let _ = success_count; // tracked for diagnostics

        // Disable ALL faults for clean restart and invariant checks.
        let prev_event = harness.sim_event_store.disable_faults();
        let prev_plat = harness.sim_platform_store.disable_faults();
        harness.restart().await;

        assert_boot_invariants(&harness).await.unwrap_or_else(|e| {
            panic!("seed {seed}: boot invariants failed after combined faults: {e}")
        });

        assert_data_invariants(&harness).await.unwrap_or_else(|e| {
            panic!("seed {seed}: data invariants failed after combined faults: {e}")
        });
        harness.sim_event_store.restore_faults(prev_event);
        harness.sim_platform_store.restore_faults(prev_plat);
    }
}
