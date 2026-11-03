//! Paused legacy downloads must leave the shared blob I/O budget available.
use std::time::Duration;

use axum::{
    body::Bytes,
    extract::{Extension, Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use futures_util::StreamExt as _;
use temper_authz::{AuthenticatedRequestContext, SecurityContext};
use temper_runtime::{ActorSystem, tenant::TenantId};
use temper_server::{ServerState, StorageStack, blob_store::BlobStreamRead};
use temper_spec::csdl::CsdlDocument;
use temper_store_turso::TursoEventStore;

async fn another_tenant_can_write_and_read(state: &ServerState, key: &str) {
    let tenant = TenantId::new("writer");
    let authenticated = AuthenticatedRequestContext::new(
        tenant.clone(),
        SecurityContext::from_resolved_identity("writer", "test-agent", None),
    );
    let response = tokio::time::timeout(
        Duration::from_secs(5),
        temper_server::blobs::put_blob(
            State(state.clone()),
            Extension(authenticated),
            Path(key.to_string()),
            Bytes::from_static(b"written"),
        ),
    )
    .await
    .expect("a paused legacy download must not block another tenant's blob write")
    .into_response();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let bytes = tokio::time::timeout(
        Duration::from_secs(5),
        state.get_blob_with_legacy_fallback(&tenant, key),
    )
    .await
    .expect("a paused legacy download must not block another tenant's blob read")
    .unwrap();
    assert_eq!(bytes, Some(b"written".to_vec()));
}

#[test]
fn paused_legacy_downloads_leave_blob_io_available() {
    // Set process-wide budgets before the child creates threads. Using a child
    // also prevents this regression from depending on other tests' semaphores.
    const CHILD: &str = "TEMPER_LEGACY_STREAM_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "paused_legacy_downloads_leave_blob_io_available",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("TEMPER_BLOB_IO_MAX_CONCURRENCY", "1")
            .env("TEMPER_BLOB_STREAM_MAX_CONCURRENCY", "2")
            .status()
            .unwrap();
        assert!(status.success(), "isolated legacy stream regression failed");
        return;
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let temp = tempfile::tempdir().unwrap();
            let store = TursoEventStore::new(
                &format!("file:{}", temp.path().join("blobs.db").display()),
                None,
            )
            .await
            .unwrap();
            let bytes = vec![b'x'; 256 * 1024];
            store.put_blob("legacy", &bytes).await.unwrap();
            let mut state = ServerState::new(
                ActorSystem::new("legacy-isolation"),
                CsdlDocument {
                    version: "4.0".into(),
                    schemas: vec![],
                },
                String::new(),
            );
            state.data_dir = temp.path().join("objects");
            state
                .authz
                .reload_tenant_policies("writer", "permit(principal, action, resource);")
                .unwrap();
            state.set_storage_stack(StorageStack::from_turso(store));
            let BlobStreamRead::Found(download) = state
                .stream_blob_object(&TenantId::default(), "legacy", bytes.len() as u64)
                .await
                .unwrap()
            else {
                panic!("legacy value");
            };
            let mut download = download.into_stream();
            // A body can pause before its first poll and after any emitted chunk.
            another_tenant_can_write_and_read(&state, "before-first-poll").await;
            assert_eq!(download.next().await.unwrap().unwrap().len(), 64 * 1024);
            another_tenant_can_write_and_read(&state, "between-chunks").await;
            drop(download);
            another_tenant_can_write_and_read(&state, "after-disconnect").await;
        });
}
