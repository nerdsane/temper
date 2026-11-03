//! A guest's loopback File `$value` call runs through the OData `$value` handlers as the guest's caller, so it gets
//! the Cedar decision the same caller's HTTP request gets: read for GET, update for PUT.

use super::*;
use crate::registry::{
    EntityLevelSummary, EntityVerificationResult, SpecRegistry, VerificationStatus,
};
use crate::request_context::AgentContext;
use crate::storage::StorageStack;

const FILE_CSDL_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.LocalFileValue" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="File" HasStream="true">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
        <Property Name="content_hash" Type="Edm.String"/>
        <Property Name="mime_type" Type="Edm.String"/>
        <Property Name="has_content" Type="Edm.Boolean"/>
        <Property Name="size_bytes" Type="Edm.Int64"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Files" EntityType="Temper.LocalFileValue.File"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

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
initial = false

[[state]]
name = "size_bytes"
type = "counter"
initial = 0

[[state]]
name = "version_count"
type = "counter"
initial = 0

[[action]]
name = "StreamUpdated"
kind = "input"
from = ["Created", "Ready"]
to = "Ready"
params = ["content_hash", "size_bytes", "mime_type", "version_number", "previous_version_id", "created_by"]
effect = ["version_count += 1", "size_bytes = params.size_bytes", "has_content = true"]
"#;

const OWNER_TEXT: &str = "owner text";

/// A tenant whose one File, `doc-7`, holds `OWNER_TEXT`, and whose policy lets only `file-owner` act on Files.
async fn owned_file_state(name: &str) -> (ServerState, tempfile::TempDir) {
    let data_dir = tempfile::tempdir().expect("data dir");
    let db_url = format!("file:{}", data_dir.path().join("events.db").display());
    let store = temper_store_turso::TursoEventStore::new(&db_url, None)
        .await
        .expect("local turso db");
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        parse_csdl(FILE_CSDL_XML).expect("File CSDL"),
        FILE_CSDL_XML.to_string(),
        &[("File", FILE_IOA)],
    );
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
            verified_at: "2026-10-23T00:00:00Z".to_string(),
        }),
    );
    let mut state = ServerState::from_registry(ActorSystem::new(name), registry);
    state.set_storage_stack(StorageStack::from_turso(store));
    state.data_dir = data_dir.path().to_path_buf();
    state
        .authz
        .reload_tenant_policies(
            "default",
            r#"permit(principal, action, resource is File) when { principal.id == "file-owner" };"#,
        )
        .expect("owner-only File policy");
    let tenant = TenantId::default();
    state
        .get_or_create_tenant_entity(&tenant, "File", "doc-7", serde_json::json!({}))
        .await
        .expect("create File");
    state
        .put_file_stream_content(
            &tenant,
            "doc-7",
            OWNER_TEXT.as_bytes(),
            "text/plain",
            &AgentContext::for_service("file-owner"),
        )
        .await
        .expect("owner's bytes");
    (state, data_dir)
}

/// A guest running for `caller`. Its delegate refuses every call, so each answer below came from the in-process
/// OData handlers.
fn guest_for(state: &ServerState, caller: &str) -> LocalTDataWasmHost {
    LocalTDataWasmHost::new(
        state.clone(),
        TenantId::default(),
        Some(&SecurityContext::from_resolved_identity(
            caller, "operator", None,
        )),
        "http_fetch",
        Arc::new(FailingHost),
    )
}

async fn file_content(state: &ServerState) -> (String, serde_json::Value) {
    let file = state
        .get_tenant_entity_state(&TenantId::default(), "File", "doc-7")
        .await
        .expect("File");
    (
        file.state.fields["content_hash"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        file.state.fields["version_count"].clone(),
    )
}

const VALUE_URL: &str = "http://127.0.0.1:8787/tdata/Files('doc-7')/$value";

#[tokio::test]
async fn a_guest_reads_a_files_value_only_with_its_callers_read_grant() {
    let (state, _dir) = owned_file_state("local-file-value-read").await;

    let (status, body) = guest_for(&state, "other-member")
        .http_call("GET", VALUE_URL, &[], "")
        .await
        .expect("served in-process");
    assert!(matches!(status, 403 | 404), "status {status}");
    assert!(!body.contains(OWNER_TEXT), "{body}");

    let (status, body) = guest_for(&state, "file-owner")
        .http_call("GET", VALUE_URL, &[], "")
        .await
        .expect("served in-process");
    assert_eq!((status, body.as_str()), (200, OWNER_TEXT));
}

#[tokio::test]
async fn a_guest_writes_a_files_value_only_with_its_callers_update_grant() {
    let (state, _dir) = owned_file_state("local-file-value-write").await;
    let before = file_content(&state).await;
    let headers = vec![("content-type".to_string(), "text/plain".to_string())];

    let (status, _) = guest_for(&state, "other-member")
        .http_call("PUT", VALUE_URL, &headers, "other member's text")
        .await
        .expect("served in-process");
    assert!(matches!(status, 403 | 404), "status {status}");
    assert_eq!(file_content(&state).await, before, "the File is unchanged");

    let (status, _) = guest_for(&state, "file-owner")
        .http_call("PUT", VALUE_URL, &headers, "owner's second text")
        .await
        .expect("served in-process");
    assert_eq!(status, 204);
    let (status, body) = guest_for(&state, "file-owner")
        .http_call("GET", VALUE_URL, &[], "")
        .await
        .expect("served in-process");
    assert_eq!((status, body.as_str()), (200, "owner's second text"));
}
