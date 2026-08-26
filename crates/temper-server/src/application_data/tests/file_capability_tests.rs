use super::*;

#[tokio::test]
async fn unrelated_entity_file_operations_cannot_authorize_file_content() {
    let mut invocation = invocation(
        BTreeSet::from([DataOperationKind::FileRead]),
        SecurityContext::system(),
    );
    std::sync::Arc::get_mut(&mut invocation)
        .expect("unshared fixture")
        .authority
        .binding
        .grant
        .entities[0]
        .file_operations
        .insert(FileOperationKind::ContentRead);
    let response = call(
        &invocation,
        DataOperationV1::FileReadOpen {
            file_id: "file-1".into(),
            version_id: None,
        },
    )
    .await;
    assert_eq!(
        response_error(response).code,
        "FileCapabilityDenied",
        "a non-File entity grant must not authorize hard-coded File actors"
    );
}

#[tokio::test]
async fn file_metadata_reads_require_metadata_capability_before_cedar() {
    let mut invocation = invocation(
        BTreeSet::from([DataOperationKind::EntityGet, DataOperationKind::EntityQuery]),
        SecurityContext::system(),
    );
    std::sync::Arc::get_mut(&mut invocation)
        .expect("unshared fixture")
        .authority
        .binding
        .grant
        .entities
        .push(EntityDataGrant {
            entity_type: "Temper.FileSystem.File".into(),
            ..EntityDataGrant::default()
        });

    let get = call(
        &invocation,
        DataOperationV1::EntityGet {
            entity_type: "Temper.FileSystem.File".into(),
            entity_id: "file-1".into(),
            at_least_sequence: None,
        },
    )
    .await;
    let get_error = response_error(get);
    assert_eq!(get_error.kind, ModuleDataErrorKind::AuthorizationDenied);
    assert_eq!(get_error.code, "CapabilityDenied");

    let query = call(
        &invocation,
        DataOperationV1::EntityQuery {
            entity_type: "Temper.FileSystem.File".into(),
            filter: None,
            order_by: Vec::new(),
            page: temper_wasm_sdk::data::PageV1 {
                limit: 10,
                cursor: None,
            },
        },
    )
    .await;
    let query_error = response_error(query);
    assert_eq!(query_error.kind, ModuleDataErrorKind::AuthorizationDenied);
    assert_eq!(query_error.code, "CapabilityDenied");

    std::sync::Arc::get_mut(&mut invocation)
        .expect("unshared fixture")
        .authority
        .binding
        .grant
        .entities[1]
        .file_operations
        .insert(FileOperationKind::MetadataRead);
    assert!(
        invocation
            .require(DataOperationKind::EntityGet, "Temper.FileSystem.File", None)
            .is_ok()
    );
    assert!(
        invocation
            .require(
                DataOperationKind::EntityQuery,
                "Temper.FileSystem.File",
                None
            )
            .is_ok()
    );
}
