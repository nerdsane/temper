use super::*;
use temper_runtime::persistence::{EventMetadata, PersistenceEnvelope, StreamMutability};
use temper_server::state::stream_migration::{
    StreamDescriptorBackfillCandidateV1, StreamDescriptorBackfillOutcomeV1,
};

#[tokio::test]
async fn historical_stream_backfill_is_verified_replayable_and_idempotent() {
    let db_path = std::env::temp_dir().join(format!(
        "temper-stream-backfill-{}.db",
        uuid::Uuid::new_v4()
    ));
    let store = TursoEventStore::new(&format!("file:{}", db_path.display()), None)
        .await
        .expect("create local turso db");
    let legacy_csdl = FILE_CSDL_XML
        .replace(
            "        <Annotation Term=\"Temper.Vocab.Stream.Mutability\" String=\"Mutable\"/>\n",
            "",
        )
        .replace(
            "        <Annotation Term=\"Temper.Vocab.Stream.DescriptorContractVersion\" Int=\"1\"/>\n",
            "",
        );
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        parse_csdl(&legacy_csdl).expect("legacy CSDL parses"),
        legacy_csdl.clone(),
        &[("File", FILE_IOA)],
    );
    let mut state = ServerState::from_registry(ActorSystem::new("stream-backfill"), registry);
    state.set_storage_stack(StorageStack::from_turso(store.clone()));
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .expect("install test policy");
    let data_dir = tempfile::tempdir().expect("temp data dir");
    state.data_dir = data_dir.path().to_path_buf();
    let tenant = TenantId::default();
    let body = b"historical bytes";
    let mut content_hash = format!("sha256:{:x}", Sha256::digest(body));
    let response = state
        .create_file_with_initial_stream_content(
            &tenant,
            "legacy-file",
            serde_json::json!({}),
            body,
            "text/plain",
            &AgentContext::for_service("migration-test"),
        )
        .await
        .expect("legacy stream commit succeeds before activation");
    assert!(
        store
            .read_events("default:File:legacy-file", 0)
            .await
            .unwrap()
            .iter()
            .all(|event| event.metadata.kernel.is_none())
    );

    state.registry.write().unwrap().register_tenant(
        "default",
        parse_csdl(FILE_CSDL_XML).expect("activated CSDL parses"),
        FILE_CSDL_XML.to_string(),
        &[("File", FILE_IOA)],
    );
    let mut candidate = StreamDescriptorBackfillCandidateV1 {
        entity_type: "File".into(),
        entity_id: "legacy-file".into(),
        content_hash: content_hash.clone(),
        storage_object_id: format!("temper-fs/{content_hash}"),
        byte_length: body.len() as u64,
        content_type: Some("text/plain".into()),
        content_event_sequence: response.state.sequence_nr,
        expected_current_sequence: response.state.sequence_nr,
        mutability: StreamMutability::Mutable,
    };
    let mut missing_blob = candidate.clone();
    missing_blob.storage_object_id = "temper-fs/missing".into();
    let unresolved = state
        .backfill_stream_descriptor_inventory_page_v1(
            &tenant,
            "inventory-page-missing",
            true,
            &[missing_blob],
        )
        .await
        .expect("unresolved page is durably reported");
    assert!(!unresolved.migration_complete);
    assert!(matches!(
        unresolved.outcomes.as_slice(),
        [StreamDescriptorBackfillOutcomeV1::Unresolved { reason }]
            if reason.contains("missing")
    ));
    state.registry.write().unwrap().register_tenant(
        "default",
        parse_csdl(&legacy_csdl).expect("legacy CSDL reparses"),
        legacy_csdl,
        &[("File", FILE_IOA)],
    );
    let newer_body = b"newer historical bytes";
    content_hash = format!("sha256:{:x}", Sha256::digest(newer_body));
    let newer = state
        .put_file_stream_content(
            &tenant,
            "legacy-file",
            newer_body,
            "text/plain",
            &AgentContext::for_service("migration-test"),
        )
        .await
        .expect("legacy stream can change while reader-first migration is incomplete");
    candidate.content_hash.clone_from(&content_hash);
    candidate.storage_object_id = format!("temper-fs/{content_hash}");
    candidate.byte_length = newer_body.len() as u64;
    candidate.content_event_sequence = newer.state.sequence_nr;
    candidate.expected_current_sequence = newer.state.sequence_nr;
    state.registry.write().unwrap().register_tenant(
        "default",
        parse_csdl(FILE_CSDL_XML).expect("reactivated CSDL parses"),
        FILE_CSDL_XML.to_string(),
        &[("File", FILE_IOA)],
    );
    let receipt = state
        .backfill_stream_descriptor_inventory_page_v1(
            &tenant,
            "inventory-page-0001",
            true,
            std::slice::from_ref(&candidate),
        )
        .await
        .expect("migration page and report commit");
    assert!(receipt.migration_complete);
    assert_eq!(
        receipt.outcomes,
        vec![StreamDescriptorBackfillOutcomeV1::Appended {
            descriptor_event_sequence: newer.state.sequence_nr + 1,
        }]
    );
    let mut restarted_registry = SpecRegistry::new();
    restarted_registry.register_tenant(
        "default",
        parse_csdl(FILE_CSDL_XML).expect("restart CSDL parses"),
        FILE_CSDL_XML.to_string(),
        &[("File", FILE_IOA)],
    );
    let mut restarted = ServerState::from_registry(
        ActorSystem::new("stream-backfill-restart"),
        restarted_registry,
    );
    restarted.set_storage_stack(StorageStack::from_turso(store.clone()));
    restarted.data_dir = data_dir.path().to_path_buf();
    let replayed = restarted
        .get_tenant_entity_state(&tenant, "File", "legacy-file")
        .await
        .expect("backfilled journal replays after restart");
    assert_eq!(replayed.state.sequence_nr, newer.state.sequence_nr + 1);
    let events = store
        .read_events("default:File:legacy-file", 0)
        .await
        .expect("backfilled journal reads");
    let descriptor = events
        .last()
        .and_then(|event| event.metadata.kernel.as_ref())
        .map(|metadata| metadata.stream_descriptor())
        .expect("backfilled descriptor resolves");
    assert!(descriptor.is_backfill());
    assert_eq!(descriptor.content_hash(), content_hash);
    let rerun = state
        .backfill_stream_descriptor_inventory_page_v1(
            &tenant,
            "inventory-page-0001",
            true,
            &[candidate],
        )
        .await
        .expect("same migration cursor is idempotent");
    assert_eq!(rerun, receipt);
}

