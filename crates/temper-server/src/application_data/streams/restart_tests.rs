use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};

use sha2::{Digest as _, Sha256};
use temper_authz::SecurityContext;
use temper_runtime::persistence::schema_deployment::{
    StreamPublicationFence, UnscopedStreamPublicationBinding,
};
use temper_runtime::persistence::{
    EventMetadata, EventStore, PersistenceEnvelope, StreamMutability,
};
use temper_runtime::{ActorSystem, TenantId};
use temper_spec::bundle::IoaSourceInput;
use temper_spec::csdl::parse_csdl;
use temper_store_sim::{SimEventStore, SimFaultConfig};
use temper_store_turso::TursoEventStore;
use temper_wasm_sdk::data::{
    DataOperationKind, DataOperationV1, DataRequestV1, EntityDataGrant, FileOperationKind,
    ModuleDataGrant, ModuleSdkManifest,
};

use crate::ServerState;
use crate::application_data::{ApplicationDataInvocation, ModuleInvocationAuthority};
use crate::registry::SpecRegistry;
use crate::request_context::AgentContext;
use crate::state::stream_migration::StreamDescriptorBackfillCandidateV1;
use crate::storage::StorageStack;

const TEMPER_FS_CSDL: &str = include_str!("../../../../../os-apps/temper-fs/specs/model.csdl.xml");
const FILE_IOA: &str = include_str!("../../../../../os-apps/temper-fs/specs/file.ioa.toml");
const FILE_VERSION_IOA: &str =
    include_str!("../../../../../os-apps/temper-fs/specs/file_version.ioa.toml");

fn activated_csdl() -> String {
    TEMPER_FS_CSDL
        .replace(
            "<Annotation Term=\"Temper.Vocab.Stream.Mutability\" String=\"Mutable\"/>",
            "<Annotation Term=\"Temper.Vocab.Stream.Mutability\" String=\"Mutable\"/>\n        <Annotation Term=\"Temper.Vocab.Stream.DescriptorContractVersion\" Int=\"1\"/>",
        )
        .replace(
            "<Annotation Term=\"Temper.Vocab.Stream.Mutability\" String=\"Immutable\"/>",
            "<Annotation Term=\"Temper.Vocab.Stream.Mutability\" String=\"Immutable\"/>\n        <Annotation Term=\"Temper.Vocab.Stream.DescriptorContractVersion\" Int=\"1\"/>",
        )
}

fn registry(csdl: &str) -> SpecRegistry {
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        parse_csdl(csdl).unwrap(),
        csdl.to_string(),
        &[("File", FILE_IOA), ("FileVersion", FILE_VERSION_IOA)],
    );
    registry
}

fn stream_grant() -> ModuleDataGrant {
    ModuleDataGrant {
        operations: BTreeSet::from([DataOperationKind::FileRead]),
        entities: vec![EntityDataGrant {
            entity_type: "Temper.FS.File".into(),
            file_operations: BTreeSet::from([
                FileOperationKind::ContentRead,
                FileOperationKind::VersionRead,
            ]),
            ..EntityDataGrant::default()
        }],
        ..ModuleDataGrant::default()
    }
}

fn invocation(state: ServerState, binding: ModuleSdkManifest) -> Arc<ApplicationDataInvocation> {
    ApplicationDataInvocation::new(
        state,
        ModuleInvocationAuthority::new(
            TenantId::default(),
            "stream-restart-test".into(),
            "artifact".into(),
            "StreamUpdated".into(),
            "File".into(),
            SecurityContext::system(),
            binding,
        ),
    )
}

