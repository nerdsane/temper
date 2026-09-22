//! System One verification treats model judgments as environmental inputs.

use stateright::Model;
use temper_verify::{VerificationCascade, build_model_from_ioa, check_model, verify_symbolic};

fn spec(assertion: &str) -> String {
    spec_for(
        r#"{ type = "noul", instructions = "Did the customer request a human?" }"#,
        assertion,
    )
}

fn spec_for(question: &str, assertion: &str) -> String {
    let assertion = serde_json::to_string(assertion).unwrap();
    format!(
        r#"
[automaton]
name = "SemanticGuard"
states = ["Open", "Escalated"]
initial = "Open"

[[action]]
name = "Escalate"
from = ["Open"]
to = "Escalated"
guard = [{{ type = "system_one", model = "jev-latest", state = "Please connect me with a human.", questions = {{ human_requested = {question} }}, assert = {assertion} }}]
"#
    )
}

#[test]
fn semantic_guard_is_possible_but_not_locally_guaranteed() {
    let model = build_model_from_ioa(&spec("answers.human_requested.noul >= 0.8"), 2)
        .expect("System One spec should parse");
    let state = model.init_states().remove(0);
    let guard = &model.transitions[0].guard;
    let mut actions = Vec::new();
    model.actions(&state, &mut actions);
    assert_eq!(actions.len(), 1);
    assert!(guard.contains_system_one());
    let result = check_model(&model);
    assert!(result.all_properties_hold, "{result:?}");
    assert_eq!(result.states_explored, 2);
    assert!(result.dead_transitions.is_empty());
    assert!(!result.external_guard_assumptions.is_empty());
}

#[test]
fn successful_judgment_cannot_whitewash_a_false_local_precondition() {
    let source = spec("answers.human_requested.noul >= 0.8").replace(
        "guard = [",
        "guard = [{ type = \"is_true\", var = \"approved\" },",
    );
    let source = source.replace(
        "[[action]]",
        "[[state]]\nname = \"approved\"\ntype = \"bool\"\ninitial = \"false\"\n\n[[action]]",
    );
    let model = build_model_from_ioa(&source, 2).unwrap();
    let state = model.init_states().remove(0);
    let mut actions = Vec::new();
    model.actions(&state, &mut actions);
    assert!(actions.is_empty());
    assert!(!check_model(&model).all_properties_hold);
}

#[test]
fn possible_judgment_outcome_does_not_hide_a_safety_violation() {
    let source = format!(
        "{}\n[[invariant]]\nname = \"NeverEscalated\"\nassert = \"never(Escalated)\"\n",
        spec("answers.human_requested.noul >= 0.8")
    );
    let model = build_model_from_ioa(&source, 2).unwrap();
    let result = check_model(&model);
    assert!(!result.all_properties_hold, "{result:?}");
    assert!(result.counterexamples.iter().any(|example| {
        example
            .trace
            .iter()
            .any(|(state, _)| state.status == "Escalated")
    }));
    let symbolic = verify_symbolic(&source, 2);
    assert!(!symbolic.all_passed, "{symbolic:?}");
    assert!(!symbolic.unreachable_states.contains(&"Escalated".into()));
}

#[test]
fn contradictory_predicates_over_one_answer_are_dead() {
    let source = spec("answers.human_requested.noul >= 0.8 && answers.human_requested.noul < 0.2");
    let model = build_model_from_ioa(&source, 2).unwrap();
    let state = model.init_states().remove(0);
    let mut actions = Vec::new();
    model.actions(&state, &mut actions);
    assert!(actions.is_empty());
    let symbolic = verify_symbolic(&source, 2);
    assert_eq!(symbolic.guard_satisfiability, [("Escalate".into(), false)]);
    assert!(!symbolic.all_passed);
    assert!(!check_model(&model).dead_transitions.is_empty());
}

