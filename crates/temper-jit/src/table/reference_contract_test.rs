use super::{Guard, TransitionTable};

#[test]
fn typed_reference_metadata_and_equality_guard_compile() {
    let spec = r#"
[automaton]
name = "Document"
states = ["Active"]
initial = "Active"

[[state]]
name = "workspace_id"
type = "ref"
entity_type = "Workspace"
initial = ""

[[key]]
name = "workspace_document"
properties = ["workspace_id"]
entity_id = true

[[action]]
name = "Update"
kind = "input"
from = ["Active"]
to = "Active"
params = [{ name = "workspace_id", type = "ref", entity_type = "Workspace" }]
guard = [{ type = "reference_equals", reference = "workspace_id", param = "workspace_id" }]
"#;
    let table = TransitionTable::from_ioa_source(spec);
    assert_eq!(
        table.state_var_metadata["workspace_id"]
            .entity_type
            .as_deref(),
        Some("Workspace")
    );
    assert_eq!(
        table.action_params["Update"]["workspace_id"]
            .entity_type
            .as_deref(),
        Some("Workspace")
    );
    assert!(table.keys[0].entity_id);
    assert!(matches!(
        table.rules[0].guard,
        Guard::And(ref guards)
            if guards.iter().any(|guard| matches!(guard, Guard::ReferenceEquals { .. }))
    ));
}
