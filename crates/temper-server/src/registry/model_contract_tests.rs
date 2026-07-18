use super::*;
use temper_spec::csdl::parse_csdl;

const CSDL_XML: &str = include_str!("../../../../test-fixtures/specs/model.csdl.xml");

fn minimal_csdl() -> (temper_spec::csdl::CsdlDocument, String) {
    let doc = parse_csdl(CSDL_XML).expect("CSDL should parse");
    (doc, CSDL_XML.to_string())
}

#[test]
fn hot_swap_rejects_guard_only_model_contract_before_mutating_registry() {
    const OLD_IOA: &str = r#"
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
name = "unlock"
type = "bool"
initial = "false"

[[action]]
name = "BreakSafety"
kind = "input"
from = ["Active"]
to = "Active"
guard = [{ type = "is_true", var = "unlock" }]
effect = [{ type = "set_bool", var = "safe", value = "false" }]
"#;
    const VERIFIED_IOA: &str = r#"
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
name = "unlock"
type = "bool"
initial = "false"

[[action]]
name = "BreakSafety"
kind = "input"
from = ["Active"]
to = "Active"
guard = [{ type = "is_true", var = "unlock" }]
effect = [{ type = "set_bool", var = "safe", value = "false" }]

[[invariant]]
name = "AlwaysSafe"
when = ["Active"]
assert = "safe"
"#;

    let mut registry = SpecRegistry::new();
    let (csdl, xml) = minimal_csdl();
    registry.register_tenant("alpha", csdl, xml, &[("Order", OLD_IOA)]);
    let tenant = TenantId::new("alpha");
    let original_spec = registry.get_spec(&tenant, "Order").unwrap();
    let original_lock = original_spec.swap_controller().current();
    let original_table = original_spec.table();
    assert!(
        original_table.model_protected_state_vars.is_empty(),
        "the old contract permits durable caller-authored boolean state"
    );
    let original_table_json = serde_json::to_value(&*original_table).unwrap();
    let original = registry.get_tenant(&tenant).unwrap();
    let original_source = original.entities["Order"].ioa_source.clone();
    let original_csdl = Arc::clone(&original.csdl);
    let original_csdl_xml = Arc::clone(&original.csdl_xml);

    let (replacement_csdl, replacement_xml) = minimal_csdl();
    let error = registry
        .try_register_tenant(
            "alpha",
            replacement_csdl,
            replacement_xml,
            &[("Order", VERIFIED_IOA)],
        )
        .expect_err("a new reachability contract requires durable-state migration");

    assert!(matches!(
        error,
        RegistryError::RuntimeInvariantMigrationRequired { .. }
    ));
    let current = registry.get_tenant(&tenant).unwrap();
    assert_eq!(current.entities["Order"].ioa_source, original_source);
    assert!(Arc::ptr_eq(&current.csdl, &original_csdl));
    assert!(Arc::ptr_eq(&current.csdl_xml, &original_csdl_xml));
    let current_spec = registry.get_spec(&tenant, "Order").unwrap();
    let current_lock = current_spec.swap_controller().current();
    assert!(Arc::ptr_eq(&current_lock, &original_lock));
    let current_table = current_spec.table();
    assert_eq!(
        serde_json::to_value(&*current_table).unwrap(),
        original_table_json,
        "rejection must leave the prior live transition table untouched"
    );
}

#[test]
fn hot_swap_allows_metadata_only_change_under_same_model_contract() {
    const ORIGINAL_IOA: &str = r#"
[automaton]
name = "Order"
states = ["Active"]
initial = "Active"
allow_indefinite_states = ["Active"]

[[state]]
name = "safe"
type = "bool"
initial = "true"

[[invariant]]
name = "AlwaysSafe"
when = ["Active"]
assert = "safe"
"#;
    const METADATA_IOA: &str = r#"
[automaton]
name = "Order"
states = ["Active"]
initial = "Active"
allow_indefinite_states = ["Active"]

[[state]]
name = "safe"
type = "bool"
initial = "true"
overflow_inline_max_bytes = 4096
overflow_ttl_seconds = 60

[[invariant]]
name = "AlwaysSafe"
when = ["Active"]
assert = "safe"
"#;

    let mut registry = SpecRegistry::new();
    let (csdl, xml) = minimal_csdl();
    registry.register_tenant("alpha", csdl, xml, &[("Order", ORIGINAL_IOA)]);
    let original_lock = registry
        .get_spec(&TenantId::new("alpha"), "Order")
        .unwrap()
        .swap_controller()
        .current();

    let (replacement_csdl, replacement_xml) = minimal_csdl();
    registry
        .try_register_tenant(
            "alpha",
            replacement_csdl,
            replacement_xml,
            &[("Order", METADATA_IOA)],
        )
        .expect("runtime-only metadata must remain hot-swappable");

    let current = registry.get_spec(&TenantId::new("alpha"), "Order").unwrap();
    assert!(Arc::ptr_eq(
        &current.swap_controller().current(),
        &original_lock
    ));
    let metadata = &current.table().state_var_metadata["safe"];
    assert_eq!(metadata.overflow_inline_max_bytes, Some(4096));
    assert_eq!(metadata.overflow_ttl_seconds, Some(60));
}

