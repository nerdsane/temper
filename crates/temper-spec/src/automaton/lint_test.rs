use super::*;
use crate::automaton::{lint_csdl_reference_contracts, parse_automaton};
use crate::csdl::parse_csdl;
use std::collections::BTreeMap;

#[test]
fn lint_rejects_unknown_state_var_type() {
    let src = r#"
[automaton]
name = "Task"
states = ["Draft", "Done"]
initial = "Draft"

[[state]]
name = "mystery"
type = "mystery_type"
initial = "0"

[[action]]
name = "Complete"
from = ["Draft"]
to = "Done"
"#;
    let automaton = parse_automaton(src).expect("parse");
    let findings = lint_automaton(&automaton);
    assert!(
        findings
            .iter()
            .any(|finding| finding.code == "unknown_state_var_type"
                && finding.severity == LintSeverity::Error)
    );
}

#[test]
fn lint_rejects_unknown_guard_and_effect_variables() {
    let src = r#"
[automaton]
name = "Task"
states = ["Draft", "Done"]
initial = "Draft"

[[state]]
name = "approved"
type = "bool"
initial = "false"

[[action]]
name = "Complete"
from = ["Draft"]
to = "Done"
guard = "is_true phantom"
effect = "set ghost true"
"#;
    let automaton = parse_automaton(src).expect("parse");
    let findings = lint_automaton(&automaton);
    assert!(
        findings
            .iter()
            .any(|finding| finding.code == "guard_unknown_var"
                && finding.severity == LintSeverity::Error)
    );
    assert!(
        findings
            .iter()
            .any(|finding| finding.code == "effect_unknown_var"
                && finding.severity == LintSeverity::Error)
    );
}

#[test]
fn lint_warns_for_missing_to_on_internal_action() {
    let src = r#"
[automaton]
name = "Task"
states = ["Draft", "Done"]
initial = "Draft"

[[action]]
name = "Nop"
kind = "internal"
from = ["Draft"]
"#;
    let automaton = parse_automaton(src).expect("parse");
    let findings = lint_automaton(&automaton);
    assert!(
        findings
            .iter()
            .any(|finding| finding.code == "action_missing_to"
                && finding.severity == LintSeverity::Warning)
    );
}

#[test]
fn lint_allows_missing_to_for_output_action() {
    let src = r#"
[automaton]
name = "Task"
states = ["Draft", "Done"]
initial = "Draft"

[[action]]
name = "EmitAudit"
kind = "output"
from = ["Draft"]
effect = "emit audit"
"#;
    let automaton = parse_automaton(src).expect("parse");
    let findings = lint_automaton(&automaton);
    assert!(
        !findings
            .iter()
            .any(|finding| finding.code == "action_missing_to")
    );
}

#[test]
fn lint_rejects_nullable_parameter_consumed_by_mutating_effect() {
    let src = r#"
[automaton]
name = "Task"
states = ["Draft"]
initial = "Draft"

[[state]]
name = "count"
type = "counter"
initial = "0"

[[action]]
name = "Adjust"
from = ["Draft"]
params = [{ name = "delta", type = "Edm.Int64", nullable = true }]
effect = [{ type = "increment", var = "count", amount = "delta" }]
"#;
    let automaton = parse_automaton(src).expect("parse");
    let findings = lint_automaton(&automaton);

    assert!(findings.iter().any(|finding| {
        finding.code == "nullable_action_parameter_consumed"
            && finding.message.contains("delta")
            && finding.message.contains("counter effect")
    }));
}

#[test]
fn lint_allows_nullable_parameter_as_module_pass_through() {
    let src = r#"
[automaton]
name = "Task"
states = ["Draft"]
initial = "Draft"

[[action]]
name = "Enrich"
from = ["Draft"]
params = [{ name = "optional_note", type = "Edm.String", nullable = true }]

[[action.triggers]]
name = "enrich_module"
kind = "wasm"
module = "enrich"
"#;
    let automaton = parse_automaton(src).expect("parse");
    let findings = lint_automaton(&automaton);

    assert!(
        !findings
            .iter()
            .any(|finding| finding.code == "nullable_action_parameter_consumed")
    );
}

