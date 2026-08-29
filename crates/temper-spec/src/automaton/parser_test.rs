pub(super) const ORDER_IOA: &str = include_str!("../../../../test-fixtures/specs/order.ioa.toml");
use super::parse_automaton;

#[path = "parser_collection_test.rs"]
mod collection;
#[path = "parser_core_test.rs"]
mod core;
#[path = "parser_failure_routes_test.rs"]
mod failure_routes;
#[path = "parser_features_test.rs"]
mod features;
#[path = "parser_integrations_test.rs"]
mod integrations;
#[path = "parser_triggers_test.rs"]
mod triggers;

// --- ADR-0049 / ADR-0050: [[state_timeout]] + allow_indefinite_states ---

#[cfg(test)]
mod state_timeout_validation {
    use super::super::parse_automaton;

    const BASE_SPEC: &str = r#"
[automaton]
name = "S"
states = ["A", "B", "C"]
initial = "A"

[[action]]
name = "Configure"
from = ["A"]
to = "B"

[[action]]
name = "TimeoutFail"
from = []
to = "C"
params = ["error_message"]

[[action]]
name = "Heartbeat"
from = []
"#;

    #[test]
    fn valid_state_timeout_parses_and_auto_wires_from_state() {
        let spec = format!(
            "{BASE_SPEC}\n[[state_timeout]]\nstate = \"B\"\nafter_seconds = 30\non_timeout = \"TimeoutFail\"\nreset_on = [\"Heartbeat\"]\n"
        );
        let auto = parse_automaton(&spec).unwrap();
        assert_eq!(auto.state_timeouts.len(), 1);

        let timeout_fail = auto
            .actions
            .iter()
            .find(|a| a.name == "TimeoutFail")
            .expect("TimeoutFail action present");
        assert!(
            timeout_fail.from.contains(&"B".to_string()),
            "state_timeout target action must auto-include the state in its `from` list: {:?}",
            timeout_fail.from
        );
    }

    #[test]
    fn auto_wire_is_idempotent_when_from_already_includes_state() {
        // TimeoutFail.from already contains "B"; adding state_timeout for "B"
        // must not duplicate the entry.
        let mut spec = BASE_SPEC.replace(
            "name = \"TimeoutFail\"\nfrom = []",
            "name = \"TimeoutFail\"\nfrom = [\"B\"]",
        );
        spec.push_str(
            "\n[[state_timeout]]\nstate = \"B\"\nafter_seconds = 30\non_timeout = \"TimeoutFail\"\n",
        );
        let auto = parse_automaton(&spec).unwrap();
        let timeout_fail = auto
            .actions
            .iter()
            .find(|a| a.name == "TimeoutFail")
            .unwrap();
        let count = timeout_fail
            .from
            .iter()
            .filter(|s| s.as_str() == "B")
            .count();
        assert_eq!(count, 1, "`from` must not duplicate already-present state");
    }

    #[test]
    fn rejects_timeout_on_undeclared_state() {
        let spec = format!(
            "{BASE_SPEC}\n[[state_timeout]]\nstate = \"Nope\"\nafter_seconds = 30\non_timeout = \"TimeoutFail\"\n"
        );
        let err = parse_automaton(&spec).unwrap_err();
        assert!(err.to_string().contains("undeclared state 'Nope'"));
    }

    #[test]
    fn rejects_timeout_on_unknown_action() {
        let spec = format!(
            "{BASE_SPEC}\n[[state_timeout]]\nstate = \"B\"\nafter_seconds = 30\non_timeout = \"Phantom\"\n"
        );
        let err = parse_automaton(&spec).unwrap_err();
        assert!(err.to_string().contains("on_timeout action 'Phantom'"));
    }

    #[test]
    fn rejects_reset_on_unknown_action() {
        let spec = format!(
            "{BASE_SPEC}\n[[state_timeout]]\nstate = \"B\"\nafter_seconds = 30\non_timeout = \"TimeoutFail\"\nreset_on = [\"Mystery\"]\n"
        );
        let err = parse_automaton(&spec).unwrap_err();
        assert!(err.to_string().contains("reset_on action 'Mystery'"));
    }

    #[test]
    fn rejects_zero_after_seconds() {
        let spec = format!(
            "{BASE_SPEC}\n[[state_timeout]]\nstate = \"B\"\nafter_seconds = 0\non_timeout = \"TimeoutFail\"\n"
        );
        let err = parse_automaton(&spec).unwrap_err();
        assert!(err.to_string().contains("after_seconds > 0"));
    }

