//! Regressions for declared-key ownership across non-action write surfaces.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use temper_jit::table::TransitionTable;
use temper_jit::table::types::DeclaredKey;
use temper_runtime::ActorSystem;
use temper_runtime::persistence::PersistenceError;
use temper_runtime::scheduler::{install_deterministic_context, sim_uuid};
use temper_runtime::tenant::TenantId;
use temper_server::key_index::{canonical_key_hash, declared_key_set_signature};
use temper_server::registry::SpecRegistry;
use temper_server::storage::{
    BackendLabel, BoxedEventStore, QueryPlaneStore, QueryProjectionFieldsRow, StorageStack,
};
use temper_server::{EntityActor, EntityMsg, EntityResponse, ServerState};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;

#[path = "key_ownership_write_surfaces/field_update_recovery.rs"]
mod field_update_recovery;
#[path = "key_ownership_write_surfaces/key_contract_aba.rs"]
mod key_contract_aba;

const CSDL_XML: &str = include_str!("../../../test-fixtures/specs/model.csdl.xml");
const DOC_IOA: &str = include_str!("../../../test-fixtures/specs/keyed_doc.ioa.toml");
const DATA_ONLY_DOC_IOA: &str = r#"
[automaton]
name = "Doc"
states = ["Ready"]
initial = "Ready"

[[state]]
name = "WorkspaceId"
type = "string"
initial = ""

[[state]]
name = "Path"
type = "string"
initial = ""

[[key]]
name = "path"
properties = ["WorkspaceId", "Path"]
"#;
fn doc_key_hash(workspace: &str, path: &str) -> String {
    canonical_key_hash(
        "path",
        &["WorkspaceId".to_string(), "Path".to_string()],
        serde_json::json!({"WorkspaceId": workspace, "Path": path})
            .as_object()
            .expect("key fields"),
    )
    .expect("complete declared key")
}

async fn action(
    actor: &temper_runtime::actor::ActorRef<EntityMsg>,
    name: &str,
    params: serde_json::Value,
) -> EntityResponse {
    actor
        .ask(
            EntityMsg::Action {
                name: name.to_string(),
                params,
                cross_entity_booleans: BTreeMap::new(),
                idempotency_key: None,
            },
            Duration::from_secs(5),
        )
        .await
        .expect("action response")
}

async fn update(
    actor: &temper_runtime::actor::ActorRef<EntityMsg>,
    fields: serde_json::Value,
    replace: bool,
) -> EntityResponse {
    actor
        .ask(
            EntityMsg::UpdateFields {
                fields,
                replace,
                idempotency_key: format!("test-field-update:{}", sim_uuid()),
            },
            Duration::from_secs(5),
        )
        .await
        .expect("field update response")
}

async fn state(actor: &temper_runtime::actor::ActorRef<EntityMsg>) -> EntityResponse {
    actor
        .ask(EntityMsg::GetState, Duration::from_secs(5))
        .await
        .expect("state response")
}

/// PATCH and PUT are durable writes: they must atomically move declared-key
/// ownership, roll back a uniqueness reject, and replay after actor restart.
#[tokio::test]
async fn field_updates_reconcile_keys_and_survive_restart() {
    let (_guard, _clock, _ids) = install_deterministic_context(238);
    let sim = SimEventStore::no_faults(238);
    let events = BoxedEventStore::new(sim.clone());
    let table = Arc::new(RwLock::new(TransitionTable::from_ioa_source(DOC_IOA)));
    let system = ActorSystem::new("arn238-field-updates");

    let first = system.spawn(
        EntityActor::with_persistence(
            "Doc",
            "doc-a",
            table.clone(),
            serde_json::json!({}),
            events.clone(),
            BackendLabel::Sim,
        )
        .with_tenant("default"),
        "doc-a",
    );
    let blocker = system.spawn(
        EntityActor::with_persistence(
            "Doc",
            "doc-b",
            table.clone(),
            serde_json::json!({}),
            events.clone(),
            BackendLabel::Sim,
        )
        .with_tenant("default"),
        "doc-b",
    );

    assert!(
        action(
            &first,
            "Create",
            serde_json::json!({"WorkspaceId": "ws", "Path": "/old"}),
        )
        .await
        .success
    );
    assert!(
        action(
            &blocker,
            "Create",
            serde_json::json!({"WorkspaceId": "ws", "Path": "/blocked"}),
        )
        .await
        .success
    );
    let created_sequence = state(&first).await.state.sequence_nr;

    let patched = update(
        &first,
        serde_json::json!({"Path": "/patched", "Note": "keep-until-put"}),
        false,
    )
    .await;
    assert!(patched.success, "PATCH failed: {:?}", patched.error);
    assert_eq!(patched.state.sequence_nr, created_sequence + 1);
    assert_eq!(
        events
            .lookup_by_key("default", "Doc", "path", &doc_key_hash("ws", "/old"))
            .await
            .expect("old key lookup"),
        None,
        "PATCH must release the old declared key"
    );
    assert_eq!(
        events
            .lookup_by_key("default", "Doc", "path", &doc_key_hash("ws", "/patched"),)
            .await
            .expect("new key lookup"),
        Some("doc-a".to_string()),
        "PATCH must claim the new declared key"
    );

    let rejected = update(&first, serde_json::json!({"Path": "/blocked"}), false).await;
    assert!(
        !rejected.success,
        "a conflicting PATCH must be rejected instead of mutating memory only"
    );
    assert_eq!(rejected.state.fields["Path"], "/patched");
    assert_eq!(rejected.state.sequence_nr, patched.state.sequence_nr);

    let replaced = update(
        &first,
        serde_json::json!({"WorkspaceId": "ws", "Path": "/put"}),
        true,
    )
    .await;
    assert!(replaced.success, "PUT failed: {:?}", replaced.error);
    assert_eq!(replaced.state.sequence_nr, patched.state.sequence_nr + 1);
    assert!(replaced.state.fields.get("Note").is_none());
    assert_eq!(replaced.state.fields["Id"], "doc-a");
    assert_eq!(
        events
            .lookup_by_key("default", "Doc", "path", &doc_key_hash("ws", "/patched"),)
            .await
            .expect("patched key lookup"),
        None
    );
    assert_eq!(
        events
            .lookup_by_key("default", "Doc", "path", &doc_key_hash("ws", "/put"))
            .await
            .expect("PUT key lookup"),
        Some("doc-a".to_string())
    );

    drop(first);
    drop(blocker);
    drop(system);

    let restarted = ActorSystem::new("arn238-field-updates-restarted");
    let recovered = restarted.spawn(
        EntityActor::with_persistence(
            "Doc",
            "doc-a",
            table,
            serde_json::json!({}),
            events,
            BackendLabel::Sim,
        )
        .with_tenant("default"),
        "doc-a",
    );
    let recovered = state(&recovered).await;
    assert_eq!(recovered.state.fields["Path"], "/put");
    assert_eq!(recovered.state.sequence_nr, replaced.state.sequence_nr);
    assert!(recovered.state.fields.get("Note").is_none());
}

