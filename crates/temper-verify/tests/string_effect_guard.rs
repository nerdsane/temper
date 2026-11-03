//! A recovery literal may rely on its transition guard to establish equality.
use temper_verify::model::builder::build_model_from_ioa;
use temper_verify::{VerificationCascade, checker::check_model, smt::verify_symbolic};

const SPEC: &str = r#"
[automaton]
name = "GuardedStringRecovery"
states = ["Ready", "Failed", "Recovered"]
initial = "Ready"
terminal = ["Recovered"]

[[state]]
name = "error"
type = "string"
initial = ""

[[state]]
name = "reason"
type = "string"
initial = "initial"

[[action]]
name = "FailEmpty"
from = ["Ready"]
to = "Failed"
effect = ["reason = ''"]

[[action]]
name = "FailStale"
from = ["Ready"]
to = "Failed"
effect = ["reason = 'stale'"]

[[action]]
name = "FailRecoverable"
from = ["Ready"]
to = "Failed"
effect = ["reason = 'recovered'"]

[[action]]
name = "Recover"
from = ["Failed"]
to = "Recovered"
guard = "reason == 'recovered'"
effect = ["error = 'recovered'"]

[[invariant]]
name = "RecoveryMatchesReason"
assert = "status in ['Recovered'] => error == reason"
"#;

#[test]
fn guarded_literal_establishes_string_equality() {
    let model = check_model(&build_model_from_ioa(SPEC, 2).unwrap());
    assert!(model.all_properties_hold && model.is_complete, "{model:#?}");
    assert_eq!(model.states_explored, 5);
    let smt = verify_symbolic(SPEC, 2);
    assert!(smt.all_passed, "{smt:#?}");
    assert!(
        smt.inductive_invariants
            .iter()
            .any(|(name, proved)| name == "RecoveryMatchesReason" && *proved),
        "{smt:#?}"
    );
    let cascade = VerificationCascade::from_ioa(SPEC)
        .with_sim_seeds(20)
        .with_prop_test_cases(100)
        .run();
    assert!(cascade.all_passed, "{cascade:#?}");
}

#[test]
fn missing_guard_or_stale_literal_breaks_string_equality() {
    for broken in [
        SPEC.replace("guard = \"reason == 'recovered'\"", ""),
        SPEC.replace("error = 'recovered'", "error = 'stale'"),
    ] {
        let smt = verify_symbolic(&broken, 2);
        assert!(
            smt.inductive_invariants
                .iter()
                .any(|(name, proved)| name == "RecoveryMatchesReason" && !proved),
            "{smt:#?}"
        );
        let model = check_model(&build_model_from_ioa(&broken, 2).unwrap());
        assert!(!model.all_properties_hold, "{model:#?}");
        assert!(
            model
                .counterexamples
                .iter()
                .any(|c| c.property == "Invariants")
        );
        let cascade = VerificationCascade::from_ioa(&broken).run();
        assert!(!cascade.all_passed, "{cascade:#?}");
    }
}
