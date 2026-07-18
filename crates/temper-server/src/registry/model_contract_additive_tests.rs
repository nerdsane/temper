use super::*;
use temper_spec::csdl::parse_csdl;

const CSDL_XML: &str = include_str!("../../../../test-fixtures/specs/model.csdl.xml");

const EXTENSIBLE_IOA: &str = r#"
[automaton]
name = "Order"
states = ["Active"]
initial = "Active"
allow_indefinite_states = ["Active"]

[[state]]
name = "safe"
type = "bool"
initial = "true"

[[state]]
name = "budget"
type = "counter"
initial = "0"

[[action]]
name = "StaySafe"
kind = "input"
from = ["Active"]
to = "Active"

[[invariant]]
name = "AlwaysSafe"
when = ["Active"]
assert = "safe"
"#;

fn minimal_csdl() -> (temper_spec::csdl::CsdlDocument, String) {
    let doc = parse_csdl(CSDL_XML).expect("CSDL should parse");
    (doc, CSDL_XML.to_string())
}

#[test]
fn hot_swap_allows_verified_additive_model_extension() {
    let extended_base = EXTENSIBLE_IOA.replacen(
        "states = [\"Active\"]",
        "states = [\"Active\", \"Archived\"]",
        1,
    );
    let extended = format!(
        r#"{extended_base}

[[action]]
name = "ArchiveSafe"
kind = "input"
from = ["Active"]
to = "Archived"
"#
    );
    let mut registry = SpecRegistry::new();
    let (csdl, xml) = minimal_csdl();
    registry.register_tenant("alpha", csdl, xml, &[("Order", EXTENSIBLE_IOA)]);
    let tenant = TenantId::new("alpha");
    let original_lock = registry
        .get_spec(&tenant, "Order")
        .unwrap()
        .swap_controller()
        .current();

    let (replacement_csdl, replacement_xml) = minimal_csdl();
    registry
        .try_register_tenant(
            "alpha",
            replacement_csdl,
            replacement_xml,
            &[("Order", &extended)],
        )
        .expect("proof-preserving additive extensions remain hot-swappable");

    let current = registry.get_spec(&tenant, "Order").unwrap();
    assert_eq!(current.ioa_source, extended);
    assert!(Arc::ptr_eq(
        &current.swap_controller().current(),
        &original_lock
    ));
    assert_eq!(current.table().rules.len(), 2);
}

#[test]
fn hot_swap_rejects_unsafe_unique_additive_action_before_mutating_registry() {
    let unsafe_extension = format!(
        r#"{EXTENSIBLE_IOA}

[[action]]
name = "BreakSafety"
kind = "input"
from = ["Active"]
to = "Active"
effect = [{{ type = "set_bool", var = "safe", value = "false" }}]
"#
    );
    assert_addition_rejected(&unsafe_extension, "unverified additive model mutation");
}

#[test]
fn hot_swap_rejects_parameterized_counter_effect_before_mutating_registry() {
    let unsafe_extension = format!(
        r#"{EXTENSIBLE_IOA}

[[action]]
name = "SetBudget"
kind = "input"
from = ["Active"]
to = "Active"
effect = [{{ type = "set_counter_from_param", var = "budget", param = "value" }}]
"#
    );
    assert_addition_rejected(
        &unsafe_extension,
        "parameterized runtime effects are not effect-free",
    );
}

#[test]
fn hot_swap_rejects_parameterized_effect_mutation_before_mutating_registry() {
    let original = format!(
        r#"{EXTENSIBLE_IOA}

[[action]]
name = "SetBudget"
kind = "input"
from = ["Active"]
to = "Active"
effect = [{{ type = "set_counter_from_param", var = "budget", param = "value" }}]
"#
    );
    let incoming = original.replacen("param = \"value\"", "param = \"replacement\"", 1);
    assert_swap_rejected(
        &original,
        &incoming,
        "existing runtime effects must retain exact replay semantics",
    );
}

#[test]
fn hot_swap_rejects_action_under_global_terminal_invariant() {
    const TERMINAL_IOA: &str = r#"
[automaton]
name = "Order"
states = ["Stopped"]
initial = "Stopped"
allow_indefinite_states = ["Stopped"]

[[invariant]]
name = "GloballyTerminal"
when = []
assert = "no_further_transitions"
"#;
    let incoming = format!(
        r#"{TERMINAL_IOA}

[[action]]
name = "Restart"
kind = "input"
from = ["Stopped"]
to = "Stopped"
"#
    );
    assert_swap_rejected(
        TERMINAL_IOA,
        &incoming,
        "global invariants forbid unverified additive actions",
    );
}

#[test]
fn hot_swap_rejects_additive_action_name_collision() {
    let colliding = format!(
        r#"{EXTENSIBLE_IOA}

[[action]]
name = "StaySafe"
kind = "input"
from = ["Active"]
to = "Active"
effect = [{{ type = "set_bool", var = "safe", value = "true" }}]
"#
    );
    assert_addition_rejected(&colliding, "new action identity collision");
}

fn assert_addition_rejected(incoming: &str, reason: &str) {
    assert_swap_rejected(EXTENSIBLE_IOA, incoming, reason);
}

fn assert_swap_rejected(original: &str, incoming: &str, reason: &str) {
    let mut registry = SpecRegistry::new();
    let (csdl, xml) = minimal_csdl();
    registry.register_tenant("alpha", csdl, xml, &[("Order", original)]);
    let tenant = TenantId::new("alpha");
    let original_spec = registry.get_spec(&tenant, "Order").unwrap();
    let original_lock = original_spec.swap_controller().current();
    let original_table = serde_json::to_value(&*original_spec.table()).unwrap();

    let (replacement_csdl, replacement_xml) = minimal_csdl();
    let error = registry
        .try_register_tenant(
            "alpha",
            replacement_csdl,
            replacement_xml,
            &[("Order", incoming)],
        )
        .expect_err(reason);

    assert!(matches!(
        error,
        RegistryError::RuntimeInvariantMigrationRequired { .. }
    ));
    let current = registry.get_spec(&tenant, "Order").unwrap();
    assert_eq!(current.ioa_source, original);
    assert!(Arc::ptr_eq(
        &current.swap_controller().current(),
        &original_lock
    ));
    assert_eq!(
        serde_json::to_value(&*current.table()).unwrap(),
        original_table
    );
}
