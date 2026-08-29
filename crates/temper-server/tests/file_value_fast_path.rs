use axum::body::Body;
use axum::http::{Request, StatusCode};
use sha2::{Digest, Sha256};
use temper_runtime::ActorSystem;
use temper_runtime::persistence::EventStore;
use temper_runtime::persistence::schema_deployment::{
    SchemaExecutionPin, SchemaScope, SchemaScopeKind, scoped_journal_entity_id,
};
use temper_runtime::tenant::TenantId;
use temper_server::registry::SpecRegistry;
use temper_server::registry::{EntityLevelSummary, EntityVerificationResult, VerificationStatus};
use temper_server::request_context::AgentContext;
use temper_server::state::IndexedFileStreamRead;
use temper_server::storage::StorageStack;
use temper_server::{ServerState, build_router};
use temper_spec::csdl::parse_csdl;
use temper_store_turso::TursoEventStore;
use tower::ServiceExt;

async fn authenticate_test_request(
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    request
        .extensions_mut()
        .insert(temper_authz::AuthenticatedRequestContext::new(
            TenantId::default(),
            temper_authz::SecurityContext::from_resolved_identity(
                "file-value-test",
                "test-agent",
                None,
            ),
        ));
    next.run(request).await
}

fn authenticated_router(state: ServerState) -> axum::Router {
    build_router(state).layer(axum::middleware::from_fn(authenticate_test_request))
}

const FILE_CSDL_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.FileReadFastPathTest" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="File" HasStream="true">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
        <Property Name="content_hash" Type="Edm.String"/>
        <Property Name="mime_type" Type="Edm.String"/>
        <Property Name="has_content" Type="Edm.Boolean"/>
        <Property Name="size_bytes" Type="Edm.Int64"/>
        <Annotation Term="Temper.Vocab.Stream.Mutability" String="Mutable"/>
        <Annotation Term="Temper.Vocab.Stream.DescriptorContractVersion" Int="1"/>
        <Annotation Term="Temper.Vocab.Stream.MigrationPublicationAction" String="StreamUpdated"/>
        <Annotation Term="Temper.Vocab.Stream.MigrationContentHashParameter" String="content_hash"/>
        <Annotation Term="Temper.Vocab.Stream.MigrationByteLengthParameter" String="size_bytes"/>
        <Annotation Term="Temper.Vocab.Stream.MigrationContentTypeParameter" String="mime_type"/>
        <Annotation Term="Temper.Vocab.Stream.MigrationStorageContractVersion" Int="1"/>
        <Annotation Term="Temper.Vocab.Stream.MigrationStorageKeyPrefix" String="temper-fs/"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Files" EntityType="Temper.FileReadFastPathTest.File"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

// CSDL with both File and Workspace, used by the workspace-freeze write-gate
// tests. Workspace carries a Status so `resolve_entity_status` can read it.
const FILE_WORKSPACE_CSDL_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.FileWorkspaceWriteGateTest" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="File" HasStream="true">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
        <Property Name="workspace_id" Type="Edm.String"/>
        <Property Name="content_hash" Type="Edm.String"/>
        <Property Name="mime_type" Type="Edm.String"/>
        <Property Name="has_content" Type="Edm.Boolean"/>
        <Property Name="size_bytes" Type="Edm.Int64"/>
      </EntityType>
      <EntityType Name="Workspace">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Files" EntityType="Temper.FileWorkspaceWriteGateTest.File"/>
        <EntitySet Name="Workspaces" EntityType="Temper.FileWorkspaceWriteGateTest.Workspace"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

// Minimal Workspace IOA with the Active/Frozen lifecycle and a Freeze action,
// enough for `resolve_entity_status` to report a non-Active status.
const WORKSPACE_IOA: &str = r#"
[automaton]
name = "Workspace"
states = ["Active", "Frozen"]
initial = "Active"

[[action]]
name = "Freeze"
kind = "internal"
from = ["Active"]
to = "Frozen"
"#;

// File IOA carrying the real cross-entity guard on StreamUpdated, mirroring
// os-apps/temper-fs/specs/file.ioa.toml (Fix #1/#2).
const FILE_IOA_GUARDED: &str = r#"
[automaton]
name = "File"
states = ["Created", "Ready"]
initial = "Created"

[[state]]
name = "content_hash"
type = "string"
initial = ""

[[state]]
name = "has_content"
type = "bool"
initial = "false"

[[state]]
name = "size_bytes"
type = "counter"
initial = "0"

[[state]]
name = "version_count"
type = "counter"
initial = "0"

[[action]]
name = "Create"
kind = "input"
from = ["Created"]
to = "Created"
params = ["name", "path", "directory_id", "workspace_id", "mime_type"]

