use super::*;
use temper_wasm_sdk::data::FileOperationKind;

fn single_entity_grant(
    entity_type: &str,
    operations: impl IntoIterator<Item = DataOperationKind>,
) -> ModuleDataGrant {
    ModuleDataGrant {
        operations: operations.into_iter().collect(),
        entities: vec![EntityDataGrant {
            entity_type: entity_type.into(),
            ..EntityDataGrant::default()
        }],
        ..ModuleDataGrant::default()
    }
}

fn public_surface(source: &str) -> String {
    source
        .lines()
        .filter_map(|line| {
            let line = line.trim_start();
            let (kind, rest) = [
                ("struct", line.strip_prefix("pub struct ")),
                ("enum", line.strip_prefix("pub enum ")),
                ("fn", line.strip_prefix("pub fn ")),
                ("const", line.strip_prefix("pub const ")),
            ]
            .into_iter()
            .find_map(|(kind, rest)| rest.map(|rest| (kind, rest)))?;
            let name = rest
                .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
                .next()
                .expect("public symbol has a name");
            Some(format!("{kind} {name}"))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn assert_public_surface(generated: &GeneratedModuleSdk, golden: &str) {
    assert_eq!(public_surface(&generated.source), golden.trim());
}

fn generated_file_source(
    operations: impl IntoIterator<Item = DataOperationKind>,
    file_operations: impl IntoIterator<Item = FileOperationKind>,
) -> String {
    let mut file_grant = single_entity_grant("Temper.App.File", operations);
    file_grant.entities[0].file_operations = file_operations.into_iter().collect();
    generate_module_sdk(
        &parse_csdl(CSDL).unwrap(),
        "file-client",
        "closure",
        "closure",
        "artifact",
        file_grant,
    )
    .unwrap()
    .source
}

#[test]
fn entity_get_only_surface_matches_golden() {
    let generated = generate_module_sdk(
        &parse_csdl(CSDL).unwrap(),
        "reader",
        "closure",
        "closure",
        "artifact",
        single_entity_grant("Temper.App.Task", [DataOperationKind::EntityGet]),
    )
    .unwrap();

    assert_public_surface(
        &generated,
        include_str!("../../../tests/goldens/module_sdk_entity_get_only.txt"),
    );
    assert!(generated.source.contains("pub fn get("));
    for absent in [
        "TaskCreate",
        "TaskPatch",
        "TaskFilter",
        "TaskOrder",
        "pub fn query(",
        "pub fn create(",
        "pub fn patch(",
        "pub fn start_work(",
    ] {
        assert!(!generated.source.contains(absent), "unexpected {absent}");
    }
}

#[test]
fn one_action_surface_matches_golden() {
    let mut action_grant =
        single_entity_grant("Temper.App.Task", [DataOperationKind::ActionInvoke]);
    action_grant.entities[0].actions.insert("StartWork".into());
    let generated = generate_module_sdk(
        &parse_csdl(CSDL).unwrap(),
        "actor",
        "closure",
        "closure",
        "artifact",
        action_grant,
    )
    .unwrap();

    assert_public_surface(
        &generated,
        include_str!("../../../tests/goldens/module_sdk_one_action.txt"),
    );
    assert!(generated.source.contains("pub fn start_work("));
    assert_eq!(generated.source.matches("Input {").count(), 1);
    for absent in [
        "TaskCreate",
        "TaskPatch",
        "TaskFilter",
        "TaskOrder",
        "pub fn get(",
        "pub fn query(",
        "pub fn create(",
        "pub fn patch(",
        "pub fn reset(",
    ] {
        assert!(!generated.source.contains(absent), "unexpected {absent}");
    }
}

#[test]
fn read_only_file_surface_matches_golden() {
    let mut file_grant = single_entity_grant(
        "Temper.App.File",
        [DataOperationKind::EntityGet, DataOperationKind::FileRead],
    );
    file_grant.entities[0].file_operations = [
        FileOperationKind::MetadataRead,
        FileOperationKind::ContentRead,
    ]
    .into_iter()
    .collect();
    let generated = generate_module_sdk(
        &parse_csdl(CSDL).unwrap(),
        "file-reader",
        "closure",
        "closure",
        "artifact",
        file_grant,
    )
    .unwrap();

    assert_public_surface(
        &generated,
        include_str!("../../../tests/goldens/module_sdk_file_read_only.txt"),
    );
    assert!(generated.source.contains("pub fn get("));
    assert!(generated.source.contains("pub fn open_file_read("));
    for absent in [
        "FileCreate",
        "FilePatch",
        "FileFilter",
        "FileOrder",
        "pub fn query(",
        "pub fn create(",
        "pub fn patch(",
        "pub fn open_file_version_read(",
        "pub fn open_file_write(",
        "pub fn commit_file_write(",
    ] {
        assert!(!generated.source.contains(absent), "unexpected {absent}");
    }
}

#[test]
fn query_surface_contains_only_declared_filter_and_order_fields() {
    let mut query_grant = single_entity_grant("Temper.App.Task", [DataOperationKind::EntityQuery]);
    query_grant.entities[0]
        .query_filter_fields
        .insert("Status".into());
    query_grant.entities[0]
        .query_order_fields
        .insert("Id".into());
    let generated = generate_module_sdk(
        &parse_csdl(CSDL).unwrap(),
        "query",
        "closure",
        "closure",
        "artifact",
        query_grant,
    )
    .unwrap();

    assert!(generated.source.contains("pub struct TaskFilter"));
    assert!(generated.source.contains("pub fn status_eq("));
    assert!(!generated.source.contains("pub fn id_eq("));
    assert!(generated.source.contains("pub struct TaskOrder"));
    assert!(generated.source.contains("pub fn id("));
    assert!(!generated.source.contains("pub fn status("));
    assert!(generated.source.contains("pub fn query("));
    assert!(!generated.source.contains("TaskCreate"));
    assert!(!generated.source.contains("TaskPatch"));
}

#[test]
fn file_read_methods_are_split_by_exact_file_operation() {
    let content = generated_file_source(
        [DataOperationKind::FileRead],
        [FileOperationKind::ContentRead],
    );
    assert!(content.contains("pub fn open_file_read("));
    assert!(!content.contains("pub fn open_file_version_read("));
    assert!(!content.contains("pub struct File {"));

    let version = generated_file_source(
        [DataOperationKind::FileRead],
        [FileOperationKind::VersionRead],
    );
    assert!(!version.contains("pub fn open_file_read("));
    assert!(version.contains("pub fn open_file_version_read("));
    assert!(!version.contains("pub struct File {"));
}

#[test]
fn every_file_api_requires_global_and_entity_capabilities() {
    let global_metadata_without_file_capability = generated_file_source(
        [DataOperationKind::EntityGet, DataOperationKind::EntityQuery],
        [],
    );
    for absent in [
        "pub struct File {",
        "FileFilter",
        "FileOrder",
        "pub fn get(",
        "pub fn query(",
    ] {
        assert!(
            !global_metadata_without_file_capability.contains(absent),
            "unexpected {absent}"
        );
    }

    let metadata_without_global = generated_file_source([], [FileOperationKind::MetadataRead]);
    assert!(!metadata_without_global.contains("pub fn get("));
    assert!(!metadata_without_global.contains("pub fn query("));

    let global_read_without_file_capability =
        generated_file_source([DataOperationKind::FileRead], []);
    assert!(!global_read_without_file_capability.contains("pub fn open_file_read("));
    assert!(!global_read_without_file_capability.contains("pub fn open_file_version_read("));

    for file_operation in [
        FileOperationKind::ContentRead,
        FileOperationKind::VersionRead,
    ] {
        let file_capability_without_global = generated_file_source([], [file_operation]);
        assert!(!file_capability_without_global.contains("pub fn open_file_read("));
        assert!(!file_capability_without_global.contains("pub fn open_file_version_read("));
    }

    let global_write_without_file_capability =
        generated_file_source([DataOperationKind::FileWrite], []);
    assert!(!global_write_without_file_capability.contains("pub fn open_file_write("));
    assert!(!global_write_without_file_capability.contains("pub fn commit_file_write("));
    assert!(!global_write_without_file_capability.contains("pub fn abort_file_stream("));

    let write_capability_without_global =
        generated_file_source([], [FileOperationKind::ContentWrite]);
    assert!(!write_capability_without_global.contains("pub fn open_file_write("));
    assert!(!write_capability_without_global.contains("pub fn commit_file_write("));
    assert!(!write_capability_without_global.contains("pub fn abort_file_stream("));
}