#[tokio::test]
async fn typed_current_and_version_reads_survive_restart_and_reject_cross_file() {
    let csdl = activated_csdl();
    let generated = temper_codegen::generate_module_sdk(
        &parse_csdl(&csdl).unwrap(),
        &[
            IoaSourceInput {
                entity_type: "Temper.FS.File".into(),
                source: FILE_IOA.into(),
            },
            IoaSourceInput {
                entity_type: "Temper.FS.FileVersion".into(),
                source: FILE_VERSION_IOA.into(),
            },
        ],
        "stream-restart-test",
        "closure",
        "closure",
        "artifact",
        stream_grant(),
    )
    .unwrap();
    let db_path = std::env::temp_dir().join(format!(
        "temper-typed-stream-restart-{}.db",
        uuid::Uuid::new_v4()
    ));
    let store = TursoEventStore::new(&format!("file:{}", db_path.display()), None)
        .await
        .unwrap();
    let mut state =
        ServerState::from_registry(ActorSystem::new("typed-stream-write"), registry(&csdl));
    state.set_storage_stack(StorageStack::from_turso(store.clone()));
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .unwrap();
    let data_dir = tempfile::tempdir().unwrap();
    state.data_dir = data_dir.path().to_path_buf();
    let tenant = TenantId::default();
    let body = b"typed restart bytes";
    let content_hash = format!("sha256:{:x}", Sha256::digest(body));
    for file_id in ["file-1", "file-2"] {
        state
            .create_file_with_initial_stream_content(
                &tenant,
                file_id,
                serde_json::json!({}),
                body,
                "text/plain",
                &AgentContext::for_service("typed-restart-test"),
            )
            .await
            .unwrap();
        let persistence_id = format!("default:File:{file_id}");
        let mut settled_events = None;
        for _ in 0..256 {
            let events = store.read_events(&persistence_id, 0).await.unwrap();
            if events
                .iter()
                .any(|event| event.event_type == "RecordVersion")
            {
                settled_events = Some(events);
                break;
            }
            tokio::task::yield_now().await;
        }
        let events = settled_events.expect("File.RecordVersion reaction did not settle");
        let content_event_sequence = events
            .iter()
            .rev()
            .find(|event| event.event_type == "StreamUpdated")
            .unwrap()
            .sequence_nr;
        let receipt = state
            .backfill_stream_descriptor_inventory_page_v1(
                &tenant,
                &format!("typed-file-{file_id}-final"),
                true,
                &[StreamDescriptorBackfillCandidateV1 {
                    entity_type: "File".into(),
                    entity_id: file_id.into(),
                    content_hash: content_hash.clone(),
                    storage_object_id: format!("temper-fs/{content_hash}"),
                    byte_length: body.len() as u64,
                    content_type: Some("text/plain".into()),
                    content_event_sequence,
                    expected_current_sequence: events.last().unwrap().sequence_nr,
                    mutability: StreamMutability::Mutable,
                }],
            )
            .await
            .unwrap();
        assert!(receipt.migration_complete, "{receipt:?}");
    }
    store
        .append(
            "default:FileVersion:version-1",
            0,
            &[PersistenceEnvelope {
                sequence_nr: 1,
                event_type: "Create".into(),
                payload: serde_json::json!({
                    "action": "Create",
                    "from_status": "Current",
                    "to_status": "Current",
                    "timestamp": temper_runtime::scheduler::sim_now(),
                    "params": {
                        "file_id": "file-1",
                        "version_number": 1,
                        "content_hash": content_hash.clone(),
                        "mime_type": "text/plain",
                        "size_bytes": body.len() as u64,
                        "previous_version_id": null,
                        "created_by": "typed-restart-test"
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
            }],
        )
        .await
        .unwrap();
    let receipt = state
        .backfill_stream_descriptor_inventory_page_v1(
            &tenant,
            "typed-version-final",
            true,
            &[StreamDescriptorBackfillCandidateV1 {
                entity_type: "FileVersion".into(),
                entity_id: "version-1".into(),
                content_hash: content_hash.clone(),
                storage_object_id: format!("temper-fs/{content_hash}"),
                byte_length: body.len() as u64,
                content_type: Some("text/plain".into()),
                content_event_sequence: 1,
                expected_current_sequence: 1,
                mutability: StreamMutability::Immutable,
            }],
        )
        .await
        .unwrap();
    assert!(receipt.migration_complete, "{receipt:?}");
    let capabilities = temper_spec::csdl::verify_stream_capabilities_v1(
        &temper_spec::csdl::parse_csdl(&csdl).unwrap(),
    )
    .unwrap();
    let mut bindings = BTreeMap::new();
    for capability in capabilities
        .iter()
        .filter(|capability| capability.descriptor_contract_v1_active)
    {
        let entity_type = capability.subject_type.rsplit('.').next().unwrap();
        let provenance = capability.migration_provenance.as_ref().unwrap();
        bindings.insert(
            entity_type.to_string(),
            UnscopedStreamPublicationBinding {
                publication_action: provenance.publication_action.clone(),
                capability_digest: temper_spec::csdl::stream_capability_set_digest_v1(
                    std::slice::from_ref(capability),
                )
                .unwrap(),
                expected_write_version: store
                    .unscoped_entity_type_write_version("default", entity_type)
                    .await
                    .unwrap(),
            },
        );
    }
    store
        .activate_unscoped_stream_publication_fence(
            "default",
            &StreamPublicationFence::InstalledApplication {
                application_id: "temper-fs".into(),
                semantic_digest: format!("sha256:{}", "f".repeat(64)),
                bindings,
            },
        )
        .await
        .unwrap();

    let mut restarted =
        ServerState::from_registry(ActorSystem::new("typed-stream-restart"), registry(&csdl));
    restarted.set_storage_stack(StorageStack::from_turso(store));
    restarted.data_dir = data_dir.path().to_path_buf();
    restarted
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .unwrap();
    let invocation = invocation(restarted, generated.manifest.clone());
    let blob_path = data_dir
        .path()
        .join("blobs")
        .join("temper-fs")
        .join(&content_hash);
    run_generated_stream_client(
        invocation,
        &generated.source,
        &generated.manifest,
        &blob_path,
    )
    .await;
}

async fn run_generated_stream_client(
    invocation: Arc<ApplicationDataInvocation>,
    generated_source: &str,
    manifest: &ModuleSdkManifest,
    blob_path: &std::path::Path,
) {
    let wasm = compile_generated_stream_guest(generated_source);
    let engine = temper_wasm::WasmEngine::new().unwrap();
    let module_hash = engine.compile_and_cache(&wasm).unwrap();
    let (data_service, read_service, write_service) = invocation.callbacks();
    let open_calls = Arc::new(AtomicUsize::new(0));
    let counted_open_calls = Arc::clone(&open_calls);
    let blob_path = blob_path.to_path_buf();
    let proof_blob_path = blob_path.clone();
    let data_service: temper_wasm::TemperDataCallFn = Arc::new(move |bytes| {
        let service = Arc::clone(&data_service);
        let open_index = serde_json::from_slice::<DataRequestV1>(&bytes)
            .ok()
            .filter(|request| matches!(request.operation, DataOperationV1::FileReadOpen { .. }))
            .map(|_| counted_open_calls.fetch_add(1, Ordering::SeqCst));
        let blob_path = blob_path.clone();
        Box::pin(async move {
            if open_index == Some(2) {
                tokio::fs::remove_file(&blob_path)
                    .await
                    .map_err(|error| format!("failed to arm pre-I/O ownership proof: {error}"))?;
            }
            service(bytes).await
        })
    });
    let host = temper_wasm::ProductionWasmHost::new(BTreeMap::new()).with_temper_data_service(
        data_service,
        read_service,
        write_service,
        &manifest.grant.budgets,
    );
    let result = engine
        .invoke(
            &module_hash,
            &temper_wasm::WasmInvocationContext {
                tenant: "default".into(),
                entity_type: "File".into(),
                entity_id: "file-1".into(),
                trigger_action: "StreamReadProof".into(),
                wasm_module: Some("stream-restart-test".into()),
                trigger_params: serde_json::Value::Null,
                entity_state: serde_json::Value::Null,
                agent_id: None,
                session_id: None,
                integration_config: BTreeMap::new(),
                trace_id: String::new(),
                workflow_root_entity_type: None,
                workflow_root_entity_id: None,
                workflow_run_id: None,
                http_request: None,
            },
            Arc::new(host),
            &temper_wasm::WasmResourceLimits::default(),
            Arc::new(RwLock::new(temper_wasm::StreamRegistry::default())),
        )
        .await
        .unwrap();
    assert!(
        result.success,
        "generated client failed: {:?}",
        result.error
    );
    assert_eq!(result.callback_params["verified"], true);
    assert_eq!(open_calls.load(Ordering::SeqCst), 3);
    assert!(
        !proof_blob_path.exists(),
        "cross-File rejection proof must remove the blob before the third open"
    );
}

fn compile_generated_stream_guest(generated_source: &str) -> Vec<u8> {
    let guest = r#"
fn read_all(mut opened: OpenedFileRead) -> Result<Vec<u8>, ModuleDataError> {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 7];
    loop {
        let read = opened.reader.read(&mut chunk)?;
        if read == 0 {
            return Ok(bytes);
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn run(_ctx_ptr: i32, _ctx_len: i32) -> i32 {
    let result = (|| -> Result<(), String> {
        let mut client = FileClient::new();
        let current = client
            .open_file_read("file-1")
            .map_err(|error| format!("current read: {error}"))?;
        if read_all(current).map_err(|error| error.to_string())? != b"typed restart bytes" {
            return Err("generated current File read returned different bytes".into());
        }
        let version = client
            .open_file_version_read("file-1", "version-1")
            .map_err(|error| format!("version read: {error}"))?;
        if read_all(version).map_err(|error| error.to_string())? != b"typed restart bytes" {
            return Err("generated immutable FileVersion read returned different bytes".into());
        }
        let mismatch = client
            .open_file_version_read("file-2", "version-1")
            .expect_err("cross-File version must be rejected");
        if mismatch.code != "FileVersionMismatch" {
            return Err(format!("unexpected cross-File error: {}", mismatch.code));
        }
        Ok(())
    })();
    match result {
        Ok(()) => temper_wasm_sdk::set_success_result(
            "callback",
            &temper_wasm_sdk::json!({"verified": true}),
        ),
        Err(error) => temper_wasm_sdk::set_error_result(&error),
    }
    0
}
"#;
    let temp = tempfile::tempdir().unwrap();
    let sdk_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("temper-wasm-sdk");
    fs::create_dir(temp.path().join("src")).unwrap();
    fs::write(
        temp.path().join("Cargo.toml"),
        format!(
            "[package]\nname='generated-stream-restart-guest'\nversion='0.0.0'\nedition='2024'\n\n[lib]\ncrate-type=['cdylib']\n\n[dependencies]\ntemper-wasm-sdk={{path={sdk_path:?}}}\nserde={{version='1',features=['derive']}}\nserde_json='1'\n"
        ),
    )
    .unwrap();
    fs::write(
        temp.path().join("src/lib.rs"),
        format!("{generated_source}\n{guest}"),
    )
    .unwrap();
    let output = Command::new(env!("CARGO"))
        .args([
            "build",
            "--target",
            "wasm32-unknown-unknown",
            "--release",
            "--offline",
            "--quiet",
        ])
        .current_dir(temp.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "generated stream guest failed to compile:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    fs::read(
        temp.path()
            .join("target/wasm32-unknown-unknown/release/generated_stream_restart_guest.wasm"),
    )
    .unwrap()
}

#[tokio::test]
async fn append_failure_after_blob_durability_publishes_no_descriptor() {
    let csdl = activated_csdl();
    let sim = SimEventStore::new(
        1_188,
        SimFaultConfig {
            write_failure_prob: 1.0,
            ..SimFaultConfig::none()
        },
    );
    let mut state =
        ServerState::from_registry(ActorSystem::new("stream-append-failure"), registry(&csdl));
    state.set_storage_stack(StorageStack::from_sim(sim.clone(), None));
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .unwrap();
    let data_dir = tempfile::tempdir().unwrap();
    state.data_dir = data_dir.path().to_path_buf();
    let tenant = TenantId::default();
    let body = b"orphaned but unpublished bytes";
    let content_hash = format!("sha256:{:x}", Sha256::digest(body));
    assert!(
        state
            .create_file_with_initial_stream_content(
                &tenant,
                "failed-file",
                serde_json::json!({}),
                body,
                "text/plain",
                &AgentContext::for_service("fault-test"),
            )
            .await
            .is_err()
    );
    sim.disable_faults();
    assert!(sim.dump_journal("default:File:failed-file").is_empty());
    assert_eq!(
        state
            .get_blob_with_legacy_fallback(&tenant, &format!("temper-fs/{content_hash}"),)
            .await
            .unwrap(),
        Some(body.to_vec())
    );
    assert!(
        state
            .resolve_stream_descriptor(&tenant, "File", "failed-file")
            .await
            .is_err()
    );
}