#[test]
fn lint_rejects_nullable_cross_entity_guard_identity() {
    let src = r#"
[automaton]
name = "Task"
states = ["Open"]
initial = "Open"

[[action]]
name = "CheckOwner"
from = ["Open"]
params = [{ name = "owner_id", type = "Edm.Guid", nullable = true }]
guard = [{ type = "cross_entity_state", entity_type = "Owner", entity_id_source = "owner_id", required_status = ["Active"] }]
"#;
    let automaton = parse_automaton(src).expect("parse");
    let findings = lint_automaton(&automaton);

    assert!(findings.iter().any(|finding| {
        finding.code == "nullable_action_parameter_consumed"
            && finding.message.contains("owner_id")
            && finding.message.contains("guard")
    }));
}

#[test]
fn lint_rejects_nullable_reference_equality_parameter() {
    let src = r#"
[automaton]
name = "Document"
states = ["Open"]
initial = "Open"

[[state]]
name = "workspace_id"
type = "ref"
entity_type = "Workspace"
initial = ""

[[action]]
name = "Update"
from = ["Open"]
params = [{ name = "workspace_id", type = "ref", entity_type = "Workspace", nullable = true }]
guard = [{ type = "reference_equals", reference = "workspace_id", param = "workspace_id" }]
"#;
    let findings = lint_automaton(&parse_automaton(src).expect("parse"));
    assert!(findings.iter().any(|finding| {
        finding.code == "nullable_action_parameter_consumed"
            && finding.message.contains("workspace_id")
            && finding.message.contains("guard")
    }));
}

fn parse(src: &str) -> Automaton {
    parse_automaton(src).expect("parse")
}

#[test]
fn bundle_lint_rejects_missing_spawn_target() {
    let parent = parse(
        r#"
[automaton]
name = "Plan"
states = ["Draft"]
initial = "Draft"

[[action]]
name = "AddTask"
from = ["Draft"]
effect = [{ type = "spawn", entity_type = "Task", entity_id_source = "{uuid}", initial_action = "Create" }]
"#,
    );

    let bundle = BTreeMap::from([("Plan".to_string(), parent)]);
    let findings = lint_automata_bundle(&bundle);
    assert!(findings.iter().any(|finding| {
        finding.code == "spawn_target_missing"
            && finding.entity == "Plan"
            && finding.severity == LintSeverity::Error
    }));
}

#[test]
fn bundle_lint_rejects_missing_spawn_initial_action() {
    let parent = parse(
        r#"
[automaton]
name = "Plan"
states = ["Draft"]
initial = "Draft"

[[action]]
name = "AddTask"
from = ["Draft"]
effect = [{ type = "spawn", entity_type = "Task", entity_id_source = "{uuid}", initial_action = "Create" }]
"#,
    );
    let child = parse(
        r#"
[automaton]
name = "Task"
states = ["Open", "Done"]
initial = "Open"

[[action]]
name = "Complete"
from = ["Open"]
to = "Done"
"#,
    );

    let bundle = BTreeMap::from([("Plan".to_string(), parent), ("Task".to_string(), child)]);
    let findings = lint_automata_bundle(&bundle);
    assert!(findings.iter().any(|finding| {
        finding.code == "spawn_initial_action_missing" && finding.entity == "Plan"
    }));
}

#[test]
fn bundle_lint_rejects_spawn_initial_action_not_enabled_from_initial() {
    let parent = parse(
        r#"
[automaton]
name = "Plan"
states = ["Draft"]
initial = "Draft"

[[action]]
name = "AddTask"
from = ["Draft"]
effect = [{ type = "spawn", entity_type = "Task", entity_id_source = "{uuid}", initial_action = "Create" }]
"#,
    );
    let child = parse(
        r#"
[automaton]
name = "Task"
states = ["Open", "InProgress"]
initial = "Open"

[[action]]
name = "Create"
from = ["InProgress"]
"#,
    );

    let bundle = BTreeMap::from([("Plan".to_string(), parent), ("Task".to_string(), child)]);
    let findings = lint_automata_bundle(&bundle);
    assert!(
        findings
            .iter()
            .any(|finding| { finding.code == "spawn_initial_action_not_from_initial_state" })
    );
}

