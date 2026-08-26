use super::build_model_from_ioa;

const VALID: &str = r#"
[automaton]
name = "Payment"
states = ["Created", "Charging", "Reconciling"]
initial = "Created"

[[action]]
name = "Charge"
from = ["Created"]
to = "Charging"

[[action.triggers]]
name = "charge_card"
kind = "wasm"
module = "payments"

[[action.triggers.failure_routes]]
category = "ambiguous"
action = "Reconcile"

[[action]]
name = "Reconcile"
from = ["Charging"]
to = "Reconciling"
params = [{ name = "failure", type = "failure_v1" }]
"#;

#[test]
fn verification_model_accepts_validated_typed_recovery_transition() {
    let model = build_model_from_ioa(VALID, 2).expect("valid failure route model");
    assert!(
        model
            .transitions
            .iter()
            .any(|transition| transition.name == "Reconcile")
    );
}

#[test]
fn verification_fails_before_model_build_for_incompatible_callback() {
    let invalid = VALID.replace(
        "params = [{ name = \"failure\", type = \"failure_v1\" }]",
        "params = [\"error_message\"]",
    );
    let error = match build_model_from_ioa(&invalid, 2) {
        Ok(_) => panic!("callback ABI must fail closed"),
        Err(error) => error,
    };
    assert!(error.contains("failure_v1"));
}

#[test]
fn verification_fails_before_model_build_for_undeclared_category() {
    let invalid = VALID.replace("category = \"ambiguous\"", "category = \"provider_busy\"");
    let error = match build_model_from_ioa(&invalid, 2) {
        Ok(_) => panic!("open category must fail closed"),
        Err(error) => error,
    };
    assert!(error.contains("unknown variant"));
}
