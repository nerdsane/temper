//! Compile the generator's real output for a representative closed SDK surface.

use std::collections::BTreeSet;
use std::fs;
use std::process::Command;

use temper_codegen::generate_module_sdk;
use temper_spec::csdl::parse_csdl;
use temper_wasm_sdk::data::{
    DataOperationKind, EntityDataGrant, FileOperationKind, ModuleDataGrant,
};

const CSDL: &str = r#"
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Generated" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EnumType Name="TaskStatus"><Member Name="Open"/><Member Name="Done"/></EnumType>
      <EntityType Name="User"><Key><PropertyRef Name="Id"/></Key><Property Name="Id" Type="Edm.String" Nullable="false"/></EntityType>
      <EntityType Name="File" HasStream="true">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Temper.Generated.TaskStatus" Nullable="false"/>
        <Property Name="Owner" Type="Temper.Generated.User" Nullable="false"/>
        <Property Name="Estimate" Type="Edm.Decimal" Nullable="true"/>
        <Property Name="CreatedAt" Type="Edm.DateTimeOffset" Nullable="false"/>
      </EntityType>
      <Action Name="Complete" IsBound="true">
        <Parameter Name="binding" Type="Temper.Generated.File" Nullable="false"/>
        <Parameter Name="Note" Type="Edm.String" Nullable="true"/>
        <ReturnType Type="Temper.Generated.TaskStatus" Nullable="false"/>
      </Action>
      <Action Name="Snapshot" IsBound="true">
        <Parameter Name="binding" Type="Temper.Generated.File" Nullable="false"/>
        <ReturnType Type="Temper.Generated.File" Nullable="false"/>
      </Action>
      <EntityContainer Name="Container">
        <EntitySet Name="Files" EntityType="Temper.Generated.File"/>
        <EntitySet Name="Users" EntityType="Temper.Generated.User"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>
"#;

#[test]
fn representative_generated_surface_compiles() {
    let csdl = parse_csdl(CSDL).unwrap();
    let generated = generate_module_sdk(
        &csdl,
        "worker",
        "closure",
        "closure",
        "unpackaged",
        ModuleDataGrant {
            operations: [
                DataOperationKind::EntityGet,
                DataOperationKind::EntityQuery,
                DataOperationKind::EntityCreate,
                DataOperationKind::EntityPatch,
                DataOperationKind::ActionInvoke,
                DataOperationKind::Batch,
                DataOperationKind::FileRead,
                DataOperationKind::FileWrite,
            ]
            .into_iter()
            .collect(),
            entities: vec![EntityDataGrant {
                entity_type: "Temper.Generated.File".into(),
                actions: BTreeSet::from(["Complete".into(), "Snapshot".into()]),
                query_filter_fields: BTreeSet::from(["Status".into(), "Estimate".into()]),
                query_order_fields: BTreeSet::from(["CreatedAt".into(), "Estimate".into()]),
                query_order_by_sequence: true,
                file_operations: BTreeSet::from([
                    FileOperationKind::ContentRead,
                    FileOperationKind::ContentWrite,
                ]),
                ..EntityDataGrant::default()
            }],
            ..ModuleDataGrant::default()
        },
    )
    .unwrap();

    let temp = tempfile::tempdir().unwrap();
    let sdk_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("temper-wasm-sdk");
    fs::create_dir(temp.path().join("src")).unwrap();
    fs::write(
        temp.path().join("Cargo.toml"),
        format!(
            "[package]\nname='generated-sdk-proof'\nversion='0.0.0'\nedition='2024'\n\n[dependencies]\ntemper-wasm-sdk={{path={sdk_path:?},features=['test-helpers']}}\nserde={{version='1',features=['derive']}}\nserde_json='1'\n"
        ),
    )
    .unwrap();
    let usage = r#"
pub fn typecheck_surface() {
    let _id = FileId("file-1".into());
    let _owner = TemperGeneratedUserId("user-1".into());
    let _status = TemperGeneratedTaskStatus::Open;
    let filter = FileFilter::status_eq(TemperGeneratedTaskStatus::Open);
    let order = FileOrder::created_at(OrderDirectionV1::Asc);
    let sequence_order = FileOrder::commit_sequence(OrderDirectionV1::Desc);
    let _query: fn(&mut FileClient, Option<FileFilter>, Vec<FileOrder>, PageV1) -> Result<TypedPage<File>, ModuleDataError> = FileClient::query;
    let _entity_action: fn(&mut FileClient, String, Option<u64>, FileSnapshotInput) -> Result<TypedAction<File>, ModuleDataError> = FileClient::snapshot;
    let _ = (filter, order, sequence_order);
    let _batch: fn(&mut DataClient, Vec<BatchItemV1>) -> Result<DataResultV1, ModuleDataError> = execute_batch;
    let _read: fn(&mut FileClient, String, Option<String>) -> Result<OpenedFileRead, ModuleDataError> = FileClient::open_file_read;
}

#[test]
fn generated_entity_action_decodes_and_advances_the_next_read() {
    let value = serde_json::json!({
        "Id": "file-1",
        "Status": "Done",
        "Owner": "user-1",
        "Estimate": null,
        "CreatedAt": "2026-08-24T12:00:00Z"
    });
    let commit = CommitToken {
        entity_type: FileClient::ENTITY_TYPE.into(),
        entity_id: "file-1".into(),
        sequence: 9,
    };
    install_native_data_host_for_test(vec![
        DataResponseV1::ok(DataResultV1::Action {
            commit: commit.clone(),
            result: Some(value.clone()),
            result_omitted: false,
        }),
        DataResponseV1::ok(DataResultV1::Entity {
            value: value.as_object().cloned().expect("fixture entity is an object"),
            sequence: commit.sequence,
        }),
    ]);

    let mut client = FileClient::new();
    let action = client
        .snapshot("file-1", None, FileSnapshotInput {})
        .expect("generated entity action decodes");
    assert_eq!(action.commit, commit);
    assert_eq!(action.result.expect("entity result").id, "file-1");
    let read = client.get("file-1").expect("sequence-aware keyed read");
    assert_eq!(read.sequence, commit.sequence);
    assert_eq!(read.value.status, TemperGeneratedTaskStatus::Done);

    let requests = take_native_data_requests_for_test();
    assert_eq!(
        requests
            .iter()
            .filter(|request| matches!(request.operation, DataOperationV1::ActionInvoke { .. }))
            .count(),
        1,
        "the generated client must invoke the committed action exactly once"
    );
    assert!(matches!(
        &requests[1].operation,
        DataOperationV1::EntityGet {
            at_least_sequence: Some(9),
            ..
        }
    ));
}
"#;
    fs::write(
        temp.path().join("src/lib.rs"),
        format!("{}\n{}", generated.source, usage),
    )
    .unwrap();
    let output = Command::new(env!("CARGO"))
        .args(["test", "--offline", "--quiet"])
        .current_dir(temp.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "generated SDK failed to compile:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
