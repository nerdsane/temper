use super::*;

#[test]
fn test_verify_reference_specs() {
    let specs_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../test-fixtures/specs");

    if !Path::new(specs_dir).join("model.csdl.xml").exists() {
        eprintln!("Skipping verify test: reference specs not found");
        return;
    }

    let result = run(&[specs_dir.to_string()], 250_000);
    result.expect("verify should pass on reference specs");
}

#[test]
fn test_verify_fails_on_broken_spawn_contract_with_exact_lint_code() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let specs_dir = tmp.path();

    let csdl = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
<Schema Namespace="Temper.Broken" xmlns="http://docs.oasis-open.org/odata/ns/edm">
  <EntityType Name="Plan">
    <Key><PropertyRef Name="Id" /></Key>
    <Property Name="Id" Type="Edm.Guid" Nullable="false" />
    <Property Name="status" Type="Edm.String" />
  </EntityType>
  <EntityType Name="Task">
    <Key><PropertyRef Name="Id" /></Key>
    <Property Name="Id" Type="Edm.Guid" Nullable="false" />
    <Property Name="status" Type="Edm.String" />
  </EntityType>
  <EntityContainer Name="Service">
    <EntitySet Name="Plans" EntityType="Temper.Broken.Plan" />
    <EntitySet Name="Tasks" EntityType="Temper.Broken.Task" />
  </EntityContainer>
</Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;
    let plan = r#"
[automaton]
name = "Plan"
states = ["Active"]
initial = "Active"

[[action]]
name = "AddTask"
kind = "input"
from = ["Active"]
params = ["title"]
effect = [{ type = "spawn", entity_type = "Task", entity_id_source = "{uuid}", initial_action = "Create" }]
"#;
    let task = r#"
[automaton]
name = "Task"
states = ["Open"]
initial = "Open"

[[action]]
name = "Create"
kind = "input"
from = ["Open"]
params = ["title", "description", "plan_id"]
"#;

    fs::write(specs_dir.join("model.csdl.xml"), csdl).expect("write csdl");
    fs::write(specs_dir.join("plan.ioa.toml"), plan).expect("write plan");
    fs::write(specs_dir.join("task.ioa.toml"), task).expect("write task");

    let result = run(
        &[specs_dir.to_str().expect("tmp path utf-8").to_string()],
        250_000,
    );
    let err = result.expect_err("verify should fail on broken spawn contract");
    let msg = err.to_string();
    assert!(
        msg.contains("spawn_initial_action_params_unmapped"),
        "expected exact lint code in error, got: {msg}"
    );
}

#[test]
fn test_multi_entity_dir_runs_composite_as_gating_step() {
    // A two-entity directory whose cross-entity reaction can be dropped:
    // Workspace.Freeze moves Workspace out of Active before File.Touch
    // fires Workspace.IncrementUsage (enabled only from Active). The
    // dropped reaction must FAIL the command (composite is gating).
    let tmp = tempfile::tempdir().expect("tempdir");
    let specs_dir = tmp.path();

    let csdl = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
<Schema Namespace="Temper.Fs" xmlns="http://docs.oasis-open.org/odata/ns/edm">
  <EntityType Name="File">
    <Key><PropertyRef Name="Id" /></Key>
    <Property Name="Id" Type="Edm.Guid" Nullable="false" />
    <Property Name="status" Type="Edm.String" />
    <Property Name="workspace_id" Type="Edm.String" />
  </EntityType>
  <EntityType Name="Workspace">
    <Key><PropertyRef Name="Id" /></Key>
    <Property Name="Id" Type="Edm.Guid" Nullable="false" />
    <Property Name="status" Type="Edm.String" />
  </EntityType>
  <EntityContainer Name="Service">
    <EntitySet Name="Files" EntityType="Temper.Fs.File" />
    <EntitySet Name="Workspaces" EntityType="Temper.Fs.Workspace" />
  </EntityContainer>
</Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;
    let file = r#"
[automaton]
name = "File"
states = ["New", "Updated"]
initial = "New"

[[action]]
name = "Touch"
kind = "input"
from = ["New"]
to = "Updated"

[[action.triggers]]
name = "touch_increments_usage"
kind = "entity"
target_entity = "Workspace"
target_action = "IncrementUsage"

[action.triggers.resolve_target]
type = "field"
field = "workspace_id"
"#;
    let workspace = r#"
[automaton]
name = "Workspace"
states = ["Active", "Frozen"]
initial = "Active"

[[action]]
name = "IncrementUsage"
kind = "input"
from = ["Active"]
to = "Active"

[[action]]
name = "Freeze"
kind = "internal"
from = ["Active"]
to = "Frozen"
"#;

    fs::write(specs_dir.join("model.csdl.xml"), csdl).expect("write csdl");
    fs::write(specs_dir.join("file.ioa.toml"), file).expect("write file");
    fs::write(specs_dir.join("workspace.ioa.toml"), workspace).expect("write workspace");

    let result = run(
        &[specs_dir.to_str().expect("tmp path utf-8").to_string()],
        250_000,
    );
    let err = result.expect_err("composite gating step must fail on a dropped reaction");
    let msg = err.to_string();
    assert!(
        msg.contains("composite cross-entity verification failed"),
        "expected composite gating failure, got: {msg}"
    );
    assert!(
        msg.contains("IncrementUsage") && msg.contains("Frozen"),
        "failure should name the dropped reaction + wrong state, got: {msg}"
    );
}