#[test]
fn bundle_lint_rejects_unmapped_spawn_params() {
    let parent = parse(
        r#"
[automaton]
name = "Plan"
states = ["Draft"]
initial = "Draft"

[[action]]
name = "AddTask"
from = ["Draft"]
params = ["title"]
effect = [{ type = "spawn", entity_type = "Task", entity_id_source = "{uuid}", initial_action = "Create" }]
"#,
    );
    let child = parse(
        r#"
[automaton]
name = "Task"
states = ["Open"]
initial = "Open"

[[action]]
name = "Create"
from = ["Open"]
params = ["title", "description", "plan_id"]
"#,
    );

    let bundle = BTreeMap::from([("Plan".to_string(), parent), ("Task".to_string(), child)]);
    let findings = lint_automata_bundle(&bundle);
    assert!(findings.iter().any(|finding| {
        finding.code == "spawn_initial_action_params_unmapped"
            && finding.entity == "Plan"
            && finding.message.contains("description")
    }));
}

#[test]
fn bundle_lint_accepts_valid_spawn_contract() {
    let parent = parse(
        r#"
[automaton]
name = "Plan"
states = ["Active"]
initial = "Active"

[[action]]
name = "AddTask"
from = ["Active"]
params = ["title", "description"]
effect = [{ type = "spawn", entity_type = "Task", entity_id_source = "{uuid}", initial_action = "Create" }]
"#,
    );
    let child = parse(
        r#"
[automaton]
name = "Task"
states = ["Open"]
initial = "Open"

[[action]]
name = "Create"
from = ["Open"]
params = ["title", "description", "plan_id"]
"#,
    );

    let bundle = BTreeMap::from([("Plan".to_string(), parent), ("Task".to_string(), child)]);
    let findings = lint_automata_bundle(&bundle);
    assert!(
        findings.is_empty(),
        "expected no bundle lint findings, got: {findings:?}"
    );
}

#[test]
fn csdl_reference_constraint_must_match_target_key() {
    let document = parse(
        r#"
[automaton]
name = "Document"
states = ["Active"]
initial = "Active"
allow_indefinite_states = ["Active"]
[[state]]
name = "workspace_id"
type = "ref"
entity_type = "Workspace"
initial = ""
"#,
    );
    let workspace = parse(
        r#"
[automaton]
name = "Workspace"
states = ["Active"]
initial = "Active"
allow_indefinite_states = ["Active"]
"#,
    );
    let csdl = crate::csdl::parse_csdl(
        r#"<?xml version="1.0"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
 <edmx:DataServices><Schema Namespace="Test" xmlns="http://docs.oasis-open.org/odata/ns/edm">
  <EntityType Name="Workspace"><Key><PropertyRef Name="Id"/></Key><Property Name="Id" Type="Edm.String" Nullable="false"/></EntityType>
  <EntityType Name="Document"><Key><PropertyRef Name="Id"/></Key><Property Name="Id" Type="Edm.String" Nullable="false"/><Property Name="workspace_id" Type="Edm.String"/><NavigationProperty Name="Workspace" Type="Test.Workspace"><ReferentialConstraint Property="workspace_id" ReferencedProperty="WrongKey"/></NavigationProperty></EntityType>
 </Schema></edmx:DataServices>
</edmx:Edmx>"#,
    )
    .unwrap();
    let bundle = BTreeMap::from([
        ("Document".to_string(), document),
        ("Workspace".to_string(), workspace),
    ]);
    let findings = lint_csdl_reference_contracts(&csdl, &bundle);
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].code, "csdl_reference_contract_mismatch");
}