#[derive(Default)]
struct NoopQueryPlane;

#[async_trait]
impl QueryPlaneStore for NoopQueryPlane {
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
        Ok(())
    }

    async fn remove_projection(
        &self,
        _tenant: &str,
        _entity_type: &str,
        _entity_id: &str,
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

/// The data-only create optimization must preserve the same declared-key
/// co-commit and uniqueness behavior as actor-backed creates.
#[tokio::test]
async fn data_only_create_co_commits_declared_keys() {
    let (_guard, _clock, _ids) = install_deterministic_context(239);
    let tenant = TenantId::default();
    let sim = SimEventStore::no_faults(239);
    let events = BoxedEventStore::new(sim.clone());
    let csdl = parse_csdl(CSDL_XML).expect("CSDL parse");
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        tenant.as_str(),
        csdl,
        CSDL_XML.to_string(),
        &[("Doc", DATA_ONLY_DOC_IOA)],
    );
    let mut server = ServerState::from_registry(ActorSystem::new("arn238-data-only"), registry);
    server.set_storage_stack(StorageStack::new(
        BackendLabel::Sim,
        events.clone(),
        None,
        None,
        None,
        None,
        Some(Arc::new(NoopQueryPlane)),
        None,
        None,
        None,
    ));

    let created = server
        .try_create_data_only_tenant_entity(
            &tenant,
            "Doc",
            "doc-fast-a",
            serde_json::json!({"WorkspaceId": "ws", "Path": "/fast"}),
        )
        .await
        .expect("data-only create result")
        .expect("eligible data-only create");
    assert!(created.success);
    assert_eq!(
        events
            .lookup_by_key("default", "Doc", "path", &doc_key_hash("ws", "/fast"))
            .await
            .expect("fast-path key lookup"),
        Some("doc-fast-a".to_string()),
        "the optimized create must not leave a post-watermark entity unindexed"
    );

    let duplicate = server
        .try_create_data_only_tenant_entity(
            &tenant,
            "Doc",
            "doc-fast-b",
            serde_json::json!({"WorkspaceId": "ws", "Path": "/fast"}),
        )
        .await;
    assert!(
        !matches!(duplicate, Ok(Some(response)) if response.success),
        "a duplicate declared key must not create a second data-only entity"
    );
    assert!(
        events
            .read_events("default:Doc:doc-fast-b", 0)
            .await
            .expect("duplicate journal read")
            .is_empty(),
        "a rejected duplicate must leave the journal unchanged"
    );
}

/// A watermark covers the full ordered key definition, not merely its name.
#[test]
fn key_signature_changes_when_same_named_key_properties_change() {
    let original = [DeclaredKey {
        name: "path".to_string(),
        properties: vec!["WorkspaceId".to_string(), "Path".to_string()],
    }];
    let changed = [DeclaredKey {
        name: "path".to_string(),
        properties: vec!["WorkspaceId".to_string(), "ParentId".to_string()],
    }];
    assert_ne!(
        declared_key_set_signature(&original),
        declared_key_set_signature(&changed),
        "a same-name key definition change must invalidate the old coverage watermark"
    );

    let reordered_declarations = [
        DeclaredKey {
            name: "z".to_string(),
            properties: vec!["Z".to_string()],
        },
        DeclaredKey {
            name: "a".to_string(),
            properties: vec!["A".to_string(), "B".to_string()],
        },
    ];
    let same_definitions = [
        DeclaredKey {
            name: "a".to_string(),
            properties: vec!["A".to_string(), "B".to_string()],
        },
        DeclaredKey {
            name: "z".to_string(),
            properties: vec!["Z".to_string()],
        },
    ];
    assert_eq!(
        declared_key_set_signature(&reordered_declarations),
        declared_key_set_signature(&same_definitions),
        "declaration order must not make the coverage signature nondeterministic"
    );

    let names = original
        .iter()
        .map(|key| key.name.clone())
        .collect::<BTreeSet<_>>();
    assert_eq!(names, BTreeSet::from(["path".to_string()]));
}
