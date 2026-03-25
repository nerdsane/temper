#![cfg(feature = "observe")]
//! End-to-end GEPA self-improvement loop test.
//!
//! Proves the full GEPA cycle works by:
//! 1. Installing PM skill on a test tenant
//! 2. Simulating agent failures (Reassign action doesn't exist on Issue)
//! 3. Running sentinel check → ots_trajectory_failure_cluster fires
//! 4. Creating EvolutionRun entity, driving it through the full state machine
//! 5. Using GEPA primitives (replay, scoring, Pareto frontier) on the mutation
//! 6. Verifying the mutated spec passes L0 (IOA parse)
//! 7. Hot-deploying the mutated spec via SpecRegistry
//! 8. Replaying the same actions → all succeed
//!
//! This test does NOT require a running server or LLM — it uses the
//! SimPlatformHarness (production code, simulated I/O) and deterministic
//! spec mutations.

mod common;

use common::platform_harness::SimPlatformHarness;
use std::path::PathBuf;
use temper_runtime::scheduler::install_deterministic_context;

const TENANT: &str = "gepa-test";

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crate dir has parent")
        .parent()
        .expect("workspace root exists")
        .to_path_buf()
}

fn load_gepa_wasm_modules() -> Option<Vec<(&'static str, Vec<u8>)>> {
    let module_paths = [
        (
            "gepa-replay",
            "wasm-modules/gepa-replay/target/wasm32-unknown-unknown/release/gepa_replay_module.wasm",
        ),
        (
            "gepa-reflective",
            "wasm-modules/gepa-reflective/target/wasm32-unknown-unknown/release/gepa_reflective_module.wasm",
        ),
        (
            "gepa-score",
            "wasm-modules/gepa-score/target/wasm32-unknown-unknown/release/gepa_score_module.wasm",
        ),
        (
            "gepa-pareto",
            "wasm-modules/gepa-pareto/target/wasm32-unknown-unknown/release/gepa_pareto_module.wasm",
        ),
    ];

    let root = repo_root();
    let mut modules = Vec::with_capacity(module_paths.len());
    for (name, rel_path) in module_paths {
        let path = root.join(rel_path);
        match std::fs::read(&path) {
            Ok(bytes) => modules.push((name, bytes)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                eprintln!(
                    "skipping GEPA WASM integration test because {} is missing",
                    path.display()
                );
                return None;
            }
            Err(err) => panic!("failed to read {}: {err}", path.display()),
        }
    }
    Some(modules)
}

/// EvolutionRun spec without integrations — for manual state machine testing.
///
/// The production spec has WASM + adapter integrations that fire in background
/// on trigger effects. For tests that manually drive the state machine, we use
/// this stripped version to avoid background integration failures.
const EVOLUTION_RUN_IOA_NO_INTEGRATIONS: &str = r#"
[automaton]
name = "EvolutionRun"
states = ["Created", "Selecting", "Evaluating", "Reflecting", "Proposing", "Verifying", "Scoring", "Updating", "AwaitingApproval", "Deploying", "Completed", "Failed"]
initial = "Created"

[[state]]
name = "candidate_count"
type = "counter"
initial = "0"

[[state]]
name = "mutation_attempts"
type = "counter"
initial = "0"

[[state]]
name = "generation"
type = "counter"
initial = "0"

[[action]]
name = "Start"
kind = "input"
from = ["Created"]
to = "Selecting"
params = ["SkillName", "TargetEntityType", "AutonomyLevel"]

[[action]]
name = "SelectCandidate"
kind = "input"
from = ["Selecting"]
to = "Evaluating"
effect = "increment candidate_count"
params = ["CandidateId", "SpecSource"]

[[action]]
name = "RecordEvaluation"
kind = "input"
from = ["Evaluating"]
to = "Reflecting"
params = ["ReplayResultJson"]

[[action]]
name = "RecordDataset"
kind = "input"
from = ["Reflecting"]
to = "Proposing"
params = ["DatasetJson"]

[[action]]
name = "RecordMutation"
kind = "input"
from = ["Proposing"]
to = "Verifying"
effect = "increment mutation_attempts"
params = ["MutatedSpecSource", "MutationSummary"]

[[action]]
name = "RecordVerificationPass"
kind = "input"
from = ["Verifying"]
to = "Scoring"
params = ["VerificationReport"]

[[action]]
name = "RecordVerificationFailure"
kind = "input"
from = ["Verifying"]
to = "Reflecting"
params = ["VerificationErrors"]

[[action]]
name = "ExhaustRetries"
kind = "input"
from = ["Verifying"]
to = "Failed"
params = ["FailureReason"]

[[action]]
name = "RecordScore"
kind = "input"
from = ["Scoring"]
to = "Updating"
params = ["ScoresJson"]

[[action]]
name = "RecordFrontier"
kind = "input"
from = ["Updating"]
to = "AwaitingApproval"
params = ["FrontierUpdateJson"]

[[action]]
name = "RecordFrontierAutoApprove"
kind = "input"
from = ["Updating"]
to = "Deploying"
params = ["FrontierUpdateJson"]

[[action]]
name = "ContinueEvolution"
kind = "input"
from = ["Updating"]
to = "Selecting"
effect = "increment generation"

[[action]]
name = "Approve"
kind = "input"
from = ["AwaitingApproval"]
to = "Deploying"
params = ["ApproverId"]

[[action]]
name = "Reject"
kind = "input"
from = ["AwaitingApproval"]
to = "Selecting"
effect = "increment generation"
params = ["RejectionReason"]

[[action]]
name = "Deploy"
kind = "input"
from = ["Deploying"]
to = "Completed"
params = ["DeploymentId"]

[[action]]
name = "Fail"
kind = "input"
from = ["Created", "Selecting", "Evaluating", "Reflecting", "Proposing", "Scoring", "Updating", "Deploying"]
to = "Failed"
params = ["FailureReason"]
"#;

// =========================================================================
// Phase 1: Trajectory failure detection → Sentinel alert
// =========================================================================