#[test]
fn csdl_bundle_lint_requires_matching_parameter_nullability() {
    let automaton = parse(
        r#"
[automaton]
name = "Task"
states = ["Open"]
initial = "Open"

[[action]]
name = "Assign"
kind = "input"
from = ["Open"]
params = [{ name = "agent_id", type = "Edm.String" }]
"#,
    );
    let csdl = parse_csdl(
        r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Test" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Task"><Key><PropertyRef Name="Id"/></Key><Property Name="Id" Type="Edm.String" Nullable="false"/></EntityType>
      <Action Name="Assign" IsBound="true">
        <Parameter Name="bindingParameter" Type="Temper.Test.Task" Nullable="false"/>
        <Parameter Name="AgentId" Type="Edm.String"/>
      </Action>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#,
    )
    .expect("parse CSDL");
    let findings =
        lint_automata_csdl_bundle(&BTreeMap::from([("Task".to_string(), automaton)]), &csdl);

    assert!(findings.iter().any(|finding| {
        finding.code == "csdl_action_parameter_requiredness_mismatch"
            && finding.message.contains("agent_id")
    }));
}

#[test]
fn csdl_bundle_lint_rejects_nullable_binding_and_alias_collision_stably() {
    let automaton = parse(
        r#"
[automaton]
name = "Task"
states = ["Open"]
initial = "Open"

[[action]]
name = "Assign"
kind = "input"
from = ["Open"]
params = ["agent_id", "AgentId"]
"#,
    );
    let csdl = parse_csdl(
        r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Test" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <Action Name="Assign" IsBound="true">
        <Parameter Name="bindingParameter" Type="Temper.Test.Task"/>
        <Parameter Name="AgentId" Type="Edm.String" Nullable="false"/>
      </Action>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#,
    )
    .expect("parse CSDL");
    let bundle = BTreeMap::from([("Task".to_string(), automaton)]);
    let first = lint_automata_csdl_bundle(&bundle, &csdl);
    let second = lint_automata_csdl_bundle(&bundle, &csdl);

    assert_eq!(first, second);
    assert!(
        first
            .iter()
            .any(|finding| finding.code == "csdl_action_binding_nullable")
    );
    assert!(
        first
            .iter()
            .any(|finding| finding.code == "csdl_action_parameter_alias_collision")
    );
}

#[test]
fn csdl_bundle_lint_requires_matching_bound_action() {
    let automaton = parse(
        r#"
[automaton]
name = "Directory"
states = ["Active"]
initial = "Active"

[[action]]
name = "Create"
kind = "input"
from = ["Active"]
params = ["name", "path", "workspace_id"]
"#,
    );
    let csdl = parse_csdl(
        r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.FS" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Directory"><Key><PropertyRef Name="Id"/></Key><Property Name="Id" Type="Edm.String" Nullable="false"/></EntityType>
      <Action Name="AddChild" IsBound="true">
        <Parameter Name="bindingParameter" Type="Temper.FS.Directory" Nullable="false"/>
      </Action>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#,
    )
    .expect("parse CSDL");
    let findings = lint_automata_csdl_bundle(
        &BTreeMap::from([("Directory".to_string(), automaton)]),
        &csdl,
    );

    assert!(findings.iter().any(|finding| {
        finding.code == "csdl_action_missing"
            && finding.entity == "Directory"
            && finding.message.contains("Create")
    }));
}

#[test]
fn csdl_bundle_lint_requires_exact_action_name() {
    let automaton = parse(
        r#"
[automaton]
name = "Task"
states = ["Open"]
initial = "Open"

[[action]]
name = "Assign"
kind = "input"
from = ["Open"]
params = [{ name = "agent_id", type = "Edm.String" }]
"#,
    );
    let csdl = parse_csdl(
        r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Test" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Task"><Key><PropertyRef Name="Id"/></Key><Property Name="Id" Type="Edm.String" Nullable="false"/></EntityType>
      <Action Name="assign" IsBound="true">
        <Parameter Name="bindingParameter" Type="Temper.Test.Task" Nullable="false"/>
        <Parameter Name="AgentId" Type="Edm.String" Nullable="false"/>
      </Action>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#,
    )
    .expect("parse CSDL");
    let findings =
        lint_automata_csdl_bundle(&BTreeMap::from([("Task".to_string(), automaton)]), &csdl);

    assert!(findings.iter().any(|finding| {
        finding.code == "csdl_action_missing" && finding.message.contains("Assign")
    }));
    assert!(
        !findings
            .iter()
            .any(|finding| finding.code == "csdl_action_parameter_requiredness_mismatch")
    );
}