#[tokio::test]
async fn immutable_version_migration_derives_parent_and_reuses_deduplicated_blob() {
    const TEMPER_FS_CSDL: &str = include_str!("../../../../os-apps/temper-fs/specs/model.csdl.xml");
    const FILE_VERSION_IOA: &str =
        include_str!("../../../../os-apps/temper-fs/specs/file_version.ioa.toml");

    let db_path = std::env::temp_dir().join(format!(
        "temper-version-backfill-{}.db",
        uuid::Uuid::new_v4()
    ));
    let store = TursoEventStore::new(&format!("file:{}", db_path.display()), None)
        .await
        .expect("create local turso db");
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        parse_csdl(TEMPER_FS_CSDL).expect("TemperFS CSDL parses"),
        TEMPER_FS_CSDL.to_string(),
        &[("File", FILE_IOA), ("FileVersion", FILE_VERSION_IOA)],
    );
    let mut state = ServerState::from_registry(ActorSystem::new("version-backfill"), registry);
    state.set_storage_stack(StorageStack::from_turso(store.clone()));
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .expect("install test policy");
    let data_dir = tempfile::tempdir().expect("temp data dir");
    state.data_dir = data_dir.path().to_path_buf();
    let tenant = TenantId::default();
    let body = b"deduplicated version bytes";
    let content_hash = format!("sha256:{:x}", Sha256::digest(body));
    state
        .create_file_with_initial_stream_content(
            &tenant,
            "file-parent",
            serde_json::json!({}),
            body,
            "text/plain",
            &AgentContext::for_service("migration-test"),
        )
        .await
        .expect("historical current File stores shared blob");
    let version_event = PersistenceEnvelope {
        sequence_nr: 1,
        event_type: "Create".into(),
        payload: serde_json::json!({
            "action": "Create",
            "from_status": "Current",
            "to_status": "Current",
            "timestamp": temper_runtime::scheduler::sim_now(),
            "params": {
                "file_id": "file-parent",
                "version_number": 1,
                "content_hash": content_hash.clone(),
                "mime_type": "text/plain",
                "size_bytes": body.len() as u64,
                "previous_version_id": null,
                "created_by": "migration-test"
            },
            "idempotency_key": null
        }),
        metadata: EventMetadata {
            event_id: uuid::Uuid::new_v4(),
            causation_id: uuid::Uuid::new_v4(),
            correlation_id: uuid::Uuid::new_v4(),
            timestamp: temper_runtime::scheduler::sim_now(),
            actor_id: "default:FileVersion:version-1".into(),
            kernel: None,
        },
    };
    store
        .append("default:FileVersion:version-1", 0, &[version_event])
        .await
        .expect("append historical FileVersion Create");
    let candidate = StreamDescriptorBackfillCandidateV1 {
        entity_type: "FileVersion".into(),
        entity_id: "version-1".into(),
        content_hash: content_hash.clone(),
        storage_object_id: format!("temper-fs/{content_hash}"),
        byte_length: body.len() as u64,
        content_type: Some("text/plain".into()),
        content_event_sequence: 1,
        expected_current_sequence: 1,
        mutability: StreamMutability::Immutable,
    };
    let receipt = state
        .backfill_stream_descriptor_inventory_page_v1(
            &tenant,
            "version-inventory-final",
            true,
            &[candidate],
        )
        .await
        .expect("immutable version migration succeeds");
    assert!(receipt.migration_complete);
    let events = store
        .read_events("default:FileVersion:version-1", 0)
        .await
        .unwrap();
    let descriptor = events
        .last()
        .unwrap()
        .metadata
        .kernel
        .as_ref()
        .unwrap()
        .stream_descriptor();
    assert_eq!(descriptor.mutability(), StreamMutability::Immutable);
    assert_eq!(
        descriptor.authorization_parent().unwrap().entity_id(),
        "file-parent"
    );
}