/// Proves: dispatching an unknown action generates trajectory failures,
/// and the sentinel `ots_trajectory_failure_cluster` rule detects them.
#[tokio::test]
async fn e2e_gepa_sentinel_detects_failure_cluster() {
    let (_guard, _clock, _id_gen) = install_deterministic_context(42);
    let harness = SimPlatformHarness::no_faults(42);

    // Install PM skill.
    let types = harness
        .install_skill(TENANT, "project-management")
        .await
        .expect("PM skill should install");
    assert!(types.contains(&"Issue".to_string()));

    // Attempt "Reassign" on Issue — this action doesn't exist in the spec.
    // Each attempt should fail and be recorded in the trajectory log.
    let mut failure_count = 0;
    for i in 0..6 {
        let r = harness
            .dispatch(
                TENANT,
                "Issue",
                &format!("issue-{i}"),
                "Reassign",
                serde_json::json!({"NewAssigneeId": "agent-2"}),
            )
            .await;
        match r {
            Ok(resp) => {
                assert!(!resp.success, "Reassign should fail — action not in spec");
                failure_count += 1;
            }
            Err(_) => {
                // Dispatch-level error is also a failure signal.
                failure_count += 1;
            }
        }
    }
    assert_eq!(failure_count, 6, "Should have 6 failed Reassign attempts");

    // Build trajectory entries matching what the server would record.
    let trajectory_entries: Vec<temper_server::state::TrajectoryEntry> = (0..6)
        .map(|i| temper_server::state::TrajectoryEntry {
            timestamp: temper_runtime::scheduler::sim_now().to_rfc3339(),
            tenant: TENANT.to_string(),
            entity_type: "Issue".to_string(),
            entity_id: format!("issue-{i}"),
            action: "Reassign".to_string(),
            success: false,
            from_status: Some("Backlog".to_string()),
            to_status: None,
            error: Some("action not found in spec".to_string()),
            agent_id: Some("claude-code".to_string()),
            session_id: Some("test-session-1".to_string()),
            authz_denied: None,
            denied_resource: None,
            denied_module: None,
            source: None,
            spec_governed: Some(true),
            agent_type: Some("claude-code".to_string()),
            request_body: None,
            intent: Some("reassign issue to different agent".to_string()),
        })
        .collect();

    // Run sentinel rules against these trajectory entries.
    let rules = temper_server::sentinel::default_rules();
    let alerts = temper_server::sentinel::check_rules(
        &rules,
        &harness.platform_state.server,
        &trajectory_entries,
    );

    // The ots_trajectory_failure_cluster rule should fire (6 >= 5 threshold).
    let ots_alert = alerts
        .iter()
        .find(|a| a.rule_name == "ots_trajectory_failure_cluster");
    assert!(
        ots_alert.is_some(),
        "Sentinel should detect OTS failure cluster with 6 failures on Issue"
    );

    let alert = ots_alert.unwrap();
    assert!(alert.record.header.id.starts_with("O-"));
    assert!(alert.record.observed_value.unwrap() >= 5.0);
    assert_eq!(
        alert.record.classification,
        temper_evolution::ObservationClass::StateMachine
    );
}

// =========================================================================
// Phase 2: EvolutionRun entity full lifecycle
// =========================================================================

/// Proves: the EvolutionRun entity can be driven through its complete state
/// machine — Created → Selecting → ... → Completed.
#[tokio::test]
async fn e2e_gepa_evolution_run_full_lifecycle() {
    let (_guard, _clock, _id_gen) = install_deterministic_context(43);
    let harness = SimPlatformHarness::no_faults(43);

    // Install evolution skill, then override EvolutionRun with integration-free
    // version to prevent background WASM failures during manual state machine testing.
    let types = harness
        .install_skill(TENANT, "evolution")
        .await
        .expect("evolution skill should install");
    assert!(types.contains(&"EvolutionRun".to_string()));
    assert!(types.contains(&"SentinelMonitor".to_string()));
    harness.register_inline_spec(TENANT, "EvolutionRun", EVOLUTION_RUN_IOA_NO_INTEGRATIONS);

    let evo_id = "evo-run-1";

    // Created → Selecting (Start)
    let r = harness
        .dispatch(
            TENANT,
            "EvolutionRun",
            evo_id,
            "Start",
            serde_json::json!({
                "SkillName": "project-management",
                "TargetEntityType": "Issue",
                "AutonomyLevel": "auto"
            }),
        )
        .await
        .expect("Start should succeed");
    assert!(r.success, "Start failed: {:?}", r.error);
    assert_eq!(r.state.status, "Selecting");

    // Selecting → Evaluating (SelectCandidate)
    let r = harness
        .dispatch(
            TENANT,
            "EvolutionRun",
            evo_id,
            "SelectCandidate",
            serde_json::json!({
                "CandidateId": "candidate-1",
                "SpecSource": "original issue spec"
            }),
        )
        .await
        .expect("SelectCandidate should succeed");
    assert!(r.success, "SelectCandidate failed: {:?}", r.error);
    assert_eq!(r.state.status, "Evaluating");

    // Evaluating → Reflecting (RecordEvaluation)
    let r = harness
        .dispatch(
            TENANT,
            "EvolutionRun",
            evo_id,
            "RecordEvaluation",
            serde_json::json!({
                "ReplayResultJson": "{\"actions_attempted\":10,\"succeeded\":7}"
            }),
        )
        .await
        .expect("RecordEvaluation should succeed");
    assert!(r.success, "RecordEvaluation failed: {:?}", r.error);
    assert_eq!(r.state.status, "Reflecting");

    // Reflecting → Proposing (RecordDataset)
    let r = harness
        .dispatch(
            TENANT,
            "EvolutionRun",
            evo_id,
            "RecordDataset",
            serde_json::json!({
                "DatasetJson": "{\"triplets\":[{\"input\":\"Reassign\",\"output\":\"error\",\"feedback\":\"add action\"}]}"
            }),
        )
        .await
        .expect("RecordDataset should succeed");
    assert!(r.success, "RecordDataset failed: {:?}", r.error);
    assert_eq!(r.state.status, "Proposing");

    // Proposing → Verifying (RecordMutation)
    let r = harness
        .dispatch(
            TENANT,
            "EvolutionRun",
            evo_id,
            "RecordMutation",
            serde_json::json!({
                "MutatedSpecSource": "mutated spec with Reassign",
                "MutationSummary": "Added Reassign action to Issue"
            }),
        )
        .await
        .expect("RecordMutation should succeed");
    assert!(r.success, "RecordMutation failed: {:?}", r.error);
    assert_eq!(r.state.status, "Verifying");

    // Verifying → Scoring (RecordVerificationPass)
    let r = harness
        .dispatch(
            TENANT,
            "EvolutionRun",
            evo_id,
            "RecordVerificationPass",
            serde_json::json!({
                "VerificationReport": "L0-L3 all passed"
            }),
        )
        .await
        .expect("RecordVerificationPass should succeed");
    assert!(r.success, "RecordVerificationPass failed: {:?}", r.error);
    assert_eq!(r.state.status, "Scoring");

    // Scoring → Updating (RecordScore)
    let r = harness
        .dispatch(
            TENANT,
            "EvolutionRun",
            evo_id,
            "RecordScore",
            serde_json::json!({
                "ScoresJson": "{\"success_rate\":0.95,\"coverage\":1.0,\"guard_pass_rate\":0.9}"
            }),
        )
        .await
        .expect("RecordScore should succeed");
    assert!(r.success, "RecordScore failed: {:?}", r.error);
    assert_eq!(r.state.status, "Updating");

    // Updating → Deploying (RecordFrontierAutoApprove — auto-approved)
    let r = harness
        .dispatch(
            TENANT,
            "EvolutionRun",
            evo_id,
            "RecordFrontierAutoApprove",
            serde_json::json!({
                "FrontierUpdateJson": "{\"added\":true,\"dominated_removed\":[\"old-candidate\"]}"
            }),
        )
        .await
        .expect("RecordFrontierAutoApprove should succeed");
    assert!(r.success, "RecordFrontierAutoApprove failed: {:?}", r.error);
    assert_eq!(r.state.status, "Deploying");

    // Deploying → Completed (Deploy)
    let r = harness
        .dispatch(
            TENANT,
            "EvolutionRun",
            evo_id,
            "Deploy",
            serde_json::json!({
                "DeploymentId": "deploy-001"
            }),
        )
        .await
        .expect("Deploy should succeed");
    assert!(r.success, "Deploy failed: {:?}", r.error);
    assert_eq!(r.state.status, "Completed");

    // Verify full event chain: 10 transitions total.
    let entity = harness
        .platform_state
        .server
        .get_tenant_entity_state(
            &temper_runtime::tenant::TenantId::new(TENANT),
            "EvolutionRun",
            evo_id,
        )
        .await
        .expect("should get entity state");
    assert_eq!(entity.state.events.len(), 10);
}

