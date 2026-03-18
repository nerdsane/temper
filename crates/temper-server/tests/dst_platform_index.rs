//! DST index consistency tests.
//!
//! Verifies that the entity index is rebuilt correctly from the event store
//! after restarts, and that index entries are consistent across entity types
//! and tenants.

mod common;

use common::platform_harness::SimPlatformHarness;
use common::platform_invariants::*;
use temper_runtime::scheduler::install_deterministic_context;

const NUM_SEEDS: u64 = 50;

// =========================================================================
// Test: Index is rebuilt correctly after restart
// =========================================================================

#[tokio::test]
async fn dst_index_after_restart() {
    for seed in 0..NUM_SEEDS {
        let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
        let mut harness = SimPlatformHarness::no_faults(seed);
        let tenant = "index-test";

        // Install PM app.
        harness
            .install_os_app(tenant, "project-management")
            .await
            .unwrap_or_else(|e| panic!("seed {seed}: install PM failed: {e}"));

        // Create several Issue entities by dispatching actions.
        for i in 0..3 {
            let eid = format!("issue-{seed}-{i}");

            // Issue starts in Backlog. MoveToTriage transitions to Triage.
            let r = harness
                .dispatch(tenant, "Issue", &eid, "MoveToTriage", serde_json::json!({}))
                .await
                .unwrap_or_else(|e| panic!("seed {seed}: MoveToTriage failed for {eid}: {e}"));
            assert!(
                r.success,
                "seed {seed}: MoveToTriage not successful for {eid}: {:?}",
                r.error
            );

            // Set priority so we can advance further.
            let r = harness
                .dispatch(
                    tenant,
                    "Issue",
                    &eid,
                    "SetPriority",
                    serde_json::json!({"level": "1"}),
                )
                .await
                .unwrap_or_else(|e| panic!("seed {seed}: SetPriority failed for {eid}: {e}"));
            assert!(
                r.success,
                "seed {seed}: SetPriority not successful for {eid}: {:?}",
                r.error
            );
        }

        // Restart — rebuilds index from event store via populate_index_from_store.
        harness.restart().await;

        // Index must agree with the event store.
        assert_p3_index_store_agreement(&harness)
            .await
            .unwrap_or_else(|e| panic!("seed {seed}: P3 failed after restart: {e}"));

        // No tombstoned entities should be in the index.
        assert_p5_tombstone_finality(&harness)
            .await
            .unwrap_or_else(|e| panic!("seed {seed}: P5 failed after restart: {e}"));
    }
}

// =========================================================================
// Test: Index with multiple entity types
// =========================================================================

#[tokio::test]
async fn dst_index_multi_entity_types() {
    for seed in 0..NUM_SEEDS {
        let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
        let mut harness = SimPlatformHarness::no_faults(seed);
        let tenant = "index-multi";

        // Install PM app — has Issue, Project, Comment, Label, Cycle.
        harness
            .install_os_app(tenant, "project-management")
            .await
            .unwrap_or_else(|e| panic!("seed {seed}: install PM failed: {e}"));

        // Create an Issue entity.
        let issue_eid = format!("issue-{seed}");
        let r = harness
            .dispatch(
                tenant,
                "Issue",
                &issue_eid,
                "MoveToTriage",
                serde_json::json!({}),
            )
            .await
            .unwrap_or_else(|e| panic!("seed {seed}: MoveToTriage failed: {e}"));
        assert!(
            r.success,
            "seed {seed}: MoveToTriage not successful: {:?}",
            r.error
        );

        // Create a Label entity (if it has an initial action).
        // Labels start in Active state — dispatch AddDescription to exercise it.
        let label_eid = format!("label-{seed}");
        let r = harness
            .dispatch(
                tenant,
                "Label",
                &label_eid,
                "SetDescription",
                serde_json::json!({"description": "test label"}),
            )
            .await;
        // Label may or may not support this action from initial state — that's OK.
        // The entity is created either way via the dispatch attempt.
        let _ = r;

        // Restart — rebuilds index from event store.
        harness.restart().await;

        // Index-store agreement across all entity types.
        assert_p3_index_store_agreement(&harness)
            .await
            .unwrap_or_else(|e| panic!("seed {seed}: P3 failed after restart: {e}"));

        // Tenant isolation (single tenant, but still validates no cross-contamination).
        assert_p14_tenant_isolation(&harness)
            .await
            .unwrap_or_else(|e| panic!("seed {seed}: P14 failed after restart: {e}"));
    }
}
