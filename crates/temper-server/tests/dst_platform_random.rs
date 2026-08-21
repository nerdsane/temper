//! Randomized platform workload DST test suite.
//!
//! Exercises the platform's install/dispatch/persist/restart pipeline with
//! randomized operation sequences generated from deterministic seeds. Each
//! seed produces an identical sequence — failures are reproducible.
//!
//! FoundationDB pattern: same code, simulated I/O, multi-seed coverage.

mod common;
#[path = "common/workload_budget.rs"]
mod workload_budget;

use temper_runtime::scheduler::install_deterministic_context;
use temper_server::platform_store::{PlatformStore, SimPlatformFaultConfig};
use temper_store_sim::SimFaultConfig;

use common::platform_harness::SimPlatformHarness;
use common::platform_invariants::*;
use common::workload_gen::{MAX_UNIQUE_INSTALLS, WorkloadGenerator, WorkloadOp};
use workload_budget::{WorkloadBudget, format_operation_progress, report_progress};

// ── Helpers ──────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RandomMode {
    Full,
    Smoke,
}

impl RandomMode {
    fn current() -> Self {
        // determinism-ok: CI mode selection happens before deterministic seeds are installed.
        match std::env::var("TEMPER_DST_RANDOM_MODE") {
            Ok(value) => Self::parse(Some(&value)),
            Err(std::env::VarError::NotPresent) => Self::parse(None),
            Err(err) => panic!("TEMPER_DST_RANDOM_MODE is not valid UTF-8: {err}"),
        }
    }

    fn parse(value: Option<&str>) -> Self {
        match value {
            Some("full") => Self::Full,
            // Keep ordinary workspace runs bounded. The dedicated non-PR DST
            // CI job sets `full` explicitly and retains the 100-seed coverage.
            Some("smoke") | None => Self::Smoke,
            Some(value) => {
                panic!("TEMPER_DST_RANDOM_MODE must be 'full' or 'smoke', got {value:?}")
            }
        }
    }

    fn seeds(self, full: u64, smoke: u64) -> u64 {
        match self {
            Self::Full => full,
            Self::Smoke => smoke,
        }
    }

    fn ops(self, full: usize, smoke: usize) -> usize {
        match self {
            Self::Full => full,
            Self::Smoke => smoke,
        }
    }
}

#[test]
fn random_mode_defaults_to_smoke_and_keeps_full_explicit() {
    assert_eq!(RandomMode::parse(None), RandomMode::Smoke);
    assert_eq!(RandomMode::parse(Some("smoke")), RandomMode::Smoke);
    assert_eq!(RandomMode::parse(Some("full")), RandomMode::Full);
}