#[test]
fn choice_and_score_assertions_preserve_scalar_domain_consistency() {
    let choice = r#"{ type = "choice", instructions = "Select a department.", criteria = { technical = "Product bugs", billing = "Payment problems" } }"#;
    let score = r#"{ type = "score", instructions = "Evaluate urgency.", criteria = ["Low", "Moderate", "High"] }"#;
    for (question, assertion, expected) in [
        (
            choice,
            r#"answers.human_requested.choice == "technical" && answers.human_requested.confidence >= 0.7"#,
            true,
        ),
        (
            choice,
            r#"answers.human_requested.choice == "technical" && answers.human_requested.choice == "billing""#,
            false,
        ),
        (
            score,
            "answers.human_requested.score >= 1.5 && answers.human_requested.confidence >= 0.7",
            true,
        ),
        (
            score,
            "answers.human_requested.score >= 1.5 && answers.human_requested.score < 1.5",
            false,
        ),
        (score, "answers.human_requested.score > 2", false),
    ] {
        let source = spec_for(question, assertion);
        let symbolic = verify_symbolic(&source, 2);
        assert_eq!(
            symbolic.guard_satisfiability,
            [("Escalate".into(), expected)],
            "{assertion}"
        );
        let model = build_model_from_ioa(&source, 2).unwrap();
        let mut actions = Vec::new();
        model.actions(&model.init_states()[0], &mut actions);
        assert_eq!(!actions.is_empty(), expected, "{assertion}");
    }
}

#[test]
fn system_one_waiting_state_remains_locally_terminal() {
    let source = format!(
        "{}\n[[invariant]]\nname = \"WaitingForJudgment\"\nwhen = [\"Open\"]\nassert = \"no_further_transitions\"\n",
        spec("answers.human_requested.noul >= 0.8")
    );
    assert!(check_model(&build_model_from_ioa(&source, 2).unwrap()).all_properties_hold);
    assert!(verify_symbolic(&source, 2).all_passed);
}

#[test]
fn provider_success_is_not_assumed_for_local_deadlock_freedom() {
    let source = format!(
        "{}\n[[liveness]]\nname = \"AlwaysEnabled\"\nfrom = [\"Open\"]\nhas_actions = true\n",
        spec("answers.human_requested.noul >= 0.8")
    );
    let result = check_model(&build_model_from_ioa(&source, 2).unwrap());
    assert!(!result.all_properties_hold);
    assert!(
        result
            .counterexamples
            .iter()
            .any(|example| example.property == "NoDeadlock")
    );
}

#[test]
fn successful_model_check_reports_external_liveness_assumptions() {
    let source = format!(
        "{}\n[[liveness]]\nname = \"EventuallyEscalated\"\nfrom = [\"Open\"]\nreaches = [\"Escalated\"]\n",
        spec("answers.human_requested.noul >= 0.8")
    );
    let model = build_model_from_ioa(&source, 2).unwrap();
    let result = check_model(&model);
    assert!(
        result
            .external_guard_assumptions
            .iter()
            .any(|note| { note.contains("liveness") && note.contains("not established") })
    );
    let cascade = VerificationCascade::from_ioa(&source)
        .with_sim_seeds(2)
        .with_prop_test_cases(10)
        .run();
    assert!(
        cascade
            .warnings
            .iter()
            .any(|note| note.contains("liveness"))
    );
    assert!(
        cascade
            .warnings
            .iter()
            .any(|note| note.contains("model accuracy"))
    );
    let symbolic = cascade.levels[0].smt.as_ref().unwrap();
    assert!(symbolic.approximate);
    assert!(
        symbolic
            .approximation_notes
            .iter()
            .any(|note| note.contains("System One"))
    );
    let simulation =
        temper_verify::run_simulation_from_ioa(&source, &temper_verify::SimConfig::default())
            .unwrap();
    assert!(
        simulation
            .external_guard_assumptions
            .iter()
            .any(|note| note.contains("liveness") && note.contains("not established"))
    );
}