    #[test]
    fn rejects_duplicate_state_declaration() {
        let spec = format!(
            "{BASE_SPEC}\n[[state_timeout]]\nstate = \"B\"\nafter_seconds = 30\non_timeout = \"TimeoutFail\"\n\n[[state_timeout]]\nstate = \"B\"\nafter_seconds = 60\non_timeout = \"TimeoutFail\"\n"
        );
        let err = parse_automaton(&spec).unwrap_err();
        assert!(err.to_string().contains("declared twice"));
    }

    #[test]
    fn rejects_allow_indefinite_with_undeclared_state() {
        let mut spec = BASE_SPEC.to_string();
        spec = spec.replace(
            "initial = \"A\"",
            "initial = \"A\"\nallow_indefinite_states = [\"Ghost\"]",
        );
        let err = parse_automaton(&spec).unwrap_err();
        assert!(err.to_string().contains("undeclared state 'Ghost'"));
    }
}

// --- ADR-0155: [[vector]] access-path validation -----------------------

#[cfg(test)]
mod vector_validation {
    use super::super::parse_automaton;

    const BASE_SPEC: &str = r#"
[automaton]
name = "DesignLanguage"
states = ["Draft", "Published"]
initial = "Draft"

[[state]]
name = "taste_vector"
type = "string"
initial = ""

[[state]]
name = "taste_vector_model"
type = "string"
initial = ""

[[action]]
name = "Publish"
from = ["Draft"]
to = "Published"
"#;

    #[test]
    fn valid_vector_path_parses() {
        let spec = format!(
            "{BASE_SPEC}\n[[vector]]\nname = \"taste\"\nproperty = \"taste_vector\"\nmodel_property = \"taste_vector_model\"\ndims = 384\nmetric = \"cosine\"\n"
        );
        let auto = parse_automaton(&spec).expect("valid vector path parses");
        assert_eq!(auto.vectors.len(), 1);
        assert_eq!(auto.vectors[0].name, "taste");
        assert_eq!(auto.vectors[0].dims, 384);
        assert_eq!(auto.vectors[0].metric, "cosine");
    }

    #[test]
    fn rejects_undeclared_property() {
        let spec = format!(
            "{BASE_SPEC}\n[[vector]]\nname = \"taste\"\nproperty = \"missing_vec\"\nmodel_property = \"taste_vector_model\"\ndims = 384\nmetric = \"cosine\"\n"
        );
        let err = parse_automaton(&spec).expect_err("undeclared property must reject");
        assert!(
            err.to_string()
                .contains("undeclared property state variable 'missing_vec'"),
            "got: {err}"
        );
    }

    #[test]
    fn rejects_undeclared_model_property() {
        let spec = format!(
            "{BASE_SPEC}\n[[vector]]\nname = \"taste\"\nproperty = \"taste_vector\"\nmodel_property = \"missing_model\"\ndims = 384\nmetric = \"cosine\"\n"
        );
        let err = parse_automaton(&spec).expect_err("undeclared model_property must reject");
        assert!(
            err.to_string()
                .contains("undeclared model_property state variable 'missing_model'"),
            "got: {err}"
        );
    }

    #[test]
    fn rejects_zero_dims() {
        let spec = format!(
            "{BASE_SPEC}\n[[vector]]\nname = \"taste\"\nproperty = \"taste_vector\"\nmodel_property = \"taste_vector_model\"\ndims = 0\nmetric = \"cosine\"\n"
        );
        let err = parse_automaton(&spec).expect_err("dims=0 must reject");
        assert!(
            err.to_string().contains("must declare dims > 0"),
            "got: {err}"
        );
    }

    #[test]
    fn rejects_unknown_metric() {
        let spec = format!(
            "{BASE_SPEC}\n[[vector]]\nname = \"taste\"\nproperty = \"taste_vector\"\nmodel_property = \"taste_vector_model\"\ndims = 384\nmetric = \"manhattan\"\n"
        );
        let err = parse_automaton(&spec).expect_err("unknown metric must reject");
        assert!(
            err.to_string().contains("unknown metric 'manhattan'"),
            "got: {err}"
        );
    }

