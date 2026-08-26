use std::collections::BTreeSet;
use std::fs;
use std::process::Command;

use temper_authz::SecurityContext;
use temper_wasm_sdk::data::{
    DataOperationKind, DataOperationV1, DataOutcomeV1, EntityDataGrant, ManifestPropertyV1,
    ModuleDataErrorKind, ModuleDataGrant,
};

use super::tests::{CSDL, IOA, call, invocation, response_error};

#[derive(Debug, serde::Deserialize)]
struct GeneratedCustomer {
    #[serde(rename = "Id")]
    id: String,
    #[serde(rename = "Name")]
    name: Option<String>,
    #[serde(rename = "Status")]
    status: Option<String>,
    #[serde(rename = "RenameCount")]
    rename_count: Option<i64>,
    #[serde(rename = "FailureReason")]
    failure_reason: String,
    #[serde(rename = "Label")]
    label: String,
    #[serde(rename = "AttemptCount")]
    attempt_count: i64,
    #[serde(rename = "Enabled")]
    enabled: bool,
    #[serde(rename = "Phase")]
    phase: String,
}

pub(super) fn assert_generated_customer_defaults(
    value: serde_json::Value,
    expected_name: Option<&str>,
) {
    let customer: GeneratedCustomer =
        serde_json::from_value(value).expect("generated required fields must decode");
    assert!(!customer.id.is_empty());
    assert_eq!(customer.name.as_deref(), expected_name);
    assert_eq!(customer.status.as_deref(), Some("Active"));
    assert!(customer.rename_count.unwrap_or_default() >= 0);
    assert_eq!(customer.failure_reason, "");
    assert_eq!(customer.label, "unknown");
    assert_eq!(customer.attempt_count, 0);
    assert!(!customer.enabled);
    assert_eq!(customer.phase, "Ready");
}

#[tokio::test]
async fn sparse_server_responses_decode_through_the_generated_client() {
    let operations = BTreeSet::from([
        DataOperationKind::ActionInvoke,
        DataOperationKind::EntityCreate,
        DataOperationKind::EntityGet,
    ]);
    let invocation = invocation(operations.clone(), SecurityContext::system());
    let id = "018f1f80-7b2d-7000-8000-000000000001";
    let created = call(
        &invocation,
        DataOperationV1::EntityCreate {
            entity_type: "Temper.Example.Customer".into(),
            value: serde_json::json!({"Id": id})
                .as_object()
                .cloned()
                .expect("fixture value is an object"),
        },
    )
    .await;
    assert!(matches!(created.outcome, DataOutcomeV1::Ok { .. }));
    let keyed = call(
        &invocation,
        DataOperationV1::EntityGet {
            entity_type: "Temper.Example.Customer".into(),
            entity_id: id.into(),
            at_least_sequence: None,
        },
    )
    .await;
    let action = call(
        &invocation,
        DataOperationV1::ActionInvoke {
            entity_type: "Temper.Example.Customer".into(),
            entity_id: id.into(),
            action: "Rename".into(),
            expected_sequence: None,
            params: serde_json::json!({"Name": "Ada"})
                .as_object()
                .cloned()
                .expect("fixture params are an object"),
        },
    )
    .await;

    let generated = temper_codegen::generate_module_sdk(
        &temper_spec::csdl::parse_csdl(CSDL).expect("fixture CSDL parses"),
        &[temper_spec::bundle::IoaSourceInput {
            entity_type: "Temper.Example.Customer".into(),
            source: IOA.into(),
        }],
        "worker",
        "closure",
        "closure",
        "artifact",
        ModuleDataGrant {
            operations,
            entities: vec![EntityDataGrant {
                entity_type: "Temper.Example.Customer".into(),
                actions: BTreeSet::from(["Rename".into()]),
                ..EntityDataGrant::default()
            }],
            ..ModuleDataGrant::default()
        },
    )
    .expect("fixture SDK generates");
    let responses = serde_json::to_string(&vec![keyed, action]).expect("responses serialize");
    let usage = format!(
        r#"
#[test]
fn server_materialized_keyed_and_action_responses_decode() {{
    let responses: Vec<DataResponseV1> = serde_json::from_str({responses:?}).unwrap();
    install_native_data_host_for_test(responses);
    let mut client = CustomerClient::new();
    let read = client.get("{id}").expect("generated keyed read decodes");
    assert_eq!(read.value.failure_reason, "");
    assert_eq!(read.value.label, "unknown");
    assert_eq!(read.value.attempt_count, 0);
    assert!(!read.value.enabled);
    assert!(matches!(read.value.phase, TemperExamplePhase::Ready));
    let renamed = client.rename(
        "{id}",
        None,
        CustomerRenameInput {{ name: "Ada".into() }},
    ).expect("generated entity action result decodes");
    let customer = renamed.result.expect("entity-valued action returns a customer");
    assert_eq!(customer.failure_reason, "");
    assert_eq!(customer.label, "unknown");
    assert_eq!(customer.attempt_count, 0);
    assert!(!customer.enabled);
    assert!(matches!(customer.phase, TemperExamplePhase::Ready));
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
            "[package]\nname='server-generated-default-proof'\nversion='0.0.0'\nedition='2024'\n\n[dependencies]\ntemper-wasm-sdk={{path={sdk_path:?},features=['test-helpers']}}\nserde={{version='1',features=['derive']}}\nserde_json='1'\n"
        ),
    )
    .expect("temporary manifest writes");
    fs::write(
        temp.path().join("src/lib.rs"),
        format!("{}\n{usage}", generated.source),
    )
    .expect("generated source writes");
    let output = Command::new(env!("CARGO"))
        .args(["test", "--offline", "--quiet"])
        .current_dir(temp.path())
        .output()
        .expect("generated crate test runs");
    assert!(
        output.status.success(),
        "generated client rejected a server response:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn canonical_read_fails_closed_when_required_property_has_no_value_or_default() {
    let mut invocation = invocation(
        BTreeSet::from([
            DataOperationKind::EntityCreate,
            DataOperationKind::EntityGet,
        ]),
        SecurityContext::system(),
    );
    let id = "018f1f80-7b2d-7000-8000-000000000001";
    let created = call(
        &invocation,
        DataOperationV1::EntityCreate {
            entity_type: "Temper.Example.Customer".into(),
            value: serde_json::json!({"Id": id})
                .as_object()
                .cloned()
                .expect("fixture value is an object"),
        },
    )
    .await;
    assert!(matches!(created.outcome, DataOutcomeV1::Ok { .. }));

    std::sync::Arc::get_mut(&mut invocation)
        .expect("fixture invocation is unshared")
        .authority
        .binding
        .entities[0]
        .properties
        .push(ManifestPropertyV1 {
            canonical_name: "RequiredWithoutDefault".into(),
            generated_name: "required_without_default".into(),
            type_name: "Edm.String".into(),
            nullable: false,
            source: temper_wasm_sdk::data::ManifestValueSourceV1::StoredField,
            default_value: None,
            enum_members: Vec::new(),
        });
    let response = call(
        &invocation,
        DataOperationV1::EntityGet {
            entity_type: "Temper.Example.Customer".into(),
            entity_id: id.into(),
            at_least_sequence: None,
        },
    )
    .await;
    let error = response_error(response);
    assert_eq!(error.kind, ModuleDataErrorKind::SchemaMismatch);
    assert_eq!(error.code, "MissingRequiredProperty");
}