[[action]]
name = "StreamUpdated"
kind = "input"
from = ["Created", "Ready"]
to = "Ready"
params = ["content_hash", "size_bytes", "mime_type", "version_number", "previous_version_id", "created_by"]
guard = [
  { type = "cross_entity_state", entity_type = "Workspace", entity_id_source = "workspace_id", required_status = ["Active"] },
]
effect = [
  { type = "increment", var = "version_count" },
  { type = "set_counter_from_param", var = "size_bytes", param = "size_bytes" },
  { type = "set_bool", var = "has_content", value = "true" },
]
"#;

const FILE_IOA: &str = r#"
[automaton]
name = "File"
states = ["Created", "Ready"]
initial = "Created"

[[state]]
name = "content_hash"
type = "string"
initial = ""

[[state]]
name = "mime_type"
type = "string"
initial = ""

[[state]]
name = "has_content"
type = "bool"
initial = "false"

[[state]]
name = "size_bytes"
type = "counter"
initial = "0"

[[state]]
name = "version_count"
type = "counter"
initial = "0"

[[action]]
name = "Create"
kind = "input"
from = ["Created"]
to = "Created"
params = ["name", "path", "directory_id", "workspace_id", "mime_type"]

[[action]]
name = "StreamUpdated"
kind = "input"
from = ["Created", "Ready"]
to = "Ready"
params = ["content_hash", "size_bytes", "mime_type", "version_number", "previous_version_id", "created_by"]
effect = [
  { type = "increment", var = "version_count" },
  { type = "set_counter_from_param", var = "size_bytes", param = "size_bytes" },
  { type = "set_bool", var = "has_content", value = "true" },
]
"#;

