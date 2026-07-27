//! Deterministic registry-restore quarantine, retention, and recovery proof.

mod common;
#[path = "dst_registry_restore/quarantine_contracts.rs"]
mod quarantine_contracts;

use temper_runtime::scheduler::install_deterministic_context;
use temper_runtime::tenant::TenantId;
use temper_server::platform_store::{PlatformStore, RegistrySourceSnapshot, SimPlatformStore};
use temper_server::registry::SpecRegistry;
use temper_server::registry_bootstrap::restore_registry_from_platform_store;

use common::platform_harness::SimPlatformHarness;
use common::platform_invariants::assert_boot_invariants;

const ORDER_IOA: &str = include_str!("../../../test-fixtures/specs/order.ioa.toml");
const ORDER_CSDL: &str = include_str!("../../../test-fixtures/specs/model.csdl.xml");

async fn source_snapshot(store: &SimPlatformStore) -> RegistrySourceSnapshot {
    RegistrySourceSnapshot::from_rows(
        &store.load_specs().await.expect("load snapshot specs"),
        &store
            .load_tenant_constraints()
            .await
            .expect("load snapshot constraints"),
    )
    .expect("build source snapshot")
}

#[tokio::test]
async fn corrupt_tenant_is_retained_and_recovers_without_harming_healthy_tenants() {
    for seed in 0..5 {
        let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
        let mut harness = SimPlatformHarness::no_faults(seed);
        let healthy_tenant = "healthy-tenant";
        let corrupt_tenant = "corrupt-tenant";

        let healthy_types = harness
            .install_app(healthy_tenant, "project-management")
            .await
            .unwrap_or_else(|error| panic!("seed {seed}: healthy install failed: {error}"));
        assert!(!healthy_types.is_empty());

        harness
            .sim_platform_store
            .upsert_spec(
                corrupt_tenant,
                "Order",
                "[automaton]\nname = \"Order\"\n",
                "<a><b",
                "corrupt-hash",
            )
            .await
            .unwrap_or_else(|error| panic!("seed {seed}: corrupt upsert failed: {error}"));
        harness
            .sim_platform_store
            .commit_specs(corrupt_tenant)
            .await
            .unwrap_or_else(|error| panic!("seed {seed}: corrupt commit failed: {error}"));
        harness
            .sim_platform_store
            .record_installed_app(corrupt_tenant, "corrupt-fixture")
            .await
            .unwrap_or_else(|error| {
                panic!("seed {seed}: corrupt installed-app record failed: {error}")
            });

        assert_degraded_boot(&mut harness, seed, healthy_tenant, corrupt_tenant).await;
        assert_degraded_boot(&mut harness, seed, healthy_tenant, corrupt_tenant).await;

        harness
            .sim_platform_store
            .upsert_spec(
                corrupt_tenant,
                "Order",
                ORDER_IOA,
                ORDER_CSDL,
                "repaired-hash",
            )
            .await
            .unwrap_or_else(|error| panic!("seed {seed}: repair upsert failed: {error}"));
        harness
            .sim_platform_store
            .commit_specs(corrupt_tenant)
            .await
            .unwrap_or_else(|error| panic!("seed {seed}: repair commit failed: {error}"));
        harness.restart().await;

        {
            let registry = harness.registry().read().unwrap(); // ci-ok: infallible lock
            assert!(
                registry
                    .get_table(&TenantId::new(corrupt_tenant), "Order")
                    .is_some(),
                "seed {seed}: repaired tenant did not recover"
            );
            assert!(
                registry.restore_health().is_healthy(),
                "seed {seed}: repaired boot remained degraded"
            );
        }
        assert_boot_invariants(&harness)
            .await
            .unwrap_or_else(|error| panic!("seed {seed}: repaired invariants failed: {error}"));
    }
}

async fn assert_degraded_boot(
    harness: &mut SimPlatformHarness,
    seed: u64,
    healthy_tenant: &str,
    corrupt_tenant: &str,
) {
    harness.restart().await;
    {
        let registry = harness.registry().read().unwrap(); // ci-ok: infallible lock
        assert!(
            !registry
                .entity_types(&TenantId::new(healthy_tenant))
                .is_empty(),
            "seed {seed}: healthy sibling did not boot"
        );
        assert!(
            registry
                .get_table(&TenantId::new(corrupt_tenant), "Order")
                .is_none(),
            "seed {seed}: corrupt entity was activated"
        );
        assert!(
            registry
                .restore_health()
                .is_quarantined(corrupt_tenant, "Order"),
            "seed {seed}: corrupt row lacks quarantine health"
        );
    }

    let retained = harness
        .sim_platform_store
        .load_specs()
        .await
        .unwrap_or_else(|error| panic!("seed {seed}: load specs failed: {error}"));
    assert!(
        retained.iter().any(|row| row.tenant == corrupt_tenant),
        "seed {seed}: corrupt evidence was deleted"
    );
    assert_boot_invariants(harness)
        .await
        .unwrap_or_else(|error| panic!("seed {seed}: degraded invariants failed: {error}"));
}
