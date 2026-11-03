//! `principal_record = true` marks a type whose record, keyed by a user's
//! `sub`, describes that user to Cedar as `principal.<Type>`; its
//! `principal_record_set` list reaches Cedar as `principal.<Type>_<field>`.

use super::parser::{LivenessEnforcement, parse_automaton_with_liveness};

fn member_spec(name: &str) -> String {
    format!(
        r#"
[automaton]
name = "{name}"
states = ["Active", "Removed"]
initial = "Active"
allow_indefinite_states = ["Active", "Removed"]
principal_record = true

[[action]]
name = "Remove"
kind = "input"
from = ["Active"]
to = "Removed"
"#
    )
}

fn parse(source: &str) -> Result<super::types::Automaton, String> {
    parse_automaton_with_liveness(source, LivenessEnforcement::WarnOnly).map_err(|e| e.to_string())
}

#[test]
fn a_type_marked_principal_record_loads() {
    if let Err(error) = parse(&member_spec("Member")) {
        panic!("a principal record spec loads: {error}");
    }
}

#[test]
fn a_principal_record_named_like_a_principal_attribute_is_refused() {
    // Each is an attribute the kernel sets on a principal, or the shape of
    // one: a record type with that name would stand in for it in Cedar.
    for name in ["role", "agentTypeVerified", "id", "actingForMember"] {
        let error = parse(&member_spec(name)).expect_err(name);
        assert!(
            error.contains(&format!(
                "principal_record type '{name}' must start with an uppercase letter"
            )),
            "{name}: {error}"
        );
    }
}

/// A Role type: the roles one person holds, granted and revoked by value.
fn role_spec(marker: &str, set: &str, roles_type: &str) -> String {
    let initial = if roles_type == "list" { "[]" } else { "\"\"" };
    format!(
        r#"
[automaton]
name = "Role"
states = ["Active"]
initial = "Active"
allow_indefinite_states = ["Active"]
{marker}
principal_record_set = "{set}"

[[state]]
name = "roles"
type = "{roles_type}"
initial = {initial}

[[action]]
name = "Grant"
kind = "input"
from = ["Active"]
to = "Active"
params = ["role"]
guard = "params.role not in roles"
effect = ["append(roles, params.role)"]

[[action]]
name = "Revoke"
kind = "input"
from = ["Active"]
to = "Active"
params = ["role"]
effect = ["remove(roles, params.role)"]
"#
    )
}

#[test]
fn a_principal_record_set_naming_a_list_loads() {
    let automaton = parse(&role_spec("principal_record = true", "roles", "list"))
        .unwrap_or_else(|error| panic!("a role spec loads: {error}"));
    assert_eq!(
        automaton.automaton.principal_record_set.as_deref(),
        Some("roles")
    );
}

#[test]
fn a_principal_record_set_without_principal_record_is_refused() {
    let error = parse(&role_spec("", "roles", "list")).expect_err("no principal_record");
    assert!(
        error.contains("principal_record_set = 'roles' needs principal_record = true"),
        "{error}"
    );
}

#[test]
fn a_principal_record_set_must_name_a_declared_list() {
    for (set, roles_type) in [("missing", "list"), ("roles", "string")] {
        let error = parse(&role_spec("principal_record = true", set, roles_type))
            .expect_err("not a declared list");
        assert!(
            error.contains(&format!(
                "principal_record_set = '{set}' must name a list state variable of 'Role'"
            )),
            "{set} ({roles_type}): {error}"
        );
    }
}