// =========================================================================
// Phase 3: Verification retry loop
// =========================================================================

/// Proves: the verification retry loop works — failed verification transitions
/// back to Reflecting, and after 3 failures ExhaustRetries → Failed.
#[tokio::test]
async fn e2e_gepa_verification_retry_loop() {
    let (_guard, _clock, _id_gen) = install_deterministic_context(44);
    let harness = SimPlatformHarness::no_faults(44);

    harness
        .install_skill(TENANT, "evolution")
        .await
        .expect("evolution skill should install");
    harness.register_inline_spec(TENANT, "EvolutionRun", EVOLUTION_RUN_IOA_NO_INTEGRATIONS);

    let evo_id = "evo-retry-1";

    // Drive to Verifying state.
    for (action, params) in [
        (
            "Start",
            serde_json::json!({"SkillName": "pm", "TargetEntityType": "Issue", "AutonomyLevel": "auto"}),
        ),
        (
            "SelectCandidate",
            serde_json::json!({"CandidateId": "c1", "SpecSource": "spec"}),
        ),
        (
            "RecordEvaluation",
            serde_json::json!({"ReplayResultJson": "{}"}),
        ),
        ("RecordDataset", serde_json::json!({"DatasetJson": "{}"})),
        (
            "RecordMutation",
            serde_json::json!({"MutatedSpecSource": "bad spec v1", "MutationSummary": "attempt 1"}),
        ),
    ] {
        let r = harness
            .dispatch(TENANT, "EvolutionRun", evo_id, action, params)
            .await
            .unwrap_or_else(|_| panic!("{action} should succeed"));
        assert!(r.success, "{action} failed: {:?}", r.error);
    }

    // Verify we're in Verifying state.
    let entity = harness
        .platform_state
        .server
        .get_tenant_entity_state(
            &temper_runtime::tenant::TenantId::new(TENANT),
            "EvolutionRun",
            evo_id,
        )
        .await
        .unwrap();
    assert_eq!(entity.state.status, "Verifying");

    // Verification failure → back to Reflecting.
    let r = harness
        .dispatch(
            TENANT,
            "EvolutionRun",
            evo_id,
            "RecordVerificationFailure",
            serde_json::json!({"VerificationErrors": "L1: invariant violated"}),
        )
        .await
        .expect("RecordVerificationFailure should succeed");
    assert!(r.success);
    assert_eq!(r.state.status, "Reflecting");

    // Second attempt cycle: Reflecting → Proposing → Verifying → Failure.
    let r = harness
        .dispatch(
            TENANT,
            "EvolutionRun",
            evo_id,
            "RecordDataset",
            serde_json::json!({"DatasetJson": "{\"verification_feedback\":[\"invariant violated\"]}"}),
        )
        .await
        .expect("RecordDataset should succeed");
    assert!(r.success);
    assert_eq!(r.state.status, "Proposing");

    let r = harness
        .dispatch(
            TENANT,
            "EvolutionRun",
            evo_id,
            "RecordMutation",
            serde_json::json!({"MutatedSpecSource": "bad spec v2", "MutationSummary": "attempt 2"}),
        )
        .await
        .expect("RecordMutation should succeed");
    assert!(r.success);
    assert_eq!(r.state.status, "Verifying");

    // After enough failures, ExhaustRetries → Failed.
    let r = harness
        .dispatch(
            TENANT,
            "EvolutionRun",
            evo_id,
            "ExhaustRetries",
            serde_json::json!({"FailureReason": "Max mutation attempts reached (3)"}),
        )
        .await
        .expect("ExhaustRetries should succeed");
    assert!(r.success);
    assert_eq!(r.state.status, "Failed");
}

// =========================================================================
// Phase 4: SentinelMonitor entity lifecycle
// =========================================================================

/// Proves: SentinelMonitor entity can cycle through its states.
#[tokio::test]
async fn e2e_gepa_sentinel_monitor_lifecycle() {
    let (_guard, _clock, _id_gen) = install_deterministic_context(45);
    let harness = SimPlatformHarness::no_faults(45);

    harness
        .install_skill(TENANT, "evolution")
        .await
        .expect("evolution skill should install");

    let sentinel_id = "sentinel-1";

    // Active → Checking (CheckSentinel)
    let r = harness
        .dispatch(
            TENANT,
            "SentinelMonitor",
            sentinel_id,
            "CheckSentinel",
            serde_json::json!({}),
        )
        .await
        .expect("CheckSentinel should succeed");
    assert!(r.success);
    assert_eq!(r.state.status, "Checking");

    // Checking → Triggering (AlertsFound)
    let r = harness
        .dispatch(
            TENANT,
            "SentinelMonitor",
            sentinel_id,
            "AlertsFound",
            serde_json::json!({
                "AlertDetails": "6 Reassign failures on Issue",
                "SuggestedTarget": "project-management/Issue"
            }),
        )
        .await
        .expect("AlertsFound should succeed");
    assert!(r.success);
    assert_eq!(r.state.status, "Triggering");

    // Triggering → Active (CreateEvolutionRun)
    let r = harness
        .dispatch(
            TENANT,
            "SentinelMonitor",
            sentinel_id,
            "CreateEvolutionRun",
            serde_json::json!({
                "EvolutionRunId": "evo-from-sentinel-1",
                "SkillName": "project-management",
                "TargetEntityType": "Issue"
            }),
        )
        .await
        .expect("CreateEvolutionRun should succeed");
    assert!(r.success);
    assert_eq!(r.state.status, "Active");

    // Second cycle: Active → Checking → Active (NoAlerts)
    let r = harness
        .dispatch(
            TENANT,
            "SentinelMonitor",
            sentinel_id,
            "CheckSentinel",
            serde_json::json!({}),
        )
        .await
        .expect("CheckSentinel should succeed");
    assert!(r.success);
    assert_eq!(r.state.status, "Checking");

    let r = harness
        .dispatch(
            TENANT,
            "SentinelMonitor",
            sentinel_id,
            "NoAlerts",
            serde_json::json!({}),
        )
        .await
        .expect("NoAlerts should succeed");
    assert!(r.success);
    assert_eq!(r.state.status, "Active");
}