    #[test]
    fn rejects_duplicate_vector_name() {
        let spec = format!(
            "{BASE_SPEC}\n[[vector]]\nname = \"taste\"\nproperty = \"taste_vector\"\nmodel_property = \"taste_vector_model\"\ndims = 384\nmetric = \"cosine\"\n[[vector]]\nname = \"taste\"\nproperty = \"taste_vector\"\nmodel_property = \"taste_vector_model\"\ndims = 8\nmetric = \"dot\"\n"
        );
        let err = parse_automaton(&spec).expect_err("duplicate name must reject");
        assert!(err.to_string().contains("declared twice"), "got: {err}");
    }
}

// --- ADR-0050: liveness coverage rule ----------------------------------

#[cfg(test)]
mod liveness_coverage {
    use super::super::{LivenessEnforcement, parse_automaton, parse_automaton_with_liveness};

    const SPEC_WITH_TRAP_STATE: &str = r#"
[automaton]
name = "Trappy"
states = ["Start", "Running", "Done"]
initial = "Start"

[[action]]
name = "Begin"
from = ["Start"]
to = "Running"

# Running is non-terminal (Done is reachable) but has no state_timeout
# and is not allowlisted. This is the trap-state pattern ADR-0050 rejects.

[[action]]
name = "Finish"
from = ["Running"]
to = "Done"
"#;

    #[test]
    fn warn_only_accepts_spec_with_trap_state() {
        let auto =
            parse_automaton_with_liveness(SPEC_WITH_TRAP_STATE, LivenessEnforcement::WarnOnly)
                .expect("warn-only mode must accept");
        assert_eq!(auto.automaton.name, "Trappy");
    }

    #[test]
    fn default_parse_does_not_enforce() {
        // `parse_automaton` reads TEMPER_LIVENESS_ENFORCE which defaults to
        // unset (warn-only). Trap-state spec must still parse successfully
        // so the production default does not break existing callers
        // during rollout.
        parse_automaton(SPEC_WITH_TRAP_STATE).expect("default mode must accept");
    }

    #[test]
    fn enforce_rejects_missing_coverage() {
        let err = parse_automaton_with_liveness(SPEC_WITH_TRAP_STATE, LivenessEnforcement::Enforce)
            .expect_err("enforce must reject");
        let msg = err.to_string();
        assert!(
            msg.contains("ADR-0050"),
            "error should cite ADR-0050: {msg}"
        );
        assert!(
            msg.contains("Running"),
            "error must name the trap state: {msg}"
        );
        // Start also has no coverage and is non-terminal — both should appear.
        assert!(
            msg.contains("Start"),
            "error must list all violations, not just the first: {msg}"
        );
    }

    #[test]
    fn enforce_accepts_spec_with_timeout_coverage() {
        let spec = r#"
[automaton]
name = "Safe"
states = ["Start", "Running", "Done"]
initial = "Start"

[[action]]
name = "Begin"
from = ["Start"]
to = "Running"

[[action]]
name = "Finish"
from = ["Running"]
to = "Done"

[[action]]
name = "BailOut"
from = []
to = "Done"
params = ["error_message"]

[[state_timeout]]
state = "Start"
after_seconds = 60
on_timeout = "BailOut"

[[state_timeout]]
state = "Running"
after_seconds = 120
on_timeout = "BailOut"
"#;
        parse_automaton_with_liveness(spec, LivenessEnforcement::Enforce)
            .expect("coverage complete spec must pass");
    }

    #[test]
    fn enforce_accepts_spec_with_allowlist_coverage() {
        let spec = r#"
[automaton]
name = "Patient"
states = ["Start", "WaitingForApproval", "Done"]
initial = "Start"
allow_indefinite_states = ["WaitingForApproval"]

[[action]]
name = "Await"
from = ["Start"]
to = "WaitingForApproval"

[[action]]
name = "Finish"
from = ["WaitingForApproval"]
to = "Done"

[[action]]
name = "BailOut"
from = []
to = "Done"
params = ["error_message"]

[[state_timeout]]
state = "Start"
after_seconds = 60
on_timeout = "BailOut"
"#;
        parse_automaton_with_liveness(spec, LivenessEnforcement::Enforce)
            .expect("allowlist + timeout spec must pass");
    }

    #[test]
    fn non_terminal_states_omits_terminal_states() {
        // Verify the helper exposes what ADR-0050 depends on.
        let auto = parse_automaton(SPEC_WITH_TRAP_STATE).unwrap();
        let non_terminal = auto.non_terminal_states();
        assert!(non_terminal.contains(&"Start".to_string()));
        assert!(non_terminal.contains(&"Running".to_string()));
        assert!(
            !non_terminal.contains(&"Done".to_string()),
            "Done has no outgoing transitions — should be terminal"
        );
    }
}