#[test]
fn test_two_dirs_compose_a_guard_only_coupling() {
    // File lives in dir A, Workspace in dir B. File.Submit reads
    // Workspace.status. The union must put Workspace in File's
    // composite scope even though no trigger connects them.
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir_a = tmp.path().join("a");
    let dir_b = tmp.path().join("b");
    fs::create_dir(&dir_a).expect("dir a");
    fs::create_dir(&dir_b).expect("dir b");

    let csdl_a = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
<Schema Namespace="A" xmlns="http://docs.oasis-open.org/odata/ns/edm">
  <EntityType Name="File">
    <Key><PropertyRef Name="Id" /></Key>
    <Property Name="Id" Type="Edm.Guid" Nullable="false" />
    <Property Name="status" Type="Edm.String" />
    <Property Name="workspace_id" Type="Edm.String" />
  </EntityType>
  <EntityContainer Name="Service">
    <EntitySet Name="Files" EntityType="A.File" />
  </EntityContainer>
</Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;
    let csdl_b = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
<Schema Namespace="B" xmlns="http://docs.oasis-open.org/odata/ns/edm">
  <EntityType Name="Workspace">
    <Key><PropertyRef Name="Id" /></Key>
    <Property Name="Id" Type="Edm.Guid" Nullable="false" />
    <Property Name="status" Type="Edm.String" />
  </EntityType>
  <EntityContainer Name="Service">
    <EntitySet Name="Workspaces" EntityType="B.Workspace" />
  </EntityContainer>
</Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;
    let file = r#"
[automaton]
name = "File"
states = ["Draft", "Ready"]
initial = "Draft"
allow_indefinite_states = ["Draft", "Ready"]

[[action]]
name = "Submit"
kind = "input"
from = ["Draft"]
to = "Ready"
guard = [{ type = "cross_entity_state", entity_type = "Workspace", entity_id_source = "workspace_id", required_status = ["Active"] }]
"#;
    let workspace = r#"
[automaton]
name = "Workspace"
states = ["Active", "Frozen"]
initial = "Active"
allow_indefinite_states = ["Active", "Frozen"]

[[action]]
name = "Freeze"
kind = "internal"
from = ["Active"]
to = "Frozen"
"#;

    fs::write(dir_a.join("model.csdl.xml"), csdl_a).expect("write csdl a");
    fs::write(dir_a.join("file.ioa.toml"), file).expect("write file");
    fs::write(dir_b.join("model.csdl.xml"), csdl_b).expect("write csdl b");
    fs::write(dir_b.join("workspace.ioa.toml"), workspace).expect("write workspace");

    let result = run(
        &[
            dir_a.to_str().expect("utf-8").to_string(),
            dir_b.to_str().expect("utf-8").to_string(),
        ],
        250_000,
    );
    result.expect("guard-only coupling across two dirs must compose and pass");
}