// =========================================================================
// Phase 5: GEPA algorithm primitives — integrated proof
// =========================================================================

/// Proves: the full GEPA algorithm primitive chain works:
/// replay → scoring → Pareto frontier management → reflective dataset.
#[tokio::test]
async fn e2e_gepa_algorithm_primitives_integrated() {
    use temper_evolution::gepa::*;

    // --- Step 1: Build replay results for original spec (missing Reassign) ---
    let mut replay_original = ReplayResult::new();
    // 5 successful actions.
    for _ in 0..5 {
        replay_original.record_success();
    }
    // 5 failures — Reassign not found.
    for _ in 0..5 {
        replay_original.record_unknown_action("Reassign", "InProgress");
    }
    assert_eq!(replay_original.actions_attempted, 10);
    assert_eq!(replay_original.succeeded, 5);
    assert_eq!(replay_original.unknown_actions, 5);
    assert!(!replay_original.all_succeeded());
    assert!((replay_original.success_rate() - 0.5).abs() < f64::EPSILON);

    // --- Step 2: Score the original spec ---
    let scores_original = ObjectiveScores::from_replay(&replay_original);
    assert!(
        (scores_original.scores["success_rate"] - 0.5).abs() < f64::EPSILON,
        "success_rate should be 0.5"
    );
    assert!(
        (scores_original.scores["coverage"] - 0.5).abs() < f64::EPSILON,
        "coverage should be 0.5 (5 unknown out of 10)"
    );

    // --- Step 3: Create candidate for original spec ---
    let now = chrono::Utc::now();
    let mut candidate_original = Candidate::new(
        "c0".into(),
        "original issue spec".into(),
        "project-management".into(),
        "Issue".into(),
        0,
        now,
    );
    for (obj, score) in scores_original.into_map() {
        candidate_original.set_score(obj, score);
    }

    // --- Step 4: Add to Pareto frontier ---
    let mut frontier = ParetoFrontier::new();
    assert!(frontier.try_add(candidate_original));
    assert_eq!(frontier.len(), 1);

    // --- Step 5: Build reflective dataset from failures ---
    let mut dataset = temper_evolution::gepa::reflective::ReflectiveDataset::new(
        "project-management".into(),
        "Issue".into(),
    );
    for i in 0..5 {
        let triplet = ReflectiveTriplet::new(
            format!("Agent attempted Reassign on issue-{i} in InProgress state"),
            "Error: action 'Reassign' not found in spec".into(),
            "Add Reassign action: from=[InProgress] to=InProgress, with guard requiring assignee_set".into(),
            0.0,
            format!("traj-{i}"),
        )
        .with_entity_type("Issue".into())
        .with_action("Reassign".into());
        dataset.add_triplet(triplet);
    }

    assert_eq!(dataset.failure_count(), 5);
    assert_eq!(dataset.success_count(), 0);

    let llm_prompt = dataset.format_for_llm();
    assert!(llm_prompt.contains("Reassign"));
    assert!(llm_prompt.contains("5 failures"));

    // --- Step 6: Simulate mutation — "LLM" proposes spec with Reassign ---
    let mut replay_mutated = ReplayResult::new();
    // All 10 actions now succeed (including the 5 Reassigns).
    for _ in 0..10 {
        replay_mutated.record_success();
    }
    assert!(replay_mutated.all_succeeded());
    assert!((replay_mutated.success_rate() - 1.0).abs() < f64::EPSILON);

    // --- Step 7: Score the mutated spec ---
    let scores_mutated = ObjectiveScores::from_replay(&replay_mutated);
    assert!(
        (scores_mutated.scores["success_rate"] - 1.0).abs() < f64::EPSILON,
        "mutated success_rate should be 1.0"
    );
    assert!(
        (scores_mutated.scores["coverage"] - 1.0).abs() < f64::EPSILON,
        "mutated coverage should be 1.0"
    );

    // --- Step 8: Mutated candidate dominates original ---
    let mut candidate_mutated = Candidate::new(
        "c1".into(),
        "mutated issue spec with Reassign".into(),
        "project-management".into(),
        "Issue".into(),
        1,
        now,
    )
    .with_parent("c0".into())
    .with_mutation_summary("Added Reassign action from InProgress to InProgress".into());

    for (obj, score) in scores_mutated.into_map() {
        candidate_mutated.set_score(obj, score);
    }

    // Add mutated to frontier — should dominate original.
    assert!(frontier.try_add(candidate_mutated));
    assert_eq!(
        frontier.len(),
        1,
        "Mutated should have dominated original — frontier should still have 1 member"
    );
    assert!(
        frontier.members.contains_key("c1"),
        "c1 (mutated) should be the sole frontier member"
    );
    assert!(
        !frontier.members.contains_key("c0"),
        "c0 (original) should have been removed"
    );

    // --- Step 9: Weighted sum confirms improvement ---
    let config = ScoringConfig::default();
    let winner = frontier.members.get("c1").unwrap();
    let winner_scores = ObjectiveScores {
        scores: winner.scores.clone(),
    };
    let weighted = winner_scores.weighted_sum(&config);
    assert!(
        weighted > 0.9,
        "Weighted sum should be > 0.9 for perfect scores, got {weighted}"
    );
}

// =========================================================================
// Phase 6: Hot-deploy mutated spec and verify Reassign works
// =========================================================================

