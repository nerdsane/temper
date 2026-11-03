//! Regressions for reaches witnesses, waiting inputs, and later obligations.

use temper_verify::simulation::{SimConfig, run_multi_seed_simulation_from_ioa};
use temper_verify::{CascadeLevel, LevelResult, VerificationCascade};

fn l1(spec: &str) -> LevelResult {
    VerificationCascade::from_ioa(spec)
        .with_sim_seeds(3)
        .with_prop_test_cases(20)
        .run()
        .level_result(CascadeLevel::ModelCheck)
        .cloned()
        .expect("L1 ran")
}

const PREFIX_TARGET: &str = r#"
[automaton]
name = "PrefixTarget"
states = ["A", "Target", "Review", "Stop"]
initial = "A"
[[action]]
name = "Visit"
from = ["A"]
to = "Target"
[[action]]
name = "Review"
from = ["Target"]
to = "Review"
[[action]]
name = "Return"
from = ["Review"]
to = "A"
[[action]]
name = "Stop"
from = ["A"]
to = "Stop"
[[liveness]]
name = "ReviewReaches"
from = ["Review"]
reaches = ["Target"]
"#;

#[test]
fn a_target_in_the_prefix_does_not_close_a_failing_loop() {
    let result = l1(PREFIX_TARGET);
    let evidence = result.verification.as_ref().unwrap();
    let broken = evidence
        .counterexamples
        .iter()
        .find(|c| c.violated == ["ReviewReaches"])
        .expect("broken rule");
    let places: Vec<_> = broken
        .trace
        .iter()
        .map(|(s, _)| s.status.as_str())
        .collect();
    assert_eq!(places, ["A", "Target", "Review", "A", "Stop"]);
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.contains("this path stops")),
        "{:?}",
        result.diagnostics
    );
}

#[test]
fn a_loop_closes_at_its_suffix_visit_instead_of_its_prefix_visit() {
    let spec = PREFIX_TARGET.replace(
        "[[liveness]]",
        r#"
[[action]]
name = "Back"
from = ["Stop"]
to = "A"
[[liveness]]"#,
    );
    let result = l1(&spec);
    let evidence = result.verification.as_ref().unwrap();
    let broken = evidence
        .counterexamples
        .iter()
        .find(|c| c.violated == ["ReviewReaches"])
        .expect("broken rule");
    let places: Vec<_> = broken
        .trace
        .iter()
        .map(|(s, _)| s.status.as_str())
        .collect();
    assert_eq!(places, ["A", "Target", "Review", "A", "Stop", "A"]);
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.contains("comes back to step 3")),
        "{:?}",
        result.diagnostics
    );
}

const IMPOSSIBLE_INPUT: &str = r#"
[automaton]
name = "ImpossibleInput"
states = ["Draft", "Published"]
initial = "Draft"
[[state]]
name = "token"
type = "string"
initial = ""
[[action]]
name = "Publish"
kind = "input"
from = ["Draft"]
to = "Published"
guard = "File[file_id].status in ['Ready']"
params = [{ name = "token", type = "string" }]
[[action.constraints]]
kind = "param_nonempty"
param = "token"
[[action]]
name = "Finish"
kind = "input"
from = ["Draft", "Published"]
to = "Published"
params = [{ name = "proof", type = "string" }]
[[action.constraints]]
kind = "param_nonempty"
param = "proof"
[[action.constraints]]
kind = "param_equals_field"
param = "proof"
field = "token"
[[invariant]]
name = "TrackToken"
assert = "token == '' || token != ''"
[[liveness]]
name = "GetsPublished"
from = ["Draft"]
reaches = ["Published"]
"#;

#[test]
fn an_impossible_input_does_not_hide_waiting_for_a_gate() {
    let result = l1(IMPOSSIBLE_INPUT);
    assert!(!result.passed, "{}", result.summary);
    assert!(result.summary.contains("not proved"), "{}", result.summary);
    let evidence = result.verification.as_ref().unwrap();
    assert!(evidence.is_complete);
    assert!(evidence.counterexamples.is_empty());
}

const REPUBLISH: &str = r#"
[automaton]
name = "Republish"
states = ["New", "Draft", "Published", "Archived"]
initial = "New"
[[state]]
name = "used"
type = "bool"
initial = false
[[action]]
name = "Begin"
from = ["New"]
to = "Draft"
[[action]]
name = "Publish"
from = ["Draft"]
to = "Published"
guard = "!used"
effect = ["used = true"]
[[action]]
name = "Archive"
from = ["Published"]
to = "Archived"
[[action]]
name = "Revise"
from = ["Archived"]
to = "Draft"
[[liveness]]
name = "GetsPublished"
from = ["Draft"]
reaches = ["Published"]
[[liveness]]
name = "GetsArchived"
from = ["Published"]
reaches = ["Archived"]
[[liveness]]
name = "AlreadyArchived"
from = ["Archived"]
reaches = ["Archived"]
"#;

#[test]
fn an_earlier_target_does_not_satisfy_a_later_simulation_obligation() {
    for initial in ["New", "Draft"] {
        let spec = REPUBLISH.replace("initial = \"New\"", &format!("initial = \"{initial}\""));
        let results = run_multi_seed_simulation_from_ioa(&spec, &SimConfig::default(), 20).unwrap();
        for result in results {
            for (_, state) in &result.actor_final_states {
                assert_eq!(state.status, "Draft", "seed {}", result.seed);
                assert_eq!(state.booleans.get("used"), Some(&true));
            }
            assert!(
                result
                    .liveness_violations
                    .iter()
                    .all(|v| v.property == "GetsPublished")
            );
            assert_eq!(
                result.liveness_violations.len(),
                3,
                "seed {}: {:?}",
                result.seed,
                result.liveness_violations
            );
        }
    }
}

#[test]
fn reaching_a_target_satisfies_only_obligations_already_started() {
    for from in ["from = [\"Draft\"]\n", ""] {
        let spec = REPUBLISH
            .replace("from = [\"Draft\"]\nreaches", &format!("{from}reaches"))
            .replace("from = [\"Archived\"]", "from = [\"New\"]");
        let results = run_multi_seed_simulation_from_ioa(&spec, &SimConfig::default(), 20).unwrap();
        for result in results {
            assert!(
                result.liveness_violations.is_empty(),
                "seed {}: {:?}",
                result.seed,
                result.liveness_violations
            );
        }
    }
}

#[test]
fn a_rule_without_from_does_not_restart_when_the_initial_place_is_revisited() {
    let spec = REPUBLISH
        .replace("initial = \"New\"", "initial = \"Draft\"")
        .replace("from = [\"Draft\"]\nreaches", "reaches");
    for result in run_multi_seed_simulation_from_ioa(&spec, &SimConfig::default(), 20).unwrap() {
        assert!(
            result.liveness_violations.is_empty(),
            "seed {}: {:?}",
            result.seed,
            result.liveness_violations
        );
        assert!(
            result
                .actor_final_states
                .iter()
                .all(|(_, s)| s.status == "Draft" && s.booleans["used"])
        );
    }
}
