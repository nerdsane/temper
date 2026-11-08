//! A parameter bound to `authenticated_subject` holds the caller's id, which
//! the kernel never lets be empty: the levels explore every other value.

use temper_verify::checker::check_model;
use temper_verify::model::build_model_from_ioa;
use temper_verify::{CascadeLevel, VerificationCascade};

const CHORE: &str = r#"
[automaton]
name = "Chore"
states = ["New", "Open", "Done"]
initial = "New"
strict_action_params = true
allow_indefinite_states = ["New", "Open", "Done"]

[[state]]
name = "created_by"
type = "string"
initial = ""

[[action]]
name = "Add"
from = ["New"]
to = "Open"
params = [{ name = "created_by", type = "string", source = "authenticated_subject" }]

[[action]]
name = "Finish"
from = ["Open"]
to = "Done"

[[invariant]]
name = "a_done_chore_has_its_adder"
assert = "status != 'Done' || created_by != ''"
"#;

#[test]
fn a_field_the_caller_fills_is_never_empty_on_every_level() {
    let result = VerificationCascade::from_ioa(CHORE)
        .with_sim_seeds(10)
        .with_prop_test_cases(200)
        .run();
    for level in [
        CascadeLevel::SymbolicVerification,
        CascadeLevel::ModelCheck,
        CascadeLevel::Simulation,
        CascadeLevel::PropertyTest,
    ] {
        assert!(
            result.level_result(level).is_some_and(|level| level.passed),
            "{result:#?}"
        );
    }
}

#[test]
fn every_value_the_kernel_can_write_is_still_explored() {
    for (what, spec) in [
        (
            "a whitespace-only id, which the kernel lets through",
            CHORE.replace("created_by != ''", "created_by != ' '"),
        ),
        (
            "any one caller's id",
            CHORE.replace("created_by != ''", "created_by != 'alice'"),
        ),
        (
            "an agent label, which is empty for a person",
            CHORE.replace("authenticated_subject", "authenticated_agent_label"),
        ),
    ] {
        let result = check_model(&build_model_from_ioa(&spec, 2).expect("the spec builds"));
        assert!(
            result
                .counterexamples
                .iter()
                .any(|c| c.property == "Invariants"),
            "{what}: {result:#?}"
        );
    }
}
