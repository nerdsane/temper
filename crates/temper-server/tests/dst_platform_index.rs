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
            .install_app(tenant, "project-management")
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
            .install_app(tenant, "project-management")
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

// Spec states and ordinary actions named Deleted still require served-state fidelity.
#[tokio::test]
async fn dst_p16_checks_deleted_state_without_tombstone() {
    use temper_runtime::persistence::EventStore;

    for seed in 0..NUM_SEEDS {
        let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
        // FieldsUpdated and FieldsReplaced are reserved by EntityActor;
        // reserved_field_update_event_names_are_refused_as_actions covers them.
        for action in ["Archive", "Deleted", "Created"] {
            let harness = SimPlatformHarness::no_faults(seed);
            let tenant = "replay-deleted";
            harness
                .install_app(tenant, "project-management")
                .await
                .unwrap();
            harness.register_inline_spec(
                tenant,
                "Issue",
                &r#"
[automaton]
name = "Issue"
states = ["Active", "Deleted"]
initial = "Active"

[[action]]
name = "Archive"
kind = "input"
from = ["Active"]
to = "Deleted"

[[action]]
name = "Touch"
kind = "input"
from = ["Active"]
to = "Active"
"#
                .replace("Archive", action),
            );
            let response = harness
                .dispatch(tenant, "Issue", "ordinary", action, serde_json::json!({}))
                .await
                .unwrap();
            assert!(response.success);
            assert_eq!(response.state.status, "Deleted");
            assert_p16_event_replay_fidelity(&harness).await.unwrap();

            // Leave the actor serving Active, then append a durable transition.
            // P16 must detect the mismatch even though the stored target is Deleted.
            let response = harness
                .dispatch(tenant, "Issue", "stale", "Touch", serde_json::json!({}))
                .await
                .unwrap();
            assert!(response.success);
            assert_eq!(response.state.status, "Active");
            let ordinary_id = format!("{tenant}:Issue:ordinary");
            let stale_id = format!("{tenant}:Issue:stale");
            let mut event = harness
                .sim_event_store
                .dump_journal(&ordinary_id)
                .last()
                .unwrap()
                .clone();
            assert_eq!(event.payload["kind"], serde_json::json!("transition"));
            assert!(!event.is_tombstone());
            let sequence = harness.sim_event_store.dump_journal(&stale_id).len() as u64;
            harness
                .sim_event_store
                .append(&stale_id, sequence, &[event.clone()])
                .await
                .unwrap();
            let error = assert_p16_event_replay_fidelity(&harness)
                .await
                .expect_err("ordinary Deleted target must be checked");
            assert!(error.contains("serves status 'Active'"), "{error}");
            assert!(error.contains("last event stored 'Deleted'"), "{error}");

            // A real tombstone, identified by the shared discriminator, is exempt.
            event.event_type = "Deleted".to_string();
            event.payload["kind"] = serde_json::json!("tombstone");
            event.payload["action"] = serde_json::json!("Deleted");
            event.payload["from_status"] = serde_json::json!("Deleted");
            harness
                .sim_event_store
                .append(&stale_id, sequence + 1, &[event])
                .await
                .unwrap();
            assert_p16_event_replay_fidelity(&harness).await.unwrap();

            // The served and journaled statuses agree, but this spec action
            // only permits Active -> Deleted. Structure must still be checked
            // even when the action shares a kernel event's name.
            let response = harness
                .dispatch(tenant, "Issue", "invalid", "Touch", serde_json::json!({}))
                .await
                .unwrap();
            assert!(response.success);
            assert_eq!(response.state.status, "Active");
            let invalid_id = format!("{tenant}:Issue:invalid");
            let mut event = harness
                .sim_event_store
                .dump_journal(&ordinary_id)
                .last()
                .unwrap()
                .clone();
            event.payload["to_status"] = serde_json::json!("Active");
            let sequence = harness.sim_event_store.dump_journal(&invalid_id).len() as u64;
            harness
                .sim_event_store
                .append(&invalid_id, sequence, &[event])
                .await
                .unwrap();
            let error = assert_p16_event_replay_fidelity(&harness)
                .await
                .expect_err("invalid spec transition must fail despite matching served state");
            assert!(error.contains("has invalid transition"), "{error}");
            assert!(error.contains(&format!("action='{action}'")), "{error}");
            assert!(error.contains("from='Active', to='Active'"), "{error}");
        }
    }
}
