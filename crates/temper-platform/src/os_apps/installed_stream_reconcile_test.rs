use temper_runtime::persistence::{EventMetadata, PersistenceEnvelope};
use temper_runtime::scheduler::{sim_now, sim_uuid};
use temper_runtime::tenant::TenantId;
use temper_wasm_sdk::schema_deployment::{
    AdvanceStreamDescriptorMigrationRequestV1, StartStreamDescriptorMigrationRequestV1,
    StreamDescriptorMigrationBudgetsV1, StreamDescriptorMigrationTargetV1,
};

use super::{OsAppReconcileResult, reconcile_os_app_from_dir};
use crate::state::PlatformState;

fn copy_tree(source: &std::path::Path, target: &std::path::Path) {
    std::fs::create_dir_all(target).unwrap();
    for entry in std::fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let destination = target.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &destination);
        } else {
            std::fs::copy(entry.path(), destination).unwrap();
        }
    }
}

fn activated_temper_fs_app(directory: &std::path::Path) -> std::path::PathBuf {
    let app_dir = directory.join("temper-fs");
    copy_tree(
        &std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../os-apps/temper-fs"),
        &app_dir,
    );
    let csdl_path = app_dir.join("specs/model.csdl.xml");
    let csdl = std::fs::read_to_string(&csdl_path).unwrap();
    let csdl = csdl
        .replace(
            "<Annotation Term=\"Temper.Vocab.Stream.Mutability\" String=\"Mutable\"/>",
            "<Annotation Term=\"Temper.Vocab.Stream.Mutability\" String=\"Mutable\"/>\n        <Annotation Term=\"Temper.Vocab.Stream.DescriptorContractVersion\" Int=\"1\"/>",
        )
        .replace(
            "<Annotation Term=\"Temper.Vocab.Stream.Mutability\" String=\"Immutable\"/>",
            "<Annotation Term=\"Temper.Vocab.Stream.Mutability\" String=\"Immutable\"/>\n        <Annotation Term=\"Temper.Vocab.Stream.DescriptorContractVersion\" Int=\"1\"/>",
        );
    std::fs::write(csdl_path, csdl).unwrap();
    app_dir
}

#[tokio::test]
async fn installed_stream_contract_migrates_once_and_survives_restart_and_later_writes() {
    let directory = tempfile::tempdir().unwrap();
    let db_url = format!("file:{}", directory.path().join("platform.db").display());
    let tenant = "installed-stream-reconcile";
    let app_name = "temper-fs";
    let app_dir = activated_temper_fs_app(directory.path());

    let store = temper_store_turso::TursoEventStore::new(&db_url, None)
        .await
        .unwrap();
    let mut state = PlatformState::new(None);
    state
        .server
        .set_storage_stack(temper_server::StorageStack::from_turso(store));

    let required = reconcile_os_app_from_dir(&state, tenant, app_name, &app_dir, None)
        .await
        .unwrap();
    let OsAppReconcileResult::MigrationRequired {
        semantic_digest,
        capability_digest,
        descriptor_contract_version,
        ..
    } = required
    else {
        panic!("first reconcile must require governed stream migration, got {required:?}")
    };
    assert_eq!(descriptor_contract_version, 1);

    let target = StreamDescriptorMigrationTargetV1::InstalledApplication {
        application_id: app_name.into(),
        semantic_digest: semantic_digest.clone(),
    };
    let started = state
        .server
        .start_governed_stream_descriptor_migration_v1(
            &TenantId::new(tenant),
            StartStreamDescriptorMigrationRequestV1 {
                request_id: "installed-stream-start".into(),
                idempotency_key: "installed-stream-start".into(),
                target,
                expected_capability_digest: capability_digest,
                descriptor_contract_version: 1,
                budgets: StreamDescriptorMigrationBudgetsV1 {
                    max_subjects: 16,
                    max_events_per_subject: 64,
                    max_blob_bytes: 1_048_576,
                },
            },
        )
        .await
        .unwrap();
    let completed = state
        .server
        .advance_governed_stream_descriptor_migration_v1(
            &TenantId::new(tenant),
            AdvanceStreamDescriptorMigrationRequestV1 {
                request_id: "installed-stream-advance".into(),
                idempotency_key: "installed-stream-advance".into(),
                job_id: started.job_id,
            },
        )
        .await
        .unwrap();
    assert_eq!(completed.status, "completed");

    let installed = reconcile_os_app_from_dir(&state, tenant, app_name, &app_dir, None)
        .await
        .unwrap();
    assert!(matches!(installed, OsAppReconcileResult::Installed { .. }));
    let persistence_id = format!("{tenant}:File:post-cutover-file");
    state
        .server
        .storage_stack
        .as_ref()
        .unwrap()
        .events
        .append(
            &persistence_id,
            0,
            &[PersistenceEnvelope {
                sequence_nr: 1,
                event_type: "Touch".into(),
                payload: serde_json::json!({}),
                metadata: EventMetadata {
                    event_id: sim_uuid(),
                    causation_id: sim_uuid(),
                    correlation_id: sim_uuid(),
                    timestamp: sim_now(),
                    actor_id: persistence_id.clone(),
                    kernel: None,
                },
            }],
        )
        .await
        .unwrap();

    drop(state);
    let reopened_store = temper_store_turso::TursoEventStore::new(&db_url, None)
        .await
        .unwrap();
    let mut restarted = PlatformState::new(None);
    restarted
        .server
        .set_storage_stack(temper_server::StorageStack::from_turso(reopened_store));
    let reconciled = reconcile_os_app_from_dir(&restarted, tenant, app_name, &app_dir, None)
        .await
        .unwrap();
    assert!(
        !matches!(reconciled, OsAppReconcileResult::MigrationRequired { .. }),
        "an exact durable fence must make restart reconcile steady-state"
    );
}

#[tokio::test]
async fn installed_stream_reconcile_propagates_staging_backend_failure() {
    use sha2::{Digest as _, Sha256};

    let directory = tempfile::tempdir().unwrap();
    let app_dir = activated_temper_fs_app(directory.path());
    let tenant = "installed-stream-backend-failure";
    let app_name = "temper-fs";
    let store = temper_store_sim::SimEventStore::no_faults(712);
    let staged_id = format!(
        "{tenant}:_TemperStreamMigrationTarget:{:x}",
        Sha256::digest(app_name.as_bytes())
    );
    store.fail_next_reads(&staged_id, 1);
    let mut state = PlatformState::new(None);
    let store = std::sync::Arc::new(store);
    state
        .server
        .set_storage_stack(temper_server::StorageStack::new(
            temper_server::storage::BackendLabel::Sim,
            temper_server::storage::BoxedEventStore::from_arc(store.clone()),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        ));

    let error = reconcile_os_app_from_dir(&state, tenant, app_name, &app_dir, None)
        .await
        .unwrap_err();
    assert!(error.starts_with("backend unavailable:"), "{error}");
}