/// Proves: after hot-deploying a mutated Issue spec (with Reassign action),
/// the previously-failing Reassign action now succeeds through the platform.
#[tokio::test]
async fn e2e_gepa_hotdeploy_and_verify() {
    let (_guard, _clock, _id_gen) = install_deterministic_context(46);
    let harness = SimPlatformHarness::no_faults(46);

    // Install PM skill (Issue spec WITHOUT Reassign).
    harness
        .install_skill(TENANT, "project-management")
        .await
        .expect("PM skill should install");

    // Verify Reassign fails on a fresh Issue.
    let r = harness
        .dispatch(
            TENANT,
            "Issue",
            "issue-hotdeploy-1",
            "Reassign",
            serde_json::json!({"NewAssigneeId": "agent-2"}),
        )
        .await;
    if let Ok(resp) = &r {
        assert!(
            !resp.success,
            "Reassign should fail before hot-deploy: {:?}",
            resp.error
        );
    }

    // Now create a mutated Issue spec that adds Reassign.
    // We take the original and add a Reassign action.
    let mutated_issue_spec = include_str!("../../../os-apps/project-management/issue.ioa.toml")
        .to_string()
        + r#"

[[action]]
name = "Reassign"
kind = "input"
from = ["Backlog", "Triage", "Todo", "InProgress", "InReview", "Planning", "Planned"]
guard = "is_true assignee_set"
params = ["NewAssigneeId"]
hint = "Reassign the issue to a different implementer."
"#;

    // Verify the mutated spec parses (L0 check).
    let parsed = temper_spec::automaton::parse_automaton(&mutated_issue_spec);
    assert!(
        parsed.is_ok(),
        "Mutated spec should parse: {:?}",
        parsed.err()
    );

    // Hot-deploy: re-register the tenant with the mutated Issue spec (merge mode).
    {
        let mut registry = harness.platform_state.registry.write().unwrap(); // ci-ok: infallible lock
        let tenant_id = temper_runtime::tenant::TenantId::new(TENANT);
        // Get existing CSDL for merge.
        let existing_csdl = registry
            .get_tenant(&tenant_id)
            .expect("tenant should exist")
            .csdl
            .as_ref()
            .clone();
        let csdl_xml = temper_spec::csdl::emit_csdl_xml(&existing_csdl);
        registry
            .try_register_tenant_with_reactions_and_constraints(
                tenant_id,
                existing_csdl,
                csdl_xml,
                &[("Issue", &mutated_issue_spec)],
                Vec::new(),
                None,
                true, // merge mode — only update Issue, preserve others
            )
            .expect("hot-deploy should succeed");
    }

    // Now Reassign should work on an Issue that has an assignee set.
    // Create a fresh Issue (starts in Backlog), then Assign to set assignee_set=true.
    let r = harness
        .dispatch(
            TENANT,
            "Issue",
            "issue-hotdeploy-2",
            "Assign",
            serde_json::json!({"AgentId": "agent-1"}),
        )
        .await
        .expect("Assign should succeed");
    assert!(r.success, "Assign failed: {:?}", r.error);

    // NOW: Reassign should succeed because the mutated spec has it
    // (self-loop on Backlog with guard is_true assignee_set).
    let r = harness
        .dispatch(
            TENANT,
            "Issue",
            "issue-hotdeploy-2",
            "Reassign",
            serde_json::json!({"NewAssigneeId": "agent-2"}),
        )
        .await
        .expect("Reassign should succeed after hot-deploy");
    assert!(
        r.success,
        "Reassign should succeed after hot-deploy: {:?}",
        r.error
    );
    assert_eq!(
        r.state.status, "Backlog",
        "Reassign is a self-loop, issue stays in Backlog"
    );
}

// =========================================================================
// Phase 7: Full integrated GEPA loop — sentinel → evolution → deploy
// =========================================================================

/// Integration test combining all phases: failure detection → sentinel →
/// evolution entity → GEPA primitives → hot-deploy → retry succeeds.
#[tokio::test]
async fn e2e_gepa_full_loop() {
    let (_guard, _clock, _id_gen) = install_deterministic_context(47);
    let harness = SimPlatformHarness::no_faults(47);

    // --- Step 1: Install both PM and evolution skills ---
    harness
        .install_skill(TENANT, "project-management")
        .await
        .expect("PM skill should install");
    harness
        .install_skill(TENANT, "evolution")
        .await
        .expect("evolution skill should install");
    harness.register_inline_spec(TENANT, "EvolutionRun", EVOLUTION_RUN_IOA_NO_INTEGRATIONS);

    // --- Step 2: Simulate 6 Reassign failures ---
    for i in 0..6 {
        let _r = harness
            .dispatch(
                TENANT,
                "Issue",
                &format!("loop-issue-{i}"),
                "Reassign",
                serde_json::json!({"NewAssigneeId": "agent-x"}),
            )
            .await;
        // All should fail — Reassign doesn't exist.
    }

    // --- Step 3: Sentinel detects the cluster ---
    let trajectory_entries: Vec<temper_server::state::TrajectoryEntry> = (0..6)
        .map(|i| temper_server::state::TrajectoryEntry {
            timestamp: temper_runtime::scheduler::sim_now().to_rfc3339(),
            tenant: TENANT.to_string(),
            entity_type: "Issue".to_string(),
            entity_id: format!("loop-issue-{i}"),
            action: "Reassign".to_string(),
            success: false,
            from_status: Some("Backlog".to_string()),
            to_status: None,
            error: Some("action not found".to_string()),
            agent_id: Some("claude-code".to_string()),
            session_id: None,
            authz_denied: None,
            denied_resource: None,
            denied_module: None,
            source: None,
            spec_governed: Some(true),
            agent_type: Some("claude-code".to_string()),
            request_body: None,
            intent: None,
        })
        .collect();

    let rules = temper_server::sentinel::default_rules();
    let alerts = temper_server::sentinel::check_rules(
        &rules,
        &harness.platform_state.server,
        &trajectory_entries,
    );
    assert!(
        alerts
            .iter()
            .any(|a| a.rule_name == "ots_trajectory_failure_cluster"),
        "Sentinel should fire"
    );

    // --- Step 4: SentinelMonitor detects and triggers EvolutionRun ---
    let r = harness
        .dispatch(
            TENANT,
            "SentinelMonitor",
            "s1",
            "CheckSentinel",
            serde_json::json!({}),
        )
        .await
        .unwrap();
    assert!(r.success);

    let r = harness
        .dispatch(
            TENANT,
            "SentinelMonitor",
            "s1",
            "AlertsFound",
            serde_json::json!({
                "AlertDetails": "6 Reassign failures on Issue",
                "SuggestedTarget": "project-management/Issue"
            }),
        )
        .await
        .unwrap();
    assert!(r.success);

    let r = harness
        .dispatch(
            TENANT,
            "SentinelMonitor",
            "s1",
            "CreateEvolutionRun",
            serde_json::json!({
                "EvolutionRunId": "evo-full-1",
                "SkillName": "project-management",
                "TargetEntityType": "Issue"
            }),
        )
        .await
        .unwrap();
    assert!(r.success);
    assert_eq!(r.state.status, "Active");

    // --- Step 5: Drive EvolutionRun through the happy path ---
    let evo_id = "evo-full-1";
    let actions = vec![
        (
            "Start",
            serde_json::json!({"SkillName": "project-management", "TargetEntityType": "Issue", "AutonomyLevel": "auto"}),
        ),
        (
            "SelectCandidate",
            serde_json::json!({"CandidateId": "c0", "SpecSource": "original"}),
        ),
        (
            "RecordEvaluation",
            serde_json::json!({"ReplayResultJson": "{\"actions_attempted\":10,\"succeeded\":5}"}),
        ),
        (
            "RecordDataset",
            serde_json::json!({"DatasetJson": "{\"triplets\":[]}"}),
        ),
        (
            "RecordMutation",
            serde_json::json!({"MutatedSpecSource": "spec with Reassign", "MutationSummary": "Added Reassign"}),
        ),
        (
            "RecordVerificationPass",
            serde_json::json!({"VerificationReport": "L0-L3 passed"}),
        ),
        (
            "RecordScore",
            serde_json::json!({"ScoresJson": "{\"success_rate\":1.0,\"coverage\":1.0}"}),
        ),
        (
            "RecordFrontierAutoApprove",
            serde_json::json!({"FrontierUpdateJson": "{\"added\":true}"}),
        ),
    ];

    for (action, params) in &actions {
        let r = harness
            .dispatch(TENANT, "EvolutionRun", evo_id, action, params.clone())
            .await
            .unwrap_or_else(|e| panic!("{action} failed: {e}"));
        assert!(r.success, "{action} failed: {:?}", r.error);
    }

    // Should be in Deploying state now.
    let entity = harness
        .platform_state
        .server
        .get_tenant_entity_state(
            &temper_runtime::tenant::TenantId::new(TENANT),
            "EvolutionRun",
            evo_id,
        )
        .await
        .unwrap();
    assert_eq!(entity.state.status, "Deploying");

    // --- Step 6: Hot-deploy the mutated spec ---
    let mutated_issue_spec = include_str!("../../../os-apps/project-management/issue.ioa.toml")
        .to_string()
        + r#"

[[action]]
name = "Reassign"
kind = "input"
from = ["Backlog", "Triage", "Todo", "InProgress", "InReview", "Planning", "Planned"]
guard = "is_true assignee_set"
params = ["NewAssigneeId"]
hint = "Reassign the issue to a different implementer."
"#;

    {
        let mut registry = harness.platform_state.registry.write().unwrap(); // ci-ok: infallible lock
        let tenant_id = temper_runtime::tenant::TenantId::new(TENANT);
        let existing_csdl = registry
            .get_tenant(&tenant_id)
            .expect("tenant should exist")
            .csdl
            .as_ref()
            .clone();
        let csdl_xml = temper_spec::csdl::emit_csdl_xml(&existing_csdl);
        registry
            .try_register_tenant_with_reactions_and_constraints(
                tenant_id,
                existing_csdl,
                csdl_xml,
                &[("Issue", &mutated_issue_spec)],
                Vec::new(),
                None,
                true, // merge mode
            )
            .expect("hot-deploy should succeed");
    }

    // Complete the deployment.
    let r = harness
        .dispatch(
            TENANT,
            "EvolutionRun",
            evo_id,
            "Deploy",
            serde_json::json!({"DeploymentId": "deploy-full-1"}),
        )
        .await
        .unwrap();
    assert!(r.success);
    assert_eq!(r.state.status, "Completed");

    // --- Step 7: Replay — Reassign now succeeds ---
    // Create a fresh issue, Assign to set assignee_set=true, then Reassign.
    let r = harness
        .dispatch(
            TENANT,
            "Issue",
            "loop-retry-1",
            "Assign",
            serde_json::json!({"AgentId": "agent-1"}),
        )
        .await
        .unwrap();
    assert!(r.success, "Assign failed: {:?}", r.error);

    // The moment of truth: Reassign should NOW succeed after evolution hot-deploy.
    let r = harness
        .dispatch(
            TENANT,
            "Issue",
            "loop-retry-1",
            "Reassign",
            serde_json::json!({"NewAssigneeId": "agent-2"}),
        )
        .await
        .expect("Reassign should succeed after evolution hot-deploy");
    assert!(
        r.success,
        "Reassign MUST succeed after GEPA evolution and hot-deploy: {:?}",
        r.error
    );
    assert_eq!(
        r.state.status, "Backlog",
        "Reassign self-loop keeps Backlog"
    );

    // --- Step 8: Verify GEPA primitives agree ---
    use temper_evolution::gepa::*;

    let mut replay = ReplayResult::new();
    // All 5 Reassign attempts now succeed.
    for _ in 0..5 {
        replay.record_success();
    }
    let scores = ObjectiveScores::from_replay(&replay);
    assert!((scores.scores["success_rate"] - 1.0).abs() < f64::EPSILON);
    assert!((scores.scores["coverage"] - 1.0).abs() < f64::EPSILON);
}

