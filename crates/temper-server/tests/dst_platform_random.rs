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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RandomMode {
    Full,
    Smoke,
}

impl RandomMode {
    fn current() -> Self {
        // determinism-ok: CI mode selection happens before deterministic seeds are installed.
        match std::env::var("TEMPER_DST_RANDOM_MODE") {
            Ok(value) if value == "full" => Self::Full,
            Ok(value) if value == "smoke" => Self::Smoke,
            Ok(value) => {
                panic!("TEMPER_DST_RANDOM_MODE must be 'full' or 'smoke', got {value:?}")
            }
            Err(std::env::VarError::NotPresent) => Self::Full,
            Err(err) => panic!("TEMPER_DST_RANDOM_MODE is not valid UTF-8: {err}"),
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

/// Read a shard env var: `None` if unset or empty, `Some(Ok(n))` if a valid integer,
/// `Some(Err(msg))` if set to something that is not a valid integer.
///
/// An empty string counts as unset because GitHub Actions passes an absent matrix value
/// (`${{ matrix.shard_index }}` on a cell that omits it) as `""`, not by dropping the env.
fn shard_env(name: &str) -> Option<Result<u64, String>> {
    // determinism-ok: CI shard selection happens before deterministic seeds are installed.
    match std::env::var(name) {
        Ok(value) if value.trim().is_empty() => None,
        Ok(value) => Some(
            value
                .trim()
                .parse::<u64>()
                .map_err(|_| format!("{name} must be a non-negative integer, got {value:?}")),
        ),
        Err(std::env::VarError::NotPresent) => None,
        Err(std::env::VarError::NotUnicode(_)) => Some(Err(format!("{name} is not valid UTF-8"))),
    }
}

/// CI seed sharding: split the `0..total` seed range across parallel CI shards.
///
/// Shard `i` of `n` runs the seeds where `seed % n == i`. The union of every shard is the
/// full `0..total` range, so coverage is identical to an unsharded run — sharding only
/// divides the wall-clock, never the coverage. With neither env var set (local runs) it
/// yields every seed. Each seed is self-contained, so which shard runs it does not change
/// the outcome.
///
/// Misconfiguration FAILS LOUDLY rather than silently dropping a partition: a bad or
/// missing `TEMPER_DST_SHARD_INDEX` while `TEMPER_DST_SHARD_COUNT` > 1 would otherwise
/// default the index to 0 and run shard 0 twice while some partition's seeds never run —
/// a silent coverage loss, which the no-silent-failure rule forbids.
fn shard_seeds(total: u64) -> impl Iterator<Item = u64> {
    let count = match shard_env("TEMPER_DST_SHARD_COUNT") {
        None => 1,
        Some(Ok(n)) if n >= 1 => n,
        Some(Ok(n)) => panic!("TEMPER_DST_SHARD_COUNT must be >= 1, got {n}"),
        Some(Err(message)) => panic!("{message}"),
    };
    let index = match shard_env("TEMPER_DST_SHARD_INDEX") {
        Some(Ok(i)) => i,
        Some(Err(message)) => panic!("{message}"),
        None if count == 1 => 0,
        None => panic!(
            "TEMPER_DST_SHARD_COUNT={count} requires TEMPER_DST_SHARD_INDEX to be set \
             (refusing to silently run shard 0 and drop the other partitions)"
        ),
    };
    assert!(
        index < count,
        "TEMPER_DST_SHARD_INDEX ({index}) must be < TEMPER_DST_SHARD_COUNT ({count})"
    );
    (0..total).filter(move |seed| seed % count == index)
}

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
    let mode = RandomMode::current();
    let seeds = mode.seeds(100, 10);
    let ops = mode.ops(50, 20);

    for seed in shard_seeds(seeds) {
        let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
        let mut harness = SimPlatformHarness::no_faults(seed);

        run_workload(&mut harness, seed, ops, true).await;

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
    let mode = RandomMode::current();
    let seeds = mode.seeds(50, 5);
    let ops = mode.ops(30, 15);

    for seed in shard_seeds(seeds) {
        let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
        let mut harness = SimPlatformHarness::new(
            seed,
            SimFaultConfig::heavy(),
            SimPlatformFaultConfig::none(),
        );

        run_workload(&mut harness, seed, ops, true).await;

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

    for seed in shard_seeds(seeds) {
        let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
        let mut harness = SimPlatformHarness::new(
            seed,
            SimFaultConfig::none(),
            SimPlatformFaultConfig::heavy(),
        );

        run_workload(&mut harness, seed, ops, true).await;

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

    for seed in shard_seeds(seeds) {
        let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
        let mut harness = SimPlatformHarness::new(
            seed,
            SimFaultConfig::heavy(),
            SimPlatformFaultConfig::heavy(),
        );

        run_workload(&mut harness, seed, ops, true).await;

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

    for seed in shard_seeds(seeds) {
        let mut results = Vec::new();

        for _run in 0..2 {
            let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
            let mut harness = SimPlatformHarness::no_faults(seed);

            run_workload(&mut harness, seed, ops, false).await;

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