#[test]
fn successful_install_pairs_are_not_generated_twice() {
    let mut generator = WorkloadGenerator::new(42);
    let mut installed = std::collections::BTreeSet::new();

    for _ in 0..500 {
        let op = generator.next_op();
        if let WorkloadOp::InstallApp { tenant, app } = op {
            assert!(
                installed.insert((tenant.clone(), app.clone())),
                "generated duplicate successful install for ({tenant}, {app})"
            );
            generator.record_install(&tenant, &app);
        }
    }

    assert_eq!(installed.len(), MAX_UNIQUE_INSTALLS);
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct WorkloadReport {
    operations: usize,
    installs: usize,
    dispatches: usize,
    restarts: usize,
    invariant_ops: usize,
    invariant_sweeps: usize,
}

impl WorkloadReport {
    fn record(&mut self, op: &WorkloadOp) {
        self.operations += 1;
        match op {
            WorkloadOp::InstallApp { .. } => self.installs += 1,
            WorkloadOp::Dispatch { .. } => self.dispatches += 1,
            WorkloadOp::Restart => self.restarts += 1,
            WorkloadOp::CheckInvariants => self.invariant_ops += 1,
        }
    }

    fn merge(&mut self, other: Self) {
        self.operations += other.operations;
        self.installs += other.installs;
        self.dispatches += other.dispatches;
        self.restarts += other.restarts;
        self.invariant_ops += other.invariant_ops;
        self.invariant_sweeps += other.invariant_sweeps;
    }
}

/// Run a full workload: generate `num_ops` operations and execute them.
///
/// When `check_invariants_inline` is true, one mid-operation invariant sweep
/// runs after every operation. When false, all inline sweeps are skipped.
async fn run_workload(
    scenario: &str,
    harness: &mut SimPlatformHarness,
    seed: u64,
    num_ops: usize,
    check_invariants_inline: bool,
    max_install_attempts: usize,
) -> WorkloadReport {
    let mut wg = WorkloadGenerator::new(seed);
    let mut budget = WorkloadBudget::new(num_ops, max_install_attempts, check_invariants_inline);
    let mut report = WorkloadReport::default();

    for op_idx in 0..num_ops {
        let op = wg.next_op();
        budget.consume_operation(seed, op_idx, &op);
        report.record(&op);
        report_progress(&format_operation_progress(
            scenario, seed, op_idx, num_ops, &op, "execute",
        ));
        match &op {
            WorkloadOp::InstallApp { tenant, app } => {
                let result = harness.install_app(tenant, app).await;
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
                // The per-operation sweep below is the explicit check. Running
                // it here as well would duplicate the most expensive phase.
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
            budget.consume_invariant_sweep(seed, op_idx, &op);
            report.invariant_sweeps += 1;
            report_progress(&format_operation_progress(
                scenario,
                seed,
                op_idx,
                num_ops,
                &op,
                "invariants",
            ));
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

    budget.assert_consumed(seed);
    report_progress(&format!(
        "DST progress scenario={scenario} seed={seed} phase=workload-complete report={report:?}"
    ));
    report
}

// =========================================================================
// Test 1: Random workload with no faults
// =========================================================================

#[tokio::test]
async fn dst_random_workload_no_faults() {
    let mode = RandomMode::current();
    let seeds = mode.seeds(100, 10);
    let ops = mode.ops(50, 20);

    let mut coverage = WorkloadReport::default();
    for seed in 0..seeds {
        report_progress(&format!(
            "DST progress scenario=no-faults seed={seed}/{seeds} phase=seed-start"
        ));
        let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
        let mut harness = SimPlatformHarness::no_faults(seed);

        let report = run_workload(
            "no-faults",
            &mut harness,
            seed,
            ops,
            true,
            MAX_UNIQUE_INSTALLS,
        )
        .await;
        assert!(
            report.installs <= MAX_UNIQUE_INSTALLS,
            "seed {seed}: no-fault workload exceeded its unique-install budget"
        );
        coverage.merge(report);

        // Final invariant check after all ops.
        report_progress(&format!(
            "DST progress scenario=no-faults seed={seed}/{seeds} phase=final-boot-invariants"
        ));
        assert_boot_invariants(&harness)
            .await
            .unwrap_or_else(|e| panic!("seed {seed}: final boot invariants failed: {e}"));
        report_progress(&format!(
            "DST progress scenario=no-faults seed={seed}/{seeds} phase=final-data-invariants"
        ));
        assert_data_invariants(&harness)
            .await
            .unwrap_or_else(|e| panic!("seed {seed}: final data invariants failed: {e}"));
        report_progress(&format!(
            "DST progress scenario=no-faults seed={seed}/{seeds} phase=seed-complete"
        ));
    }

    assert_eq!(coverage.operations, seeds as usize * ops);
    assert_eq!(coverage.invariant_sweeps, coverage.operations);
    assert!(coverage.installs > 0, "workload did not cover installs");
    assert!(coverage.dispatches > 0, "workload did not cover dispatches");
    assert!(coverage.restarts > 0, "workload did not cover restarts");
    assert!(
        coverage.invariant_ops > 0,
        "workload did not cover explicit invariant operations"
    );
}

// =========================================================================
// Test 2: Random workload with event-store faults
// =========================================================================

#[tokio::test]
async fn dst_random_workload_event_faults() {
    let mode = RandomMode::current();
    let seeds = mode.seeds(50, 5);
    let ops = mode.ops(30, 15);

    for seed in 0..seeds {
        let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
        let mut harness = SimPlatformHarness::new(
            seed,
            SimFaultConfig::heavy(),
            SimPlatformFaultConfig::none(),
        );

        run_workload("event-faults", &mut harness, seed, ops, true, ops).await;

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
    let mode = RandomMode::current();
    let seeds = mode.seeds(50, 5);
    let ops = mode.ops(30, 15);

    for seed in 0..seeds {
        let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
        let mut harness = SimPlatformHarness::new(
            seed,
            SimFaultConfig::none(),
            SimPlatformFaultConfig::heavy(),
        );

        run_workload("platform-faults", &mut harness, seed, ops, true, ops).await;

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
    let mode = RandomMode::current();
    let seeds = mode.seeds(50, 5);
    let ops = mode.ops(30, 15);

    for seed in 0..seeds {
        let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
        let mut harness = SimPlatformHarness::new(
            seed,
            SimFaultConfig::heavy(),
            SimPlatformFaultConfig::heavy(),
        );

        run_workload("combined-faults", &mut harness, seed, ops, true, ops).await;

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
    let mode = RandomMode::current();
    let seeds = mode.seeds(10, 3);
    let ops = mode.ops(50, 20);

    for seed in 0..seeds {
        let mut results = Vec::new();

        for run in 0..2 {
            let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
            let mut harness = SimPlatformHarness::no_faults(seed);

            let scenario = format!("determinism-run-{run}");
            run_workload(
                &scenario,
                &mut harness,
                seed,
                ops,
                false,
                MAX_UNIQUE_INSTALLS,
            )
            .await;

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
