//! Randomized platform workload DST test suite.
//!
//! Exercises the platform's install/dispatch/persist/restart pipeline with
//! randomized operation sequences generated from deterministic seeds. Each
//! seed produces an identical sequence — failures are reproducible.
//!
//! FoundationDB pattern: same code, simulated I/O, multi-seed coverage.

mod common;

use temper_runtime::scheduler::install_deterministic_context;
use temper_server::platform_store::{PlatformStore, SimPlatformFaultConfig};
use temper_store_sim::SimFaultConfig;

use common::platform_harness::SimPlatformHarness;
use common::platform_invariants::*;
use common::workload_gen::{WorkloadGenerator, WorkloadOp};

// ── Helpers ──────────────────────────────────────────────────────────────

/// Run a full workload: generate `num_ops` operations and execute them.
///
/// When `check_invariants_inline` is true, `CheckInvariants` ops actually
/// run the invariant checkers mid-workload. When false, they are skipped
/// (useful under fault injection where inline checks would see faulted reads).
async fn run_workload(
    harness: &mut SimPlatformHarness,
    seed: u64,
    num_ops: usize,
    check_invariants_inline: bool,
) {
    let mut wg = WorkloadGenerator::new(seed);

    for op_idx in 0..num_ops {
        let op = wg.next_op();
        match &op {
            WorkloadOp::InstallApp { tenant, app } => {
                let result = harness.install_os_app(tenant, app).await;
                if result.is_ok() {
                    wg.record_install(tenant, app);
                }
                // Install may fail due to faults — that's expected.
            }
            WorkloadOp::Dispatch {
                tenant,
                entity_type,
                entity_id,
                action,
            } => {
                let _result = harness
                    .dispatch(
                        tenant,
                        entity_type,
                        entity_id,
                        action,
                        serde_json::json!({"description": format!("seed-{seed}")}),
                    )
                    .await;
                // Dispatch may fail due to invalid action, faults, or missing
                // entity type — all expected platform behavior.
            }
            WorkloadOp::Restart => {
                harness.restart().await;
            }
            WorkloadOp::CheckInvariants => {
                if check_invariants_inline {
                    // Temporarily disable ALL faults so invariant reads succeed.
                    // Use mid-operation invariants (not full P1/P2) since orphaned
                    // specs from failed cleanup are expected mid-workload.
                    let prev_event = harness.sim_event_store.disable_faults();
                    let prev_plat = harness.sim_platform_store.disable_faults();
                    assert_mid_operation_invariants(harness)
                        .await
                        .unwrap_or_else(|e| {
                            panic!("seed {seed}: mid-operation invariants failed: {e}")
                        });
                    harness.sim_event_store.restore_faults(prev_event);
                    harness.sim_platform_store.restore_faults(prev_plat);
                }
            }
        }

        // Per-operation invariant checking (with faults disabled for reads).
        //
        // P1/P2 (registry-store consistency) can be transiently violated when:
        //   (a) `install_os_app` fails mid-write AND cleanup `delete_spec` fails, OR
        //   (b) A faulted `Restart` runs reconciliation but `delete_spec` also fails
        //
        // These orphans are reconciled on a CLEAN restart (faults disabled).
        // The final post-workload restart in each test variant disables faults
        // first, so P1/P2 are fully validated there.
        //
        // Mid-workload, we only check invariants immune to transient orphans
        // (P8: state-store sequence, P9: rollback completeness, P13: monotonicity).
        if check_invariants_inline {
            let prev_event = harness.sim_event_store.disable_faults();
            let prev_plat = harness.sim_platform_store.disable_faults();

            assert_mid_operation_invariants(harness)
                .await
                .unwrap_or_else(|e| {
                    panic!("seed {seed}, op {op_idx}: mid-operation invariants failed: {e}")
                });

            harness.sim_event_store.restore_faults(prev_event);
            harness.sim_platform_store.restore_faults(prev_plat);
        }
    }
}

// =========================================================================
// Test 1: Random workload with no faults
// =========================================================================

