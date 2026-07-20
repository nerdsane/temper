use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use sha2::{Digest, Sha256};
use temper_runtime::ActorSystem;
use temper_runtime::persistence::{EventStore, PersistenceError};
use temper_runtime::tenant::TenantId;
use temper_server::registry::SpecRegistry;
use temper_server::registry::{EntityLevelSummary, EntityVerificationResult, VerificationStatus};
use temper_server::request_context::AgentContext;
use temper_server::state::IndexedFileStreamRead;
use temper_server::storage::{QueryPlaneStore, QueryProjectionFieldsRow, StorageStack};
use temper_server::{ServerState, build_router};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;
use temper_store_turso::TursoEventStore;
use tower::ServiceExt;

#[path = "file_value_fast_path/cancellation.rs"]
mod cancellation;

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

const TIMED_FILE_IOA: &str = r#"
[automaton]
name = "File"
states = ["Created", "Ready", "TimedOut"]
initial = "Created"
allow_indefinite_states = ["Created", "TimedOut"]

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

[[action]]
name = "TimeoutFail"
kind = "internal"
from = ["Ready"]
to = "TimedOut"

[[state_timeout]]
state = "Ready"
after_seconds = 60
on_timeout = "TimeoutFail"
"#;

struct FailingQueryPlane;

#[async_trait::async_trait]
impl QueryPlaneStore for FailingQueryPlane {
    async fn upsert_projection(
        &self,
        _tenant: &str,
        _entity_type: &str,
        _entity_id: &str,
        _status: &str,
        _fields: &serde_json::Value,
        _state: &serde_json::Value,
        _sequence_nr: u64,
    ) -> Result<(), PersistenceError> {
        Err(PersistenceError::Storage(
            "injected query projection failure".to_string(),
        ))
    }

    async fn remove_projection(
        &self,
        _tenant: &str,
        _entity_type: &str,
        _entity_id: &str,
        _sequence_nr: u64,
    ) -> Result<(), PersistenceError> {
        Ok(())
    }

    async fn query_field_index(
        &self,
        _tenant: &str,
        _entity_type: &str,
        _where_clause: &str,
        _params: Vec<String>,
    ) -> Result<Option<Vec<String>>, PersistenceError> {
        Ok(None)
    }

    async fn load_projection_fields_many(
        &self,
        _tenant: &str,
        _entity_type: &str,
        _entity_ids: &[String],
        _field_names: &[&str],
    ) -> Result<Option<Vec<QueryProjectionFieldsRow>>, PersistenceError> {
        Ok(None)
    }

    async fn projected_entity_counts_by_tenant(
        &self,
    ) -> Result<Option<Vec<(String, u64)>>, PersistenceError> {
        Ok(None)
    }
}

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
    (state, store)
}

fn build_sim_timed_file_state(seed: u64) -> (ServerState, SimEventStore) {
    build_sim_timed_file_state_with_query_plane(seed, None)
}

fn build_sim_timed_file_state_with_query_plane(
    seed: u64,
    query_plane: Option<Arc<dyn QueryPlaneStore>>,
) -> (ServerState, SimEventStore) {
    let store = SimEventStore::no_faults(seed);
    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(FILE_CSDL_XML).expect("timed File CSDL should parse");
    registry.register_tenant(
        "default",
        csdl,
        FILE_CSDL_XML.to_string(),
        &[("File", TIMED_FILE_IOA)],
    );
    let mut state = ServerState::from_registry(ActorSystem::new("timed-file-create"), registry);
    let mut storage = StorageStack::from_sim(store.clone(), None);
    storage.query_plane = query_plane;
    state.set_storage_stack(storage);
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

#[path = "file_value_fast_path/http_read.rs"]
mod http_read;
#[path = "file_value_fast_path/initial.rs"]
mod initial;
#[path = "file_value_fast_path/workspace.rs"]
mod workspace;
#[path = "file_value_fast_path/writes.rs"]
mod writes;