async fn build_turso_state(test_name: &str) -> (ServerState, TursoEventStore) {
    let db_path = std::env::temp_dir().join(format!(
        "temper-file-value-fast-path-{test_name}-{}.db",
        uuid::Uuid::new_v4()
    ));
    let db_url = format!("file:{}", db_path.display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso db");

    let mut state = ServerState::from_registry(ActorSystem::new(test_name), SpecRegistry::new());
    state.set_storage_stack(StorageStack::from_turso(store.clone()));
    (state, store)
}

async fn build_turso_file_state(test_name: &str) -> (ServerState, TursoEventStore) {
    let db_path = std::env::temp_dir().join(format!(
        "temper-file-value-fast-path-{test_name}-{}.db",
        uuid::Uuid::new_v4()
    ));
    let db_url = format!("file:{}", db_path.display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso db");

    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(FILE_CSDL_XML).expect("file CSDL should parse");
    registry.register_tenant(
        "default",
        csdl,
        FILE_CSDL_XML.to_string(),
        &[("File", FILE_IOA)],
    );
    let mut state = ServerState::from_registry(ActorSystem::new(test_name), registry);
    state.set_storage_stack(StorageStack::from_turso(store.clone()));
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .expect("functional file-value tests should install an explicit policy");
    (state, store)
}

fn mark_file_verified(state: &ServerState) {
    let mut registry = state.registry.write().unwrap();
    registry.set_verification_status(
        &TenantId::default(),
        "File",
        VerificationStatus::Completed(EntityVerificationResult {
            all_passed: true,
            levels: vec![EntityLevelSummary {
                level: "L0".to_string(),
                passed: true,
                summary: "test fixture verified".to_string(),
                details: None,
            }],
            verified_at: "2026-05-15T00:00:00Z".to_string(),
        }),
    );
}

async fn assert_local_blob(data_dir: &std::path::Path, content_hash: &str, expected: &[u8]) {
    let blob_path = data_dir.join("blobs").join("temper-fs").join(content_hash);
    let stored = tokio::fs::read(&blob_path)
        .await
        .unwrap_or_else(|error| panic!("read local blob '{}': {error}", blob_path.display()));
    assert_eq!(stored, expected);
}

#[tokio::test]
async fn create_file_with_initial_stream_content_projects_only_ready_content() {
    let (mut state, store) = build_turso_file_state("atomic-initial-content").await;
    let tenant = TenantId::default();
    let data_dir = tempfile::tempdir().expect("temp data dir");
    state.data_dir = data_dir.path().to_path_buf();

    let body = b"atomic initial File value";
    let expected_hash = format!("sha256:{:x}", Sha256::digest(body));
    let response = state
        .create_file_with_initial_stream_content(
            &tenant,
            "fl-atomic-initial",
            serde_json::json!({
                "name": "atomic.md",
                "path": "/atomic.md",
                "directory_id": "dir-root",
                "workspace_id": "ws-root",
                "mime_type": "text/markdown",
            }),
            body,
            "text/markdown",
            &AgentContext::for_service("test-writer"),
        )
        .await
        .expect("atomic initial File content write should succeed");

    assert_eq!(response.state.status, "Ready");
    assert_eq!(response.state.sequence_nr, 3);
    assert_eq!(response.state.fields["name"], "atomic.md");
    assert_eq!(response.state.fields["content_hash"], expected_hash);
    assert_eq!(response.state.fields["has_content"], true);
    assert_eq!(response.state.fields["size_bytes"], body.len() as i64);

    let events = store
        .read_events("default:File:fl-atomic-initial", 0)
        .await
        .expect("read File journal");
    let actions = events
        .iter()
        .map(|event| event.event_type.as_str())
        .collect::<Vec<_>>();
    assert_eq!(actions, ["Created", "Create", "StreamUpdated"]);

    let indexed = state
        .read_file_stream_indexed(&tenant, "fl-atomic-initial")
        .await
        .expect("indexed read should see first bytes");
    assert_eq!(
        indexed,
        IndexedFileStreamRead::Content {
            content_hash: expected_hash.clone(),
            mime_type: "text/markdown".to_string(),
            bytes: body.to_vec(),
        }
    );
    assert_local_blob(data_dir.path(), &expected_hash, body).await;
}

#[tokio::test]
async fn reserved_scoped_journal_id_rejects_initial_file_content_without_side_effects() {
    let (mut state, store) = build_turso_file_state("reserved-initial-content").await;
    let data_dir = tempfile::tempdir().expect("temp data dir");
    state.data_dir = data_dir.path().to_path_buf();
    let tenant = TenantId::default();
    let pin = SchemaExecutionPin {
        scope: SchemaScope {
            kind: SchemaScopeKind::Task,
            id: "task-file".to_string(),
        },
        bundle_digest: format!("sha256:{}", "a".repeat(64)),
    };
    let reserved_id = scoped_journal_entity_id("file-1", &pin);
    let body = b"must not persist";
    let content_hash = format!("sha256:{:x}", Sha256::digest(body));

    let error = state
        .create_file_with_initial_stream_content(
            &tenant,
            &reserved_id,
            serde_json::json!({}),
            body,
            "text/plain",
            &AgentContext::default(),
        )
        .await
        .expect_err("reserved global File ID must fail before side effects");
    assert!(error.contains("reserved scoped-journal identity form"));
    assert!(
        store
            .read_events(&format!("{tenant}:File:{reserved_id}"), 0)
            .await
            .expect("read reserved File journal")
            .is_empty(),
        "reserved File create must not append journal events"
    );
    assert!(!state.entity_exists(&tenant, "File", &reserved_id));
    let blob_path = data_dir
        .path()
        .join("blobs")
        .join("temper-fs")
        .join(content_hash);
    assert!(
        tokio::fs::metadata(blob_path).await.is_err(),
        "reserved File create must reject before persisting blob bytes"
    );
}

#[tokio::test]
async fn put_file_stream_content_writes_native_blob_and_dispatches_update() {
    let (mut state, _store) = build_turso_file_state("native-write").await;
    let tenant = TenantId::default();
    let data_dir = tempfile::tempdir().expect("temp data dir");
    state.data_dir = data_dir.path().to_path_buf();

    state
        .get_or_create_tenant_entity(&tenant, "File", "fl-native-write", serde_json::json!({}))
        .await
        .expect("create File state");

    let body = b"native File value write";
    let expected_hash = format!("sha256:{:x}", Sha256::digest(body));
    let response = state
        .put_file_stream_content(
            &tenant,
            "fl-native-write",
            body,
            "text/plain",
            &AgentContext::for_service("test-writer"),
        )
        .await
        .expect("native File content write should succeed");

    assert_eq!(response.state.status, "Ready");
    assert_eq!(response.state.fields["content_hash"], expected_hash);
    assert_eq!(response.state.fields["mime_type"], "text/plain");
    assert_eq!(response.state.fields["has_content"], true);
    assert_eq!(response.state.fields["size_bytes"], body.len() as i64);
    assert_local_blob(data_dir.path(), &expected_hash, body).await;
}

#[tokio::test]
async fn odata_file_value_put_uses_native_path_without_blob_adapter() {
    let (mut state, _store) = build_turso_file_state("odata-native-write").await;
    let tenant = TenantId::default();
    let data_dir = tempfile::tempdir().expect("temp data dir");
    state.data_dir = data_dir.path().to_path_buf();
    mark_file_verified(&state);

    state
        .get_or_create_tenant_entity(&tenant, "File", "fl-odata-native", serde_json::json!({}))
        .await
        .expect("create File state");

    let app = authenticated_router(state.clone());
    let body = b"odata native File value write";
    let expected_hash = format!("sha256:{:x}", Sha256::digest(body));
    let response = app
        .oneshot(
            Request::put("/tdata/Files('fl-odata-native')/$value")
                .header("content-type", "text/plain")
                .body(Body::from(body.as_slice()))
                .expect("request"),
        )
        .await
        .expect("route should respond");

    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let expected_etag = format!("\"{expected_hash}\"");
    assert_eq!(
        response.headers().get("ETag").and_then(|v| v.to_str().ok()),
        Some(expected_etag.as_str())
    );

    let entity = state
        .get_tenant_entity_state(&tenant, "File", "fl-odata-native")
        .await
        .expect("OData native write should update File state");
    assert_eq!(entity.state.fields["content_hash"], expected_hash);
    assert_eq!(entity.state.fields["mime_type"], "text/plain");
    assert_eq!(entity.state.fields["has_content"], true);
    assert_local_blob(data_dir.path(), &expected_hash, body).await;
}

#[tokio::test]
async fn odata_file_value_put_applies_cedar_update_policy() {
    let (mut state, _store) = build_turso_file_state("odata-write-denied").await;
    let tenant = TenantId::default();
    let data_dir = tempfile::tempdir().expect("temp data dir");
    state.data_dir = data_dir.path().to_path_buf();
    mark_file_verified(&state);

    state
        .get_or_create_tenant_entity(&tenant, "File", "fl-write-denied", serde_json::json!({}))
        .await
        .expect("create File state");
    state
        .authz
        .reload_tenant_policies(
            tenant.as_str(),
            r#"permit(principal, action == Action::"read", resource is File);"#,
        )
        .expect("install Cedar policy");

    let response = authenticated_router(state.clone())
        .oneshot(
            Request::put("/tdata/Files('fl-write-denied')/$value")
                .header("content-type", "text/plain")
                .header("x-temper-principal-kind", "customer")
                .header("x-temper-principal-id", "customer-1")
                .body(Body::from("must not be written"))
                .expect("request"),
        )
        .await
        .expect("route should respond");

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let entity = state
        .get_tenant_entity_state(&tenant, "File", "fl-write-denied")
        .await
        .expect("File state should remain readable");
    assert!(entity.state.fields.get("content_hash").is_none());
}

#[tokio::test]
async fn odata_file_value_get_applies_cedar_read_policy() {
    let (mut state, _store) = build_turso_file_state("odata-read-denied").await;
    let tenant = TenantId::default();
    let data_dir = tempfile::tempdir().expect("temp data dir");
    state.data_dir = data_dir.path().to_path_buf();

    state
        .get_or_create_tenant_entity(&tenant, "File", "fl-read-denied", serde_json::json!({}))
        .await
        .expect("create File state");
    state
        .put_file_stream_content(
            &tenant,
            "fl-read-denied",
            b"private content",
            "text/plain",
            &AgentContext::for_service("test-writer"),
        )
        .await
        .expect("seed stream content");
    state
        .authz
        .reload_tenant_policies(
            tenant.as_str(),
            r#"permit(principal, action == Action::"update", resource is File);"#,
        )
        .expect("install Cedar policy");

    let response = authenticated_router(state)
        .oneshot(
            Request::get("/tdata/Files('fl-read-denied')/$value")
                .header("x-temper-principal-kind", "customer")
                .header("x-temper-principal-id", "customer-1")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("route should respond");

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn read_file_stream_indexed_returns_blob_without_actor_materialization() {
    let (state, store) = build_turso_state("content").await;
    let tenant = TenantId::default();
    let bytes = b"<main>published embodiment</main>";
    let content_hash = "sha256:fast-path-content";

    store
        .put_blob(&format!("temper-fs/{content_hash}"), bytes)
        .await
        .expect("put blob");
    store
        .upsert_query_projection(
            tenant.as_str(),
            "File",
            "fl-fast-path",
            "Ready",
            &serde_json::json!({
                "content_hash": content_hash,
                "mime_type": "text/html",
                "has_content": true,
            }),
            1,
        )
        .await
        .expect("upsert File projection");

    let read = state
        .read_file_stream_indexed(&tenant, "fl-fast-path")
        .await
        .expect("indexed file read succeeds");

    assert_eq!(
        read,
        IndexedFileStreamRead::Content {
            content_hash: content_hash.to_string(),
            mime_type: "text/html".to_string(),
            bytes: bytes.to_vec(),
        }
    );
}

#[tokio::test]
async fn read_file_stream_indexed_reports_missing_index_for_unprojected_file() {
    let (state, _store) = build_turso_state("missing-index").await;
    let tenant = TenantId::default();

    let read = state
        .read_file_stream_indexed(&tenant, "fl-missing")
        .await
        .expect("indexed file read succeeds");

    assert_eq!(read, IndexedFileStreamRead::MissingIndex);
}

#[tokio::test]
async fn read_file_stream_indexed_falls_back_to_file_state_when_projection_is_missing() {
    let (state, store) = build_turso_file_state("missing-projection-fallback").await;
    let tenant = TenantId::default();
    let bytes = b"<main>projection lag should not break publishing</main>";
    let content_hash = "sha256:file-state-fallback";

    store
        .put_blob(&format!("temper-fs/{content_hash}"), bytes)
        .await
        .expect("put blob");

    state
        .get_or_create_tenant_entity(
            &tenant,
            "File",
            "fl-state-only",
            serde_json::json!({
                "content_hash": content_hash,
                "mime_type": "text/html",
                "has_content": true,
            }),
        )
        .await
        .expect("create File state");
    store
        .remove_query_projection(tenant.as_str(), "File", "fl-state-only")
        .await
        .expect("remove File projection");

    let read = state
        .read_file_stream_indexed(&tenant, "fl-state-only")
        .await
        .expect("indexed file read should fall back to entity state");

    assert_eq!(
        read,
        IndexedFileStreamRead::Content {
            content_hash: content_hash.to_string(),
            mime_type: "text/html".to_string(),
            bytes: bytes.to_vec(),
        }
    );
}

#[tokio::test]
async fn read_file_stream_indexed_falls_back_to_file_state_when_projection_is_stale() {
    let (state, store) = build_turso_file_state("stale-projection-fallback").await;
    let tenant = TenantId::default();
    let current_bytes = b"<main>current file state should win</main>";
    let current_hash = "sha256:current-file-state";
    let stale_hash = "sha256:stale-projection";

    store
        .upsert_query_projection(
            tenant.as_str(),
            "File",
            "fl-stale-state",
            "Ready",
            &serde_json::json!({
                "content_hash": stale_hash,
                "mime_type": "text/html",
                "has_content": true,
            }),
            1,
        )
        .await
        .expect("upsert stale File projection");
    store
        .put_blob(&format!("temper-fs/{current_hash}"), current_bytes)
        .await
        .expect("put current blob");

    state
        .get_or_create_tenant_entity(
            &tenant,
            "File",
            "fl-stale-state",
            serde_json::json!({
                "content_hash": current_hash,
                "mime_type": "text/html",
                "has_content": true,
            }),
        )
        .await
        .expect("create File state");

    store
        .upsert_query_projection(
            tenant.as_str(),
            "File",
            "fl-stale-state",
            "Ready",
            &serde_json::json!({
                "content_hash": stale_hash,
                "mime_type": "text/html",
                "has_content": true,
            }),
            1,
        )
        .await
        .expect("restore stale File projection after state write");

    let read = state
        .read_file_stream_indexed(&tenant, "fl-stale-state")
        .await
        .expect("indexed file read should fall back to entity state");

    assert_eq!(
        read,
        IndexedFileStreamRead::Content {
            content_hash: current_hash.to_string(),
            mime_type: "text/html".to_string(),
            bytes: current_bytes.to_vec(),
        }
    );
}

#[tokio::test]
async fn put_value_on_new_file_is_one_atomic_append() {
    // ARN-87 write-side: a brand-new File's first `$value` PUT must commit as ONE
    // atomic create-with-content append. The pre-fix path spawned the actor
    // (whose `pre_start` persists an empty bootstrap `Created`) and only THEN
    // dispatched `StreamUpdated` as a SEPARATE append — leaving the File durable
    // and projection-visible with an empty `$value` between the two appends. This
    // test reads the durable journal and asserts there is no such window: the
    // create and the content land together (Created [+ Create] + StreamUpdated),
    // and the highest-sequence event carries content (StreamUpdated), never a
    // lone empty `Created`.
    let (mut state, store) = build_turso_file_state("new-file-atomic-append").await;
    let data_dir = tempfile::tempdir().expect("temp data dir");
    state.data_dir = data_dir.path().to_path_buf();
    mark_file_verified(&state);

    let body = b"brand new file value, one append";

    let response = authenticated_router(state.clone())
        .oneshot(
            Request::put("/tdata/Files('fl-new-atomic')/$value")
                .header("content-type", "text/plain")
                .body(Body::from(body.as_slice()))
                .expect("request"),
        )
        .await
        .expect("route should respond");
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let events = store
        .read_events("default:File:fl-new-atomic", 0)
        .await
        .expect("read File journal");
    let actions = events
        .iter()
        .map(|event| event.event_type.as_str())
        .collect::<Vec<_>>();

    // The whole journal is a single atomic boundary: the create and the first
    // content arrive together. There is NO lone bootstrap `Created` that is later
    // followed by a separate `StreamUpdated` append.
    assert_eq!(
        actions,
        ["Created", "StreamUpdated"],
        "new-File PUT must be one atomic create-with-content append, got {actions:?}"
    );

    // The highest-sequence event carries content — there is no durable state
    // whose max-seq event is an empty `Created`.
    let last = events.last().expect("journal has events");
    assert_eq!(last.event_type, "StreamUpdated");
    assert_eq!(last.sequence_nr, 2);

    // And the bootstrap `Created` never carried (empty) content of its own that
    // could be read before `StreamUpdated` landed.
    let created = events
        .iter()
        .find(|event| event.event_type == "Created")
        .expect("created event present");
    assert!(
        created
            .payload
            .get("params")
            .and_then(|params| params.get("content_hash"))
            .is_none(),
        "the create event must not carry content; content arrives with StreamUpdated"
    );
}

#[tokio::test]
async fn new_file_value_put_is_read_after_write_consistent() {
    // The window the fix closes is observable through reads: immediately after a
    // brand-new File `$value` PUT, the projection must already reflect content —
    // never an empty `has_content=false` / no-`content_hash` row.
    let (mut state, _store) = build_turso_file_state("new-file-raw").await;
    let tenant = TenantId::default();
    let data_dir = tempfile::tempdir().expect("temp data dir");
    state.data_dir = data_dir.path().to_path_buf();
    mark_file_verified(&state);

    let body = b"read after write consistency";
    let expected_hash = format!("sha256:{:x}", Sha256::digest(body));

    let response = authenticated_router(state.clone())
        .oneshot(
            Request::put("/tdata/Files('fl-raw-consistent')/$value")
                .header("content-type", "text/markdown")
                .body(Body::from(body.as_slice()))
                .expect("request"),
        )
        .await
        .expect("route should respond");
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    // Read immediately: the very first observable state already has content.
    let entity = state
        .get_tenant_entity_state(&tenant, "File", "fl-raw-consistent")
        .await
        .expect("new File should be readable right after the PUT");
    assert_eq!(entity.state.status, "Ready");
    assert_eq!(entity.state.fields["content_hash"], expected_hash);
    assert_eq!(entity.state.fields["has_content"], true);
    assert_eq!(entity.state.fields["mime_type"], "text/markdown");

    // The indexed `$value` read sees the bytes — there is no empty row to observe.
    let indexed = state
        .read_file_stream_indexed(&tenant, "fl-raw-consistent")
        .await
        .expect("indexed read should see first bytes");
    assert_eq!(
        indexed,
        IndexedFileStreamRead::Content {
            content_hash: expected_hash.clone(),
            mime_type: "text/markdown".to_string(),
            bytes: body.to_vec(),
        }
    );
}

#[tokio::test]
async fn concurrent_new_file_value_puts_yield_one_204_one_409() {
    // Two concurrent brand-new PUTs to the SAME id: the atomic single-append
    // create is guarded by the journal's expected-sequence (0) check, so exactly
    // one wins (204) and the other is rejected as a concurrency violation (409
    // ActionRejected). Neither leaves an empty `$value` behind.
    let (mut state, store) = build_turso_file_state("new-file-concurrent").await;
    let data_dir = tempfile::tempdir().expect("temp data dir");
    state.data_dir = data_dir.path().to_path_buf();
    mark_file_verified(&state);

    let body_a = b"writer A bytes";
    let body_b = b"writer B different bytes";

    let app = authenticated_router(state.clone());
    let put_a = app.clone().oneshot(
        Request::put("/tdata/Files('fl-race')/$value")
            .header("content-type", "text/plain")
            .body(Body::from(body_a.as_slice()))
            .expect("request A"),
    );
    let put_b = app.oneshot(
        Request::put("/tdata/Files('fl-race')/$value")
            .header("content-type", "text/plain")
            .body(Body::from(body_b.as_slice()))
            .expect("request B"),
    );
    let (res_a, res_b) = tokio::join!(put_a, put_b);
    let status_a = res_a.expect("route A responds").status();
    let status_b = res_b.expect("route B responds").status();

    let mut statuses = [status_a, status_b];
    statuses.sort_by_key(|status| status.as_u16());
    assert_eq!(
        statuses,
        [StatusCode::NO_CONTENT, StatusCode::CONFLICT],
        "exactly one new-File PUT must succeed (204) and the other conflict (409), got {statuses:?}"
    );

    // The winner left a single coherent journal: create + content, no second
    // bootstrap, no empty-value remnant from the loser.
    let events = store
        .read_events("default:File:fl-race", 0)
        .await
        .expect("read File journal");
    let actions = events
        .iter()
        .map(|event| event.event_type.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        actions,
        ["Created", "StreamUpdated"],
        "the winning create must be the only durable history, got {actions:?}"
    );
}

#[tokio::test]
async fn existing_file_value_put_routes_through_update_unchanged() {
    // Regression guard: PUT `$value` on an EXISTING File still routes through the
    // update path (a fresh `StreamUpdated` append that bumps the version), proving
    // the ARN-87 new-File fast path did not change behavior for updates.
    let (mut state, store) = build_turso_file_state("existing-file-update").await;
    let tenant = TenantId::default();
    let data_dir = tempfile::tempdir().expect("temp data dir");
    state.data_dir = data_dir.path().to_path_buf();
    mark_file_verified(&state);

    // First write: brand-new File via the atomic create-with-content path.
    let first = b"version one bytes";
    let first_hash = format!("sha256:{:x}", Sha256::digest(first));
    let response = authenticated_router(state.clone())
        .oneshot(
            Request::put("/tdata/Files('fl-update')/$value")
                .header("content-type", "text/plain")
                .body(Body::from(first.as_slice()))
                .expect("request"),
        )
        .await
        .expect("route should respond");
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let after_create = store
        .read_events("default:File:fl-update", 0)
        .await
        .expect("read File journal after create");
    assert_eq!(
        after_create
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        ["Created", "StreamUpdated"],
    );

    // Second write to the SAME (now existing) File: must go through the update
    // path and append another StreamUpdated, bumping the version.
    let second = b"version two bytes, larger payload";
    let second_hash = format!("sha256:{:x}", Sha256::digest(second));
    assert_ne!(first_hash, second_hash);
    let response = authenticated_router(state.clone())
        .oneshot(
            Request::put("/tdata/Files('fl-update')/$value")
                .header("content-type", "text/plain")
                .body(Body::from(second.as_slice()))
                .expect("request"),
        )
        .await
        .expect("route should respond");
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let expected_etag = format!("\"{second_hash}\"");
    assert_eq!(
        response.headers().get("ETag").and_then(|v| v.to_str().ok()),
        Some(expected_etag.as_str()),
        "update must return the new content hash"
    );

    let after_update = store
        .read_events("default:File:fl-update", 0)
        .await
        .expect("read File journal after update");
    let update_actions = after_update
        .iter()
        .map(|event| event.event_type.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        update_actions,
        ["Created", "StreamUpdated", "StreamUpdated"],
        "existing-File PUT must append a fresh StreamUpdated (no re-create), got {update_actions:?}"
    );

    let entity = state
        .get_tenant_entity_state(&tenant, "File", "fl-update")
        .await
        .expect("File state should reflect the update");
    assert_eq!(entity.state.fields["content_hash"], second_hash);
    assert_eq!(entity.state.fields["has_content"], true);
    // The StreamUpdated effect increments `version_count` once per append; two
    // content writes => version 2.
    assert_eq!(
        entity.state.counters.get("version_count").copied(),
        Some(2),
        "each content write bumps the version"
    );
}

#[tokio::test]
async fn read_file_stream_indexed_reports_stale_index_when_blob_is_missing() {
    let (state, store) = build_turso_state("stale-index").await;
    let tenant = TenantId::default();
    let content_hash = "sha256:missing-blob";

    store
        .upsert_query_projection(
            tenant.as_str(),
            "File",
            "fl-stale",
            "Ready",
            &serde_json::json!({
                "content_hash": content_hash,
                "mime_type": "text/html",
                "has_content": true,
            }),
            1,
        )
        .await
        .expect("upsert File projection");

    let read = state
        .read_file_stream_indexed(&tenant, "fl-stale")
        .await
        .expect("indexed file read succeeds");

    assert_eq!(
        read,
        IndexedFileStreamRead::StaleIndex {
            content_hash: content_hash.to_string(),
            mime_type: "text/html".to_string(),
        }
    );
}

// ─── Fix #1/#2: a Frozen Workspace rejects new File writes ──────────────

async fn build_turso_file_workspace_state(test_name: &str) -> (ServerState, TursoEventStore) {
    let db_path = std::env::temp_dir().join(format!(
        "temper-file-value-fast-path-{test_name}-{}.db",
        uuid::Uuid::new_v4()
    ));
    let db_url = format!("file:{}", db_path.display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso db");

    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(FILE_WORKSPACE_CSDL_XML).expect("file+workspace CSDL should parse");
    registry.register_tenant(
        "default",
        csdl,
        FILE_WORKSPACE_CSDL_XML.to_string(),
        &[("File", FILE_IOA_GUARDED), ("Workspace", WORKSPACE_IOA)],
    );
    let mut state = ServerState::from_registry(ActorSystem::new(test_name), registry);
    state.set_storage_stack(StorageStack::from_turso(store.clone()));
    (state, store)
}

/// Create a Workspace and Freeze it so `resolve_entity_status` reports
/// `"Frozen"`.
async fn freeze_workspace(state: &ServerState, tenant: &TenantId, workspace_id: &str) {
    state
        .get_or_create_tenant_entity(tenant, "Workspace", workspace_id, serde_json::json!({}))
        .await
        .expect("create Workspace state");
    state
        .dispatch_tenant_action(
            tenant,
            "Workspace",
            workspace_id,
            "Freeze",
            serde_json::json!({}),
            &AgentContext::for_service("test-writer"),
        )
        .await
        .expect("freeze Workspace");
}

#[tokio::test]
async fn create_file_in_frozen_workspace_is_rejected_with_no_blob() {
    let (mut state, _store) = build_turso_file_workspace_state("create-frozen-ws").await;
    let tenant = TenantId::default();
    let data_dir = tempfile::tempdir().expect("temp data dir");
    state.data_dir = data_dir.path().to_path_buf();

    freeze_workspace(&state, &tenant, "ws-frozen").await;

    let body = b"content for a frozen workspace";
    let content_hash = format!("sha256:{:x}", Sha256::digest(body));
    let result = state
        .create_file_with_initial_stream_content(
            &tenant,
            "fl-in-frozen",
            serde_json::json!({
                "name": "blocked.md",
                "path": "/blocked.md",
                "directory_id": "dir-root",
                "workspace_id": "ws-frozen",
                "mime_type": "text/markdown",
            }),
            body,
            "text/markdown",
            &AgentContext::for_service("test-writer"),
        )
        .await;

    let error = result.expect_err("write into a Frozen workspace must be rejected");
    assert!(
        error.contains("ws-frozen") && error.contains("Frozen"),
        "rejection must name the frozen workspace, got: {error}"
    );

    // No bytes were written: the pre-write check fires before the blob write.
    let blob_path = data_dir
        .path()
        .join("blobs")
        .join("temper-fs")
        .join(&content_hash);
    assert!(
        tokio::fs::metadata(&blob_path).await.is_err(),
        "no blob should be persisted when the workspace rejects the write"
    );

    // No File entity should exist either.
    assert!(
        !state
            .ensure_entity_loaded(&tenant, "File", "fl-in-frozen")
            .await,
        "File must not be created when its workspace is Frozen"
    );
}

#[tokio::test]
async fn put_existing_file_in_frozen_workspace_is_rejected_with_no_new_blob() {
    let (mut state, _store) = build_turso_file_workspace_state("put-frozen-ws").await;
    let tenant = TenantId::default();
    let data_dir = tempfile::tempdir().expect("temp data dir");
    state.data_dir = data_dir.path().to_path_buf();

    // Create the File while the workspace is still Active (so the first write
    // succeeds and the File persists its workspace_id), then freeze.
    state
        .create_file_with_initial_stream_content(
            &tenant,
            "fl-existing",
            serde_json::json!({
                "name": "doc.md",
                "path": "/doc.md",
                "directory_id": "dir-root",
                "workspace_id": "ws-later-frozen",
                "mime_type": "text/markdown",
            }),
            b"first version while active",
            "text/markdown",
            &AgentContext::for_service("test-writer"),
        )
        .await
        .expect("first write into an Active workspace should succeed");

    freeze_workspace(&state, &tenant, "ws-later-frozen").await;

    let body = b"second version after freeze";
    let content_hash = format!("sha256:{:x}", Sha256::digest(body));
    let result = state
        .put_file_stream_content(
            &tenant,
            "fl-existing",
            body,
            "text/markdown",
            &AgentContext::for_service("test-writer"),
        )
        .await;

    let error = result.expect_err("updating a File in a Frozen workspace must be rejected");
    assert!(
        error.contains("ws-later-frozen") && error.contains("Frozen"),
        "rejection must name the frozen workspace, got: {error}"
    );

    let blob_path = data_dir
        .path()
        .join("blobs")
        .join("temper-fs")
        .join(&content_hash);
    assert!(
        tokio::fs::metadata(&blob_path).await.is_err(),
        "the second (rejected) write must not persist a new blob"
    );
}

#[tokio::test]
async fn create_file_in_active_workspace_succeeds() {
    let (mut state, _store) = build_turso_file_workspace_state("create-active-ws").await;
    let tenant = TenantId::default();
    let data_dir = tempfile::tempdir().expect("temp data dir");
    state.data_dir = data_dir.path().to_path_buf();

    // Active workspace present — write must pass the gate AND the cross-entity
    // guard on StreamUpdated (which the synthetic path resolves to true).
    state
        .get_or_create_tenant_entity(&tenant, "Workspace", "ws-active", serde_json::json!({}))
        .await
        .expect("create Workspace state");

    let body = b"content for an active workspace";
    let response = state
        .create_file_with_initial_stream_content(
            &tenant,
            "fl-in-active",
            serde_json::json!({
                "name": "ok.md",
                "path": "/ok.md",
                "directory_id": "dir-root",
                "workspace_id": "ws-active",
                "mime_type": "text/markdown",
            }),
            body,
            "text/markdown",
            &AgentContext::for_service("test-writer"),
        )
        .await
        .expect("write into an Active workspace should succeed");

    assert_eq!(response.state.status, "Ready");
    assert_eq!(response.state.fields["has_content"], true);
}

#[path = "file_value_fast_path/stream_migration.rs"]
mod stream_migration_tests;