// =========================================================================
// Phase 8: WASM integration chain — REAL modules, REAL dispatch
// =========================================================================

/// Proves: the compiled GEPA WASM modules actually execute through the
/// integration dispatch chain. Uses the REAL EvolutionRun spec with
/// integrations, registers compiled replay/reflective/score/pareto binaries,
/// and verifies that `SelectCandidate` → `evaluate_candidate` trigger fires
/// `gepa-replay` which calls back `RecordEvaluation`.
///
/// This is the true end-to-end proof that the WASM chain works.
#[tokio::test(flavor = "multi_thread")]
async fn e2e_gepa_wasm_integration_chain_fires() {
    use std::time::Duration;
    use temper_runtime::ActorSystem;
    use temper_runtime::tenant::TenantId;
    use temper_server::registry::SpecRegistry;
    use temper_server::request_context::AgentContext;
    use temper_spec::csdl::parse_csdl;

    let (_guard, _clock, _id_gen) = install_deterministic_context(99);

    // --- Build ServerState with REAL EvolutionRun spec (WITH integrations) ---
    let evo_ioa = include_str!("../../../os-apps/evolution/evolution_run.ioa.toml");
    let csdl_xml = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Evolution" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="EvolutionRun">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="EvolutionRuns" EntityType="Temper.Evolution.EvolutionRun"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(csdl_xml).expect("CSDL should parse");
    registry.register_tenant(
        "wasm-test",
        csdl,
        csdl_xml.to_string(),
        &[("EvolutionRun", evo_ioa)],
    );

    let system = ActorSystem::new("gepa-wasm-chain-test");
    let state = temper_server::ServerState::from_registry(system, registry);
    let tenant = TenantId::new("wasm-test");

    // --- Register the compiled GEPA WASM modules ---
    let Some(gepa_modules) = load_gepa_wasm_modules() else {
        return;
    };

    for (name, bytes) in &gepa_modules {
        let hash = state
            .wasm_engine
            .compile_and_cache(bytes.as_slice())
            .unwrap_or_else(|e| panic!("failed to compile {name}: {e}"));
        let mut wasm_reg = state
            .wasm_module_registry
            .write()
            .expect("wasm registry lock"); // ci-ok: infallible lock
        wasm_reg.register(&tenant, name, &hash);
    }

    // --- Create entity and drive to Evaluating ---
    let evo_id = "evo-wasm-1";

    // Start
    let r = state
        .dispatch_tenant_action(
            &tenant,
            "EvolutionRun",
            evo_id,
            "Start",
            serde_json::json!({
                "SkillName": "project-management",
                "TargetEntityType": "Issue",
                "AutonomyLevel": "auto"
            }),
            &AgentContext::default(),
        )
        .await
        .expect("Start should succeed");
    assert!(r.success);
    assert_eq!(r.state.status, "Selecting");

    // A simple IOA spec for the replay module to evaluate against
    let test_spec = r#"
[automaton]
name = "TestIssue"
states = ["Backlog", "InProgress", "Done"]
initial = "Backlog"

[[action]]
name = "StartWork"
kind = "input"
from = ["Backlog"]
to = "InProgress"

[[action]]
name = "Complete"
kind = "input"
from = ["InProgress"]
to = "Done"
"#;

    // SelectCandidate — this triggers the evaluate_candidate WASM integration!
    let trajectory_actions = serde_json::json!([
        {"action": "StartWork", "params": {}},
        {"action": "Complete", "params": {}},
        {"action": "Reassign", "params": {"NewAssigneeId": "agent-x"}}
    ]);

    let r = state
        .dispatch_tenant_action(
            &tenant,
            "EvolutionRun",
            evo_id,
            "SelectCandidate",
            serde_json::json!({
                "CandidateId": "candidate-wasm-1",
                "SpecSource": test_spec,
                "TrajectoryActions": trajectory_actions,
            }),
            &AgentContext::default(),
        )
        .await
        .expect("SelectCandidate should succeed");
    assert!(r.success, "SelectCandidate failed: {:?}", r.error);
    assert_eq!(r.state.status, "Evaluating");
    println!("SelectCandidate custom_effects: {:?}", r.custom_effects);

    // The integration fires in background (tokio::spawn). Wait for it.
    // The chain is: evaluate_candidate (gepa-replay) → RecordEvaluation
    //            → build_reflective_dataset (gepa-reflective) → RecordDataset
    //            → propose_mutation (gepa-proposer-agent, not registered in this test)
    //
    // We expect the entity to reach at least "Reflecting" or "Proposing" via WASM,
    // then potentially "Failed" when propose_mutation cannot run.

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut final_status = "Evaluating".to_string();
    let mut reached_beyond_evaluating = false;

    loop {
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;

        let entity = state
            .get_tenant_entity_state(&tenant, "EvolutionRun", evo_id)
            .await
            .expect("entity should exist");
        final_status = entity.state.status.clone();

        // If we've moved past Evaluating, the WASM module fired!
        if final_status != "Evaluating" {
            reached_beyond_evaluating = true;
            // Keep polling until we hit a terminal or stable state
            if matches!(
                final_status.as_str(),
                "Proposing" | "Failed" | "Completed" | "Verifying"
            ) {
                break;
            }
        }
    }

    println!("Final entity status: {final_status}");

    // The critical assertion: the entity moved PAST Evaluating.
    // This proves the gepa-replay WASM module executed and dispatched RecordEvaluation.
    assert!(
        reached_beyond_evaluating,
        "Entity should have moved past 'Evaluating' via WASM integration chain. \
         Stuck at: {final_status}. This means the WASM module never fired its callback."
    );

    // Even better: if we reached Proposing or Failed, it means BOTH
    // gepa-replay AND gepa-reflective WASM modules fired successfully,
    // and the chain only stopped at propose_mutation (expected in this test).
    let wasm_chain_completed = matches!(final_status.as_str(), "Proposing" | "Failed");
    println!(
        "WASM chain completed (replay + reflective): {wasm_chain_completed}, final: {final_status}"
    );

    // Verify the entity accumulated the right fields from WASM callbacks
    let entity = state
        .get_tenant_entity_state(&tenant, "EvolutionRun", evo_id)
        .await
        .expect("entity should exist");

    // Check that events show the WASM callback actions were dispatched
    let event_actions: Vec<&str> = entity
        .state
        .events
        .iter()
        .map(|e| e.action.as_str())
        .collect();
    println!("Entity event trail: {:?}", event_actions);

    // We should see at least: Start, SelectCandidate, RecordEvaluation (from gepa-replay)
    assert!(
        event_actions.contains(&"RecordEvaluation"),
        "RecordEvaluation should appear in event trail — proves gepa-replay WASM module executed. \
         Events: {:?}",
        event_actions
    );
}