#[tokio::test]
async fn dst_random_workload_no_faults() {
    for seed in 0..100 {
        let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
        let mut harness = SimPlatformHarness::no_faults(seed);

        run_workload(&mut harness, seed, 50, true).await;

        // Final invariant check after all ops.
        assert_boot_invariants(&harness)
            .await
            .unwrap_or_else(|e| panic!("seed {seed}: final boot invariants failed: {e}"));
        assert_data_invariants(&harness)
            .await
            .unwrap_or_else(|e| panic!("seed {seed}: final data invariants failed: {e}"));
    }
}

// =========================================================================
// Test 2: Random workload with event-store faults
// =========================================================================

#[tokio::test]
async fn dst_random_workload_event_faults() {
    for seed in 0..50 {
        let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
        let mut harness = SimPlatformHarness::new(
            seed,
            SimFaultConfig::heavy(),
            SimPlatformFaultConfig::none(),
        );

        run_workload(&mut harness, seed, 30, true).await;

        // Disable faults before restart so restore succeeds cleanly.
        let prev_event = harness.sim_event_store.disable_faults();
        harness.restart().await;

        assert_boot_invariants(&harness).await.unwrap_or_else(|e| {
            panic!("seed {seed}: boot invariants failed after event faults: {e}")
        });
        assert_data_invariants(&harness).await.unwrap_or_else(|e| {
            panic!("seed {seed}: data invariants failed after event faults: {e}")
        });
        harness.sim_event_store.restore_faults(prev_event);
    }
}

// =========================================================================
// Test 3: Random workload with platform-store faults
// =========================================================================

#[tokio::test]
async fn dst_random_workload_platform_faults() {
    for seed in 0..50 {
        let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
        let mut harness = SimPlatformHarness::new(
            seed,
            SimFaultConfig::none(),
            SimPlatformFaultConfig::heavy(),
        );

        run_workload(&mut harness, seed, 30, true).await;

        // Disable faults before restart so restore succeeds cleanly.
        let prev_plat = harness.sim_platform_store.disable_faults();
        harness.restart().await;

        assert_boot_invariants(&harness).await.unwrap_or_else(|e| {
            panic!("seed {seed}: boot invariants failed after platform faults: {e}")
        });
        assert_data_invariants(&harness).await.unwrap_or_else(|e| {
            panic!("seed {seed}: data invariants failed after platform faults: {e}")
        });
        harness.sim_platform_store.restore_faults(prev_plat);
    }
}

// =========================================================================
// Test 4: Random workload with combined faults (event + platform)
// =========================================================================

#[tokio::test]
async fn dst_random_workload_combined_faults() {
    for seed in 0..50 {
        let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
        let mut harness = SimPlatformHarness::new(
            seed,
            SimFaultConfig::heavy(),
            SimPlatformFaultConfig::heavy(),
        );

        run_workload(&mut harness, seed, 30, true).await;

        // Disable ALL faults before restart so restore succeeds cleanly.
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

// =========================================================================
// Test 5: Determinism canary — same seed twice yields identical state
// =========================================================================

#[tokio::test]
async fn dst_random_workload_determinism() {
    for seed in 0..10 {
        let mut results = Vec::new();

        for _run in 0..2 {
            let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
            let mut harness = SimPlatformHarness::no_faults(seed);

            run_workload(&mut harness, seed, 50, false).await;

            // Restart so state is fully rebuilt from durable stores.
            harness.restart().await;

            // Capture observable state for comparison.
            let total_events = harness.sim_event_store.total_events();
            let entity_count = harness.sim_event_store.entity_count();

            let spec_count = {
                let registry = harness.platform_state.registry.read().unwrap(); // ci-ok: infallible lock
                let mut count = 0usize;
                for tenant_id in registry.tenant_ids() {
                    count += registry.entity_types(tenant_id).len();
                }
                count
            };

            let installed_apps = harness
                .sim_platform_store
                .list_all_installed_apps()
                .await
                .unwrap_or_default();
            let app_count = installed_apps.len();

            let index_count = {
                let index = harness.platform_state.server.entity_index.read().unwrap(); // ci-ok: infallible lock
                index.values().map(|ids| ids.len()).sum::<usize>()
            };

            results.push((
                total_events,
                entity_count,
                spec_count,
                app_count,
                index_count,
            ));
        }

        assert_eq!(
            results[0], results[1],
            "seed {seed}: determinism violation — run 0: {:?}, run 1: {:?}",
            results[0], results[1]
        );
    }
}
