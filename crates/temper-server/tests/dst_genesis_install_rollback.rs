//! DST: Genesis install verify + rollback (ARN-421, invariant P18).
//!
//! Contract (spec.md / P18 / this scenario all say the same thing): an install that never reaches
//! runtime-ready must leave the tenant on the PRIOR good Genesis version, never in the partial
//! local-provenance state a failed reconcile leaves. Here we drive the **production rollback
//! effect** `temper_platform::genesis_install::restore_prior_install` against the simulated
//! platform store under injected faults and assert the durable record is restored to the prior
//! good Genesis record.
//!
//! Fails-before-fix: before `restore_prior_install` existed there was no rollback, so the durable
//! record stayed in the partial local-provenance state — `after == v1_good` fails. Only the
//! rollback moves the tenant back to the prior good record.
//!
//! Coverage note (honest boundary): the DST drives the network-free rollback core against the real
//! sim store with faults. The pure routing decision (`classify_install_verify`) and the compile
//! probe are unit-tested separately in `temper-platform`. The full `install_genesis_app_from_registry`
//! entry point is not simulated because its closure materialization is network/git-backed.

mod common;

use common::platform_harness::SimPlatformHarness;
use temper_platform::genesis_install_verify::restore_prior_install;
use temper_runtime::scheduler::install_deterministic_context;
use temper_server::platform_store::{InstalledAppRecord, PlatformStore, SimPlatformFaultConfig};
use temper_store_sim::SimFaultConfig;

const NUM_SEEDS: u64 = 64;
const APP: &str = "project-management";
const V1_GOOD_REF: &str = "arni/project-management@v1good";

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

        // ── Baseline (faults off): install the good app, then record the prior good Genesis
        //    provenance (v1) carrying the real bundle digest so the rollback's digest-integrity
        //    check is satisfied. Then simulate a failed new install: reconcile overwrites the
        //    durable record with a local-provenance row (`source_kind = "local"`, `app_ref = ""`) —
        //    exactly the partial state a failed publish leaves behind. ─────────────────────────────
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

        let v1_good = InstalledAppRecord {
            source_kind: "genesis".to_string(),
            app_ref: V1_GOOD_REF.to_string(),
            version_hash: "v1good".to_string(),
            pinned_version_hash: "v1good".to_string(),
            current_version_hash: "v1good".to_string(),
            follow_policy: "pinned".to_string(),
            closure_id: format!("genesis:{V1_GOOD_REF}:v1good"),
            registry_url: "https://genesis.example/registry".to_string(),
            registry_tenant: "default".to_string(),
            status: "installed".to_string(),
            ..installed.clone()
        };
        harness
            .sim_platform_store
            .record_installed_app_metadata(&v1_good)
            .await
            .unwrap_or_else(|e| panic!("seed {seed}: record prior good failed: {e}"));
        // Failed new install leaves a local-provenance partial row.
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
        assert_ne!(
            partial, v1_good,
            "seed {seed}: precondition — the partial state must differ from the prior good record"
        );
        assert_ne!(partial.source_kind, "genesis");
        harness.sim_platform_store.restore_faults(prev);

        // ── Roll back to the prior good version under heavy platform-store faults. ───────────────
        let result = restore_prior_install(
            &harness.platform_state,
            harness.sim_platform_store.as_ref(),
            tenant,
            &v1_good,
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
            Ok(()) => {
                // Rollback succeeded: the durable record is exactly the prior good Genesis record.
                // This is the load-bearing P18 assertion — it fails without `restore_prior_install`
                // (the record would stay in the `partial` local-provenance state).
                assert_eq!(
                    after, v1_good,
                    "seed {seed}: P18 — a successful rollback pins the tenant to the prior good record"
                );
            }
            Err(_) => {
                // A store fault aborted the rollback. It must never fabricate a Genesis provenance
                // for anything other than the prior good record: the record is either the restored
                // good one or a local reconcile row — never a bogus committed Genesis version.
                assert!(
                    after == v1_good || after.source_kind != "genesis",
                    "seed {seed}: P18 — a faulted rollback never leaves a bogus Genesis provenance: {after:?}"
                );
            }
        }
        harness.sim_platform_store.restore_faults(prev);
    }
}