#[test]
fn hot_swap_rejects_reachability_change_under_same_model_invariant() {
    const GUARDED_IOA: &str = r#"
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
name = "unlock"
type = "bool"
initial = "false"

[[action]]
name = "ChangeReachability"
kind = "input"
from = ["Active"]
to = "Active"
guard = [{ type = "is_true", var = "unlock" }]
effect = [{ type = "set_bool", var = "unlock", value = "false" }]

[[invariant]]
name = "AlwaysSafe"
when = ["Active"]
assert = "safe"
"#;
    const REACHABLE_IOA: &str = r#"
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
name = "unlock"
type = "bool"
initial = "false"

[[action]]
name = "ChangeReachability"
kind = "input"
from = ["Active"]
to = "Active"
guard = [{ type = "is_false", var = "unlock" }]
effect = [{ type = "set_bool", var = "unlock", value = "true" }]

[[invariant]]
name = "AlwaysSafe"
when = ["Active"]
assert = "safe"
"#;

    let mut registry = SpecRegistry::new();
    let (csdl, xml) = minimal_csdl();
    registry.register_tenant("alpha", csdl, xml, &[("Order", GUARDED_IOA)]);
    let tenant = TenantId::new("alpha");
    let original_source = registry
        .get_spec(&tenant, "Order")
        .unwrap()
        .ioa_source
        .clone();

    let (replacement_csdl, replacement_xml) = minimal_csdl();
    let error = registry
        .try_register_tenant(
            "alpha",
            replacement_csdl,
            replacement_xml,
            &[("Order", REACHABLE_IOA)],
        )
        .expect_err("changed reachable-state semantics require durable-state migration");

    assert!(matches!(
        error,
        RegistryError::RuntimeInvariantMigrationRequired { .. }
    ));
    assert_eq!(
        registry.get_spec(&tenant, "Order").unwrap().ioa_source,
        original_source
    );
}

#[test]
fn hot_swap_rejects_action_identity_collision_before_mutation() {
    const UNIQUE_ACTIONS_IOA: &str = r#"
[automaton]
name = "Order"
states = ["Unchecked", "Checked"]
initial = "Unchecked"
allow_indefinite_states = ["Unchecked", "Checked"]

[[state]]
name = "safe"
type = "bool"
initial = "true"

[[action]]
name = "CorruptUnchecked"
kind = "input"
from = ["Unchecked"]
to = "Unchecked"
effect = [{ type = "set_bool", var = "safe", value = "false" }]

[[action]]
name = "EnterChecked"
kind = "input"
from = ["Unchecked"]
to = "Checked"
effect = [{ type = "set_bool", var = "safe", value = "true" }]

[[invariant]]
name = "CheckedIsSafe"
when = ["Checked"]
assert = "safe"
"#;

    let mut registry = SpecRegistry::new();
    let (csdl, xml) = minimal_csdl();
    registry.register_tenant("alpha", csdl, xml, &[("Order", UNIQUE_ACTIONS_IOA)]);
    let tenant = TenantId::new("alpha");
    let original_spec = registry.get_spec(&tenant, "Order").unwrap();
    let original_lock = original_spec.swap_controller().current();
    let original_table = serde_json::to_value(&*original_spec.table()).unwrap();
    let duplicate_actions =
        UNIQUE_ACTIONS_IOA.replacen("name = \"EnterChecked\"", "name = \"CorruptUnchecked\"", 1);

    let (replacement_csdl, replacement_xml) = minimal_csdl();
    let error = registry
        .try_register_tenant(
            "alpha",
            replacement_csdl,
            replacement_xml,
            &[("Order", &duplicate_actions)],
        )
        .expect_err("action identity changes L1 effect resolution");

    assert!(matches!(
        error,
        RegistryError::RuntimeInvariantMigrationRequired { .. }
    ));
    let current = registry.get_spec(&tenant, "Order").unwrap();
    assert_eq!(current.ioa_source, UNIQUE_ACTIONS_IOA);
    assert!(Arc::ptr_eq(
        &current.swap_controller().current(),
        &original_lock
    ));
    assert_eq!(
        serde_json::to_value(&*current.table()).unwrap(),
        original_table
    );
}
