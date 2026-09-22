//! Freeze the derived writer while exercising the actual journal, entity
//! reaction, WASM engine and in-process OData GET. No polling sleeps.
use super::super::*;
use crate::registry::SpecRegistry;
use crate::state::QueryProjectionWriteQueue;
use crate::storage::{EntityCatalogRow, QueryPlaneStore, QueryProjectionFieldsRow, StorageStack};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, Ordering};
use temper_runtime::ActorSystem;
use temper_runtime::persistence::PersistenceError;
use temper_spec::csdl::parse_csdl;
use temper_store_turso::TursoEventStore;

const CSDL: &str = r#"<?xml version="1.0"?><edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx"><edmx:DataServices><Schema Namespace="Test" xmlns="http://docs.oasis-open.org/odata/ns/edm"><EntityType Name="Source"><Key><PropertyRef Name="Id"/></Key><Property Name="Id" Type="Edm.String" Nullable="false"/></EntityType><EntityType Name="Verifier"><Key><PropertyRef Name="Id"/></Key><Property Name="Id" Type="Edm.String" Nullable="false"/></EntityType><EntityContainer Name="Container"><EntitySet Name="Sources" EntityType="Test.Source"/><EntitySet Name="Verifiers" EntityType="Test.Verifier"/></EntityContainer></Schema></edmx:DataServices></edmx:Edmx>"#;
const SOURCE: &str = r#"
[automaton]
name="Source"
states=["Draft","Deleted"]
initial="Draft"
[[action]]
name="Edit"
from=["Draft"]
params=["prompt_template"]
[[action]]
name="Submit"
from=["Draft"]
params=["prompt_template"]
[[action.triggers]]
name="verify_source"
kind="entity"
principal="verifier-service"
target_entity="Verifier"
target_action="Verify"
[action.triggers.resolve_target]
type="field"
field="verifier_id"
[[action]]
name="Delete"
from=["Draft"]
to="Deleted"
params=["prompt_template"]
[[action.triggers]]
name="verify_deleted_source"
kind="entity"
principal="verifier-service"
target_entity="Verifier"
target_action="Verify"
[action.triggers.resolve_target]
type="field"
field="verifier_id"
"#;
const VERIFIER: &str = r#"
[automaton]
name="Verifier"
states=["Idle","Checking","SawNew","SawOld"]
initial="Idle"
[[action]]
name="Verify"
from=["Idle"]
to="Checking"
effect="trigger check_source"
[[action]]
name="SawNew"
from=["Checking"]
to="SawNew"
[[action]]
name="SawOld"
from=["Checking"]
to="SawOld"
[[integration]]
name="check_source"
trigger="check_source"
type="wasm"
module="check_source"
"#;

pub(super) struct FaultQueryPlane {
    inner: Arc<dyn QueryPlaneStore>,
    pub(super) fail_source_write: AtomicBool,
    pub(super) hang_source_write: AtomicBool,
    pub(super) projection_started: tokio::sync::Notify,
}
#[async_trait]
impl QueryPlaneStore for FaultQueryPlane {
    async fn upsert_projection(
        &self,
        tenant: &str,
        kind: &str,
        id: &str,
        status: &str,
        fields: &Value,
        state: &Value,
        seq: u64,
    ) -> Result<(), PersistenceError> {
        if kind == "Source"
            && fields["prompt_template"] == "new"
            && self.hang_source_write.load(Ordering::SeqCst)
        {
            self.projection_started.notify_one();
            return std::future::pending().await;
        }
        if kind == "Source"
            && fields["prompt_template"] == "new"
            && self.fail_source_write.load(Ordering::SeqCst)
        {
            return Err(PersistenceError::Storage(
                "injected projection outage".into(),
            ));
        }
        self.inner
            .upsert_projection(tenant, kind, id, status, fields, state, seq)
            .await
    }
    async fn remove_projection(&self, t: &str, k: &str, id: &str) -> Result<(), PersistenceError> {
        self.inner.remove_projection(t, k, id).await
    }
    async fn query_field_index(
        &self,
        t: &str,
        k: &str,
        w: &str,
        p: Vec<String>,
    ) -> Result<Option<Vec<String>>, PersistenceError> {
        self.inner.query_field_index(t, k, w, p).await
    }
    async fn load_projection_fields_many(
        &self,
        t: &str,
        k: &str,
        ids: &[String],
        names: &[&str],
    ) -> Result<Option<Vec<QueryProjectionFieldsRow>>, PersistenceError> {
        self.inner
            .load_projection_fields_many(t, k, ids, names)
            .await
    }
    async fn load_entity_catalog_rows(
        &self,
        t: &str,
        k: &str,
        ids: &[String],
    ) -> Result<Option<Vec<EntityCatalogRow>>, PersistenceError> {
        self.inner.load_entity_catalog_rows(t, k, ids).await
    }
    async fn projected_entity_counts_by_tenant(
        &self,
    ) -> Result<Option<Vec<(String, u64)>>, PersistenceError> {
        self.inner.projected_entity_counts_by_tenant().await
    }
}