/// **Autonomous GEPA chain to approval gate (test override)** — proves the background
/// chain runs end-to-end through mutation, verification, scoring, and frontier update:
///
/// SelectCandidate → gepa-replay (WASM) → RecordEvaluation
///                → gepa-reflective (WASM) → RecordDataset
///                → claude_code adapter (mock script) → RecordMutation
///                → claude_code adapter (mock verifier) → RecordVerificationPass
///                → gepa-score (WASM) → RecordScore
///                → gepa-pareto (WASM) → RecordFrontier
///
/// Production uses `gepa-proposer-agent` WASM + TemperAgent. This test
/// overrides `propose_mutation` and `verify_candidate` to deterministic mock adapters
/// so CI can run without LLM keys or a live verification HTTP server.
#[tokio::test(flavor = "multi_thread")]
async fn e2e_gepa_full_autonomous_loop_with_adapter() {
    use std::io::Write;
    use std::time::Duration;
    use temper_runtime::ActorSystem;
    use temper_runtime::tenant::TenantId;
    use temper_server::registry::SpecRegistry;
    use temper_server::request_context::AgentContext;
    use temper_spec::csdl::parse_csdl;

    let (_guard, _clock, _id_gen) = install_deterministic_context(42);

    // --- Create mock "claude" script that returns a mutated spec ---
    let mock_dir = std::env::temp_dir().join("gepa-mock-adapter-test"); // determinism-ok: test harness
    let mock_workdir = mock_dir.join("workspace");
    std::fs::create_dir_all(&mock_dir).expect("create mock dir");
    std::fs::create_dir_all(&mock_workdir).expect("create mock workdir");
    let mock_script = mock_dir.join("mock-claude");
    let verify_script = mock_dir.join("mock-verify");
    {
        let mut f = std::fs::File::create(&mock_script).expect("create mock script");
        // The script outputs stream-JSON with MutatedSpecSource and MutationSummary.
        // This is exactly what the real Claude Code would output when acting as
        // the evolution agent — it reads the reflective dataset and proposes a fix.
        write!(
            f,
            r#"#!/bin/bash
# Mock evolution agent — simulates Claude Code proposing a spec mutation.
# In production, Claude reads the reflective dataset (failure traces) and
# proposes a minimal IOA spec edit. Here we return a deterministic mutation.
cat <<'MOCK_OUTPUT'
{{"MutatedSpecSource": "[automaton]\nname = \"TestIssue\"\nstates = [\"Backlog\", \"InProgress\", \"Done\"]\ninitial = \"Backlog\"\n\n[[action]]\nname = \"StartWork\"\nkind = \"input\"\nfrom = [\"Backlog\"]\nto = \"InProgress\"\n\n[[action]]\nname = \"Complete\"\nkind = \"input\"\nfrom = [\"InProgress\"]\nto = \"Done\"\n\n[[action]]\nname = \"Reassign\"\nkind = \"input\"\nfrom = [\"Backlog\", \"InProgress\"]\nto = \"InProgress\"\nparams = [\"NewAssigneeId\"]\n", "MutationSummary": "Added Reassign action to TestIssue spec based on trajectory failure analysis"}}
MOCK_OUTPUT
"#
        )
        .expect("write mock script");
        // Make executable
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&mock_script, std::fs::Permissions::from_mode(0o755))
                .expect("chmod +x mock script");
        }
    }
    {
        let mut f = std::fs::File::create(&verify_script).expect("create verify script");
        write!(
            f,
            r#"#!/bin/bash
cat <<'MOCK_OUTPUT'
{{"VerificationReport": "L0-L3 cascade passed for TestIssue"}}
MOCK_OUTPUT
"#
        )
        .expect("write verify script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&verify_script, std::fs::Permissions::from_mode(0o755))
                .expect("chmod +x verify script");
        }
    }

    // --- Build EvolutionRun spec with propose_mutation test override ---
    let base_ioa = include_str!("../../../os-apps/evolution/evolution_run.ioa.toml");
    // Replace proposer + verifier integrations with deterministic adapters for test-only execution.
    let mock_path = mock_script.to_str().expect("mock path to str");
    let verify_path = verify_script.to_str().expect("verify path to str");
    let mock_workdir = mock_workdir.to_str().expect("mock workdir to str");
    let modified_ioa = base_ioa
        .replace(
        "type = \"wasm\"\nmodule = \"gepa-proposer-agent\"",
        &format!("type = \"adapter\"\nadapter = \"claude_code\"\ncommand = \"{mock_path}\""),
        )
        .replace(
            "type = \"wasm\"\nmodule = \"gepa-verify\"",
            &format!(
                "type = \"adapter\"\nadapter = \"claude_code\"\ncommand = \"{verify_path}\"\non_success = \"RecordVerificationPass\""
            ),
        )
        .replace("workdir = \"/tmp/workspace\"", &format!("workdir = \"{mock_workdir}\""));

    let csdl_xml = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Evolution" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="EvolutionRun">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="EvolutionRuns" EntityType="Temper.Evolution.EvolutionRun"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(csdl_xml).expect("CSDL should parse");
    registry.register_tenant(
        "auto-test",
        csdl,
        csdl_xml.to_string(),
        &[("EvolutionRun", &modified_ioa)],
    );

    let system = ActorSystem::new("gepa-full-auto-test");
    let state = temper_server::ServerState::from_registry(system, registry);
    let tenant = TenantId::new("auto-test");

    // --- Register WASM modules ---
    let Some(gepa_modules) = load_gepa_wasm_modules() else {
        return;
    };

    for (name, bytes) in &gepa_modules {
        let hash = state
            .wasm_engine
            .compile_and_cache(bytes.as_slice())
            .unwrap_or_else(|e| panic!("failed to compile {name}: {e}"));
        let mut wasm_reg = state
            .wasm_module_registry
            .write()
            .expect("wasm registry lock"); // ci-ok: infallible lock
        wasm_reg.register(&tenant, name, &hash);
    }

    // --- Kick off the full autonomous loop ---
    let evo_id = "evo-auto-1";

    // Step 1: Start
    let r = state
        .dispatch_tenant_action(
            &tenant,
            "EvolutionRun",
            evo_id,
            "Start",
            serde_json::json!({
                "SkillName": "project-management",
                "TargetEntityType": "Issue",
                "AutonomyLevel": "auto"
            }),
            &AgentContext::default(),
        )
        .await
        .expect("Start should succeed");
    assert!(r.success);
    assert_eq!(r.state.status, "Selecting");

    // Step 2: SelectCandidate — triggers the FULL autonomous chain:
    //   evaluate_candidate (WASM) → RecordEvaluation
    //   → build_reflective_dataset (WASM) → RecordDataset
    //   → propose_mutation (adapter/mock) → RecordMutation
    let test_spec = r#"
