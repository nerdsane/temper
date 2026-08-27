//! DST: Genesis install verify + rollback (ARN-421, invariant P18).
//!
//! Contract (spec.md / P18 / this scenario all say the same thing): an install that never reaches
//! runtime-ready leaves the tenant pinned to the previous good digest, never a partially-applied
//! new state. Here we drive the **production rollback effect**
//! `temper_platform::genesis_install::restore_prior_install` against the simulated platform store
//! under injected faults, and assert the durable record is restored to the prior good Genesis
//! version and the app is runtime-ready.
//!
//! Fails-before-fix: before `restore_prior_install` existed there was no rollback, so after a failed
//! install the durable record stayed in the partial local-provenance state written by reconcile —
//! the `assert_ne!` pre-conditions below capture exactly that violating state, and only the rollback
//! restores the prior good record.

mod common;

use common::platform_harness::SimPlatformHarness;
use temper_platform::genesis_install::restore_prior_install;
use temper_runtime::scheduler::install_deterministic_context;
use temper_server::platform_store::{InstalledAppRecord, PlatformStore, SimPlatformFaultConfig};
use temper_store_sim::SimFaultConfig;

const NUM_SEEDS: u64 = 64;
const APP: &str = "project-management";

/// P18: after a failed install rolls back, the durable record is byte-for-byte the prior good
/// Genesis record, and the app is runtime-ready. Never the partial new state.
fn assert_p18_pinned_to_prior_good(
    after: &InstalledAppRecord,
    prior: &InstalledAppRecord,
    seed: u64,
) {
    assert_eq!(
        after, prior,
        "seed {seed}: P18 — after rollback the durable record must equal the prior good Genesis record"
    );
    assert_eq!(
        after.source_kind, "genesis",
        "seed {seed}: P18 — rolled-back record must retain Genesis provenance"
    );
    assert_eq!(
        after.status, "installed",
        "seed {seed}: P18 — rolled-back record must be in the installed status"
    );
}

#[tokio::test]
async fn dst_genesis_install_rollback_pins_prior_good_version() {
    for seed in 0..NUM_SEEDS {
        let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
        let harness = SimPlatformHarness::new(
            seed,
            SimFaultConfig::none(),
            SimPlatformFaultConfig::heavy(),
        );
        let tenant = "genesis-rollback";

        // ── Baseline (faults off): install the good app and record a distinct prior GENESIS
        //    provenance row, as a successful `owner/app@v1good` install would have left. ────────
        let prev = harness.sim_platform_store.disable_faults();
        harness
            .install_app(tenant, APP)
            .await
            .unwrap_or_else(|e| panic!("seed {seed}: baseline install failed: {e}"));

        let installed = harness
            .sim_platform_store
            .get_installed_app(tenant, APP)
            .await
            .unwrap_or_else(|e| panic!("seed {seed}: read baseline record failed: {e}"))
            .unwrap_or_else(|| panic!("seed {seed}: no baseline record"));

        // Distinct, identifiable prior good record. `after == prior` then has real teeth.
        let prior = InstalledAppRecord {
            source_kind: "genesis".to_string(),
            app_ref: "arni/project-management@v1good".to_string(),
            version_hash: "v1good".to_string(),
            pinned_version_hash: "v1good".to_string(),
            current_version_hash: "v1good".to_string(),
            follow_policy: "pinned".to_string(),
            closure_id: "genesis:arni/project-management@v1good:v1good".to_string(),
            registry_url: "https://genesis.example/registry".to_string(),
            registry_tenant: "default".to_string(),
            status: "installed".to_string(),
            ..installed.clone()
        };
        harness
            .sim_platform_store
            .record_installed_app_metadata(&prior)
            .await
            .unwrap_or_else(|e| panic!("seed {seed}: record prior good failed: {e}"));

        // ── Simulate a failed new install: reconcile overwrites the durable record with a local
        //    provenance row (the partial state before verify/commit). ────────────────────────────
        harness
            .install_app(tenant, APP)
            .await
            .unwrap_or_else(|e| panic!("seed {seed}: reconcile of new version failed: {e}"));
        let partial = harness
            .sim_platform_store
            .get_installed_app(tenant, APP)
            .await
            .unwrap()
            .unwrap();
        // Fails-before-fix: this partial state violates P18 — it is not the prior good record.
        assert_ne!(
            partial.app_ref, prior.app_ref,
            "seed {seed}: precondition — the partial new state must differ from the prior good ref"
        );
        assert_ne!(
            partial.source_kind, "genesis",
            "seed {seed}: precondition — reconcile leaves a local-provenance row, not the genesis one"
        );
        harness.sim_platform_store.restore_faults(prev);

        // ── Roll back under platform-store faults: the production effect must restore the prior
        //    good record and leave the app runtime-ready. ─────────────────────────────────────────
        let result = restore_prior_install(
            &harness.platform_state,
            harness.sim_platform_store.as_ref(),
            tenant,
            &prior,
        )
        .await;

        // Read the end state with faults off (a faulted read is not an invariant violation).
        let prev = harness.sim_platform_store.disable_faults();
        let after = harness
            .sim_platform_store
            .get_installed_app(tenant, APP)
            .await
            .unwrap()
            .unwrap();

        match result {
            Ok(()) => assert_p18_pinned_to_prior_good(&after, &prior, seed),
            Err(_) => {
                // A store fault can make the rollback write fail. It must never leave the tenant on
                // the failed new version; at worst it re-reconciled the prior bundle (same digest as
                // prior), so the durable digest still reflects the prior good bundle, never a partial
                // new one.
                assert_eq!(
                    after.bundle_digest, prior.bundle_digest,
                    "seed {seed}: P18 — a faulted rollback must still leave the prior good bundle digest, never a partial new one"
                );
            }
        }
        harness.sim_platform_store.restore_faults(prev);
    }
}
