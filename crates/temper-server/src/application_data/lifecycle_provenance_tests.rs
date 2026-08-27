use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::process::Command;

use temper_authz::SecurityContext;
use temper_runtime::{ActorSystem, tenant::TenantId};
use temper_spec::bundle::IoaSourceInput;
use temper_wasm_sdk::data::{
    DataOperationKind, DataOperationV1, DataOutcomeV1, DataResultV1, EntityDataGrant,
    ModuleDataGrant,
};

use super::tests::call;
use super::{ApplicationDataInvocation, ModuleInvocationAuthority};
use crate::state::ServerState;

const CSDL: &str = r#"<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx"><edmx:DataServices><Schema Namespace="Temper.Provenance" xmlns="http://docs.oasis-open.org/odata/ns/edm"><EntityType Name="SolverSession"><Key><PropertyRef Name="Id"/></Key><Property Name="Id" Type="Edm.Guid" Nullable="false"/><Property Name="State" Type="Edm.String" Nullable="false" DefaultValue="Unconfigured"/><Property Name="RegionState" Type="Edm.String" Nullable="false" DefaultValue="CA"/></EntityType><Action Name="Activate" IsBound="true"><Parameter Name="bindingParameter" Type="Temper.Provenance.SolverSession"/><ReturnType Type="Temper.Provenance.SolverSession" Nullable="false"/></Action><EntityContainer Name="Container"><EntitySet Name="SolverSessions" EntityType="Temper.Provenance.SolverSession"/></EntityContainer></Schema></edmx:DataServices></edmx:Edmx>"#;

const IOA: &str = r#"[automaton]
name = "SolverSession"
states = ["Unconfigured", "Active"]
initial = "Unconfigured"

[[action]]
name = "Activate"
kind = "input"
from = ["Unconfigured"]
to = "Active"
"#;

#[tokio::test]
async fn state_lifecycle_projects_transition_through_generated_action_and_keyed_read() {
    let operations = BTreeSet::from([
        DataOperationKind::ActionInvoke,
        DataOperationKind::EntityCreate,
        DataOperationKind::EntityGet,
    ]);
    let grant = ModuleDataGrant {
        operations: operations.clone(),
        entities: vec![EntityDataGrant {
            entity_type: "Temper.Provenance.SolverSession".into(),
            actions: BTreeSet::from(["Activate".into()]),
            ..EntityDataGrant::default()
        }],
        ..ModuleDataGrant::default()
    };
    let csdl = temper_spec::csdl::parse_csdl(CSDL).expect("fixture CSDL parses");
    let generated = temper_codegen::generate_module_sdk(
        &csdl,
        &[IoaSourceInput {
            entity_type: "Temper.Provenance.SolverSession".into(),
            source: IOA.into(),
        }],
        "solver",
        "closure",
        "closure",
        "artifact",
        grant,
    )
    .expect("fixture SDK generates");
    let state = ServerState::with_specs(
        ActorSystem::new("lifecycle-provenance-test"),
        csdl,
        CSDL.into(),
        BTreeMap::from([("SolverSession".into(), IOA.into())]),
    )
    .expect("fixture server state verifies");
    let authority = ModuleInvocationAuthority::new(
        TenantId::default(),
        "solver".into(),
        "artifact".into(),
        "Activate".into(),
        "SolverSession".into(),
        SecurityContext::system(),
        generated.manifest.clone(),
        super::ModuleDataTarget::TenantGlobal,
    );
    let invocation = ApplicationDataInvocation::new(state, authority);
    let id = "018f1f80-7b2d-7000-8000-000000000001";
    let created = call(
        &invocation,
        DataOperationV1::EntityCreate {
            entity_type: "Temper.Provenance.SolverSession".into(),
            value: serde_json::json!({"Id": id, "State": "Unconfigured"})
                .as_object()
                .cloned()
                .expect("fixture create is an object"),
        },
    )
    .await;
    assert!(matches!(created.outcome, DataOutcomeV1::Ok { .. }));
    let action = call(
        &invocation,
        DataOperationV1::ActionInvoke {
            entity_type: "Temper.Provenance.SolverSession".into(),
            entity_id: id.into(),
            action: "Activate".into(),
            expected_sequence: None,
            params: serde_json::Map::new(),
        },
    )
    .await;
    let DataOutcomeV1::Ok {
        result:
            DataResultV1::Action {
                result: Some(action_value),
                ..
            },
    } = &action.outcome
    else {
        panic!("Activate should return the canonical transitioned entity")
    };
    assert_eq!(action_value["State"], serde_json::json!("Active"));
    assert_eq!(action_value["RegionState"], serde_json::json!("CA"));

    let keyed = call(
        &invocation,
        DataOperationV1::EntityGet {
            entity_type: "Temper.Provenance.SolverSession".into(),
            entity_id: id.into(),
            at_least_sequence: None,
        },
    )
    .await;
    let DataOutcomeV1::Ok {
        result: DataResultV1::Entity { value, .. },
    } = &keyed.outcome
    else {
        panic!("keyed read should return the transitioned entity")
    };
    assert_eq!(value["State"], serde_json::json!("Active"));
    assert_eq!(value["RegionState"], serde_json::json!("CA"));

    assert_generated_client_decodes(generated.source, id, action, keyed);
}

fn assert_generated_client_decodes(
    source: String,
    id: &str,
    action: temper_wasm_sdk::data::DataResponseV1,
    keyed: temper_wasm_sdk::data::DataResponseV1,
) {
    let responses = serde_json::to_string(&vec![action, keyed]).expect("responses serialize");
    let usage = format!(
        r#"
#[test]
fn state_lifecycle_decodes_from_action_and_keyed_read() {{
    let responses: Vec<DataResponseV1> = serde_json::from_str({responses:?}).unwrap();
    install_native_data_host_for_test(responses);
    let mut client = SolverSessionClient::new();
    let activated = client.activate("{id}", None, SolverSessionActivateInput {{}}).unwrap();
    let value = activated.result.unwrap();
    assert_eq!(value.state, "Active");
    assert_eq!(value.region_state, "CA");
    let read = client.get("{id}").unwrap();
    assert_eq!(read.value.state, "Active");
    assert_eq!(read.value.region_state, "CA");
}}
"#
    );
    let temp = tempfile::tempdir().expect("temporary generated crate");
    let sdk_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace crates directory")
        .join("temper-wasm-sdk");
    fs::create_dir(temp.path().join("src")).expect("temporary source directory");
    fs::write(
        temp.path().join("Cargo.toml"),
        format!(
            "[package]\nname='server-lifecycle-provenance-proof'\nversion='0.0.0'\nedition='2024'\n\n[dependencies]\ntemper-wasm-sdk={{path={sdk_path:?},features=['test-helpers']}}\nserde={{version='1',features=['derive']}}\nserde_json='1'\n"
        ),
    )
    .expect("temporary manifest writes");
    fs::write(temp.path().join("src/lib.rs"), format!("{source}\n{usage}"))
        .expect("generated source writes");
    let output = Command::new(env!("CARGO"))
        .args(["test", "--offline", "--quiet"])
        .current_dir(temp.path())
        .output()
        .expect("generated crate test runs");
    assert!(
        output.status.success(),
        "generated client rejected lifecycle responses:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