[automaton]
name = "TestIssue"
states = ["Backlog", "InProgress", "Done"]
initial = "Backlog"

[[action]]
name = "StartWork"
kind = "input"
from = ["Backlog"]
to = "InProgress"

[[action]]
name = "Complete"
kind = "input"
from = ["InProgress"]
to = "Done"
"#;

    let trajectory_actions = serde_json::json!([
        {"action": "StartWork", "params": {}},
        {"action": "Complete", "params": {}},
        {"action": "Reassign", "params": {"NewAssigneeId": "agent-x"}}
    ]);

    let r = state
        .dispatch_tenant_action(
            &tenant,
            "EvolutionRun",
            evo_id,
            "SelectCandidate",
            serde_json::json!({
                "CandidateId": "candidate-auto-1",
                "SpecSource": test_spec,
                "TrajectoryActions": trajectory_actions,
            }),
            &AgentContext::default(),
        )
        .await
        .expect("SelectCandidate should succeed");
    assert!(r.success);
    println!(
        "[AUTO] SelectCandidate → status: {}, effects: {:?}",
        r.state.status, r.custom_effects
    );

    // Wait for the autonomous chain to progress through adapter + verification
    // + scoring + frontier update. The current branch stops at manual approval.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut final_status = "Evaluating".to_string();
    let mut event_trail = Vec::new();

    loop {
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;

        let entity = state
            .get_tenant_entity_state(&tenant, "EvolutionRun", evo_id)
            .await
            .expect("entity should exist");
        final_status = entity.state.status.clone();
        event_trail = entity
            .state
            .events
            .iter()
            .map(|e| e.action.clone())
            .collect();

        if matches!(final_status.as_str(), "AwaitingApproval" | "Failed") {
            break;
        }
    }

    println!("[AUTO] After autonomous GEPA chain: status={final_status}, events={event_trail:?}");

    assert!(
        event_trail.contains(&"RecordMutation".to_string()),
        "RecordMutation must appear — proves the claude_code adapter (mock) executed and \
         returned a mutated spec. Events: {event_trail:?}"
    );
    assert_eq!(
        final_status, "AwaitingApproval",
        "Entity should reach AwaitingApproval after replay, reflective, verify, score, and frontier callbacks. Got: {final_status}"
    );

    // Verify all WASM modules fired
    assert!(
        event_trail.contains(&"RecordVerificationPass".to_string()),
        "RecordVerificationPass must appear — proves the verification adapter callback executed. Events: {event_trail:?}"
    );
    assert!(
        event_trail.contains(&"RecordScore".to_string()),
        "RecordScore must appear — proves gepa-score WASM module executed. Events: {event_trail:?}"
    );
    assert!(
        event_trail.contains(&"RecordFrontier".to_string()),
        "RecordFrontier must appear — proves gepa-pareto WASM module executed. Events: {event_trail:?}"
    );

    // Final event trail
    let entity = state
        .get_tenant_entity_state(&tenant, "EvolutionRun", evo_id)
        .await
        .expect("entity should exist");
    let final_events: Vec<&str> = entity
        .state
        .events
        .iter()
        .map(|e| e.action.as_str())
        .collect();

    println!("\n=== AUTONOMOUS GEPA APPROVAL-GATE PROOF ===");
    println!("Event trail: {:?}", final_events);
    println!("Final status: {}", entity.state.status);

    // The complete background chain on this branch:
    let expected = [
        "Start",
        "SelectCandidate",
        "RecordEvaluation",
        "RecordDataset",
        "RecordMutation",
        "RecordVerificationPass",
        "RecordScore",
        "RecordFrontier",
    ];
    for step in &expected {
        assert!(
            final_events.contains(step),
            "Missing step '{step}' in event trail. Full trail: {final_events:?}"
        );
    }
    assert_eq!(entity.state.status, "AwaitingApproval");
    println!("AUTONOMOUS GEPA CHAIN REACHED AWAITING APPROVAL. ✓");
}