fn verifier_wat() -> String {
    let yes = r#"{"action":"SawNew","params":{},"success":true}"#;
    let no = r#"{"action":"SawOld","params":{},"success":true}"#;
    let encode = |s: &str| s.bytes().map(|b| format!("\\{b:02x}")).collect::<String>();
    let url = "http://localhost/tdata/Sources('source')";
    format!(
        r#"(module
      (import "env" "host_http_call" (func $http (param i32 i32 i32 i32 i32 i32 i32 i32 i32 i32) (result i32)))
      (import "env" "host_set_result" (func $result (param i32 i32)))
      (memory (export "memory") 2)
      (data (i32.const 0) "GET") (data (i32.const 16) "{}")
      (data (i32.const 256) "{}") (data (i32.const 512) "{}")
      (func (export "run") (param i32 i32) (result i32) (local $len i32) (local $i i32)
       i32.const 0 i32.const 3 i32.const 16 i32.const {} i32.const 0 i32.const 0 i32.const 0 i32.const 0 i32.const 4096 i32.const 65536 call $http local.set $len
       (block $old (loop $scan
         local.get $i local.get $len i32.ge_u br_if $old
         local.get $i i32.const 4096 i32.add i32.load i32.const 16777215 i32.and i32.const 7824750 i32.eq
         (if (then i32.const 256 i32.const {} call $result i32.const 0 return))
         local.get $i i32.const 1 i32.add local.set $i br $scan))
       i32.const 512 i32.const {} call $result i32.const 0))"#,
        encode(url),
        encode(yes),
        encode(no),
        url.len(),
        yes.len(),
        no.len()
    )
}

async fn fixture_from_source(
    source: &str,
) -> (
    crate::ServerState,
    Arc<FaultQueryPlane>,
    Arc<QueryProjectionWriteQueue>,
    tempfile::TempDir,
) {
    let temp = tempfile::tempdir().unwrap();
    let store = TursoEventStore::new(
        &format!("file:{}", temp.path().join("events.db").display()),
        None,
    )
    .await
    .unwrap();
    let mut stack = StorageStack::from_turso(store);
    let query = Arc::new(FaultQueryPlane {
        inner: stack.query_plane.clone().unwrap(),
        fail_source_write: AtomicBool::new(false),
        hang_source_write: AtomicBool::new(false),
        projection_started: tokio::sync::Notify::new(),
    });
    stack.query_plane = Some(query.clone());
    let mut registry = SpecRegistry::new();
    registry
        .try_register_tenant_with_reactions(
            "default",
            parse_csdl(CSDL).unwrap(),
            CSDL.into(),
            &[("Source", source), ("Verifier", VERIFIER)],
            Vec::new(),
        )
        .unwrap();
    let mut state =
        crate::ServerState::from_registry(ActorSystem::new("causal-projection"), registry);
    state.set_storage_stack(stack);
    let queue = Arc::new(QueryProjectionWriteQueue::new_for_test(
        query.clone(),
        32,
        32,
    ));
    *state.query_projection_queue.lock().unwrap() = Some(queue.clone());
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .unwrap();
    state.rebuild_reaction_dispatcher();
    let hash = state
        .wasm_engine
        .compile_and_cache(verifier_wat().as_bytes())
        .unwrap();
    state.wasm_module_registry.write().unwrap().register(
        &TenantId::default(),
        "check_source",
        &hash,
    );
    state
        .get_or_create_tenant_entity(
            &TenantId::default(),
            "Source",
            "source",
            json!({"prompt_template":"old","verifier_id":"verifier"}),
        )
        .await
        .unwrap();
    state
        .get_or_create_tenant_entity(&TenantId::default(), "Verifier", "verifier", json!({}))
        .await
        .unwrap();
    (state, query, queue, temp)
}

pub(super) async fn fixture() -> (
    crate::ServerState,
    Arc<FaultQueryPlane>,
    Arc<QueryProjectionWriteQueue>,
    tempfile::TempDir,
) {
    fixture_from_source(SOURCE).await
}

pub(super) async fn fixture_with_timeout() -> (
    crate::ServerState,
    Arc<FaultQueryPlane>,
    Arc<QueryProjectionWriteQueue>,
    tempfile::TempDir,
) {
    let source = format!(
        r#"{SOURCE}
[[action]]
name="Expire"
from=["Draft"]
to="Deleted"
[[state_timeout]]
state="Draft"
after_seconds=20
on_timeout="Expire"
reset_on=["Edit"]
"#
    );
    fixture_from_source(&source).await
}
