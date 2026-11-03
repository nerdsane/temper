//! A spec action named `Deleted` is the spec's own transition, never the
//! kernel's deletion tombstone. Records that ran it stay listed, loaded and
//! filterable after a restart on the same Turso store, and their journals
//! replay past it. A record the kernel deleted stays gone.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use temper_runtime::ActorSystem;
use temper_runtime::persistence::EventStore;
use temper_runtime::tenant::TenantId;
use temper_server::build_router;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::state::ServerState;
use temper_server::storage::StorageStack;
use temper_spec::csdl::parse_csdl;
use temper_store_turso::TursoEventStore;
use tower::ServiceExt;

const KEEPSAKE_IOA: &str = r#"
[automaton]
name = "Keepsake"
states = ["Kept", "Deleting", "Gone"]
initial = "Kept"

[[state]]
name = "Note"
type = "string"
initial = ""

[[action]]
name = "Keep"
kind = "input"
from = ["Kept"]
to = "Kept"
params = ["Note"]

[[action]]
name = "Expire"
kind = "input"
from = ["Kept"]
to = "Deleting"

[[action]]
name = "Deleted"
kind = "input"
from = ["Deleting"]
to = "Gone"
params = ["Note"]

[[action]]
name = "Annotate"
kind = "input"
from = ["Gone"]
to = "Gone"
params = ["Note"]
"#;

const CSDL_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Test" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Keepsake">
        <Key>
          <PropertyRef Name="Id"/>
        </Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Note" Type="Edm.String"/>
        <Property Name="Status" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="TestService">
        <EntitySet Name="Keepsakes" EntityType="Temper.Test.Keepsake"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>
"#;

const READ_POLICY: &str = r#"
    permit(
        principal,
        action in [Action::"list", Action::"read"],
        resource is Keepsake
    );
"#;

const TYPE: &str = "Keepsake";

fn build_state(system_name: &str, store: TursoEventStore) -> ServerState {
    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(CSDL_XML).expect("CSDL should parse");
    registry.register_tenant(
        "default",
        csdl,
        CSDL_XML.to_string(),
        &[(TYPE, KEEPSAKE_IOA)],
    );
    let mut state = ServerState::from_registry(ActorSystem::new(system_name), registry);
    state.set_storage_stack(StorageStack::from_turso(store));
    state
        .authz
        .reload_tenant_policies(TenantId::default().as_str(), READ_POLICY)
        .expect("install Keepsake read policy");
    state
}

async fn act(state: &ServerState, id: &str, action: &str, params: serde_json::Value) {
    let response = state
        .dispatch_tenant_action(
            &TenantId::default(),
            TYPE,
            id,
            action,
            params,
            &AgentContext::default(),
        )
        .await
        .unwrap_or_else(|error| panic!("{action} on {id}: {error}"));
    assert!(response.success, "{action} on {id}: {:?}", response.error);
}

async fn get_json(state: &ServerState, path: &str) -> (StatusCode, serde_json::Value) {
    let router = build_router(state.clone());
    let mut req = Request::builder().uri(path).body(Body::empty()).unwrap();
    req.extensions_mut()
        .insert(temper_authz::AuthenticatedRequestContext::new(
            TenantId::default(),
            temper_authz::SecurityContext::from_resolved_identity(
                "spec-action-named-deleted",
                "test-customer",
                None,
            ),
        ));
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000)
        .await
        .unwrap();
    let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, body)
}

fn listed_ids(body: &serde_json::Value) -> Vec<String> {
    let mut ids: Vec<String> = body["value"]
        .as_array()
        .unwrap_or_else(|| panic!("collection body: {body}"))
        .iter()
        .filter_map(|entity| {
            entity["entity_id"]
                .as_str()
                .or_else(|| entity["Id"].as_str())
                .map(str::to_string)
        })
        .collect();
    ids.sort();
    ids
}

#[tokio::test]
async fn records_that_ran_a_spec_action_named_deleted_survive_a_restart() {
    let db_path = std::env::temp_dir().join(format!(
        "temper-spec-action-named-deleted-{}.db",
        uuid::Uuid::new_v4()
    ));
    let store = TursoEventStore::new(&format!("file:{}", db_path.display()), None)
        .await
        .expect("create local turso db");
    let tenant = TenantId::default();

    let state = build_state("spec-deleted-before", store.clone());
    // `annotated` runs the spec's Deleted with params and acts after it;
    // `bare` runs it with none, the same shape the kernel's delete once wrote.
    act(
        &state,
        "annotated",
        "Keep",
        serde_json::json!({ "Note": "kept" }),
    )
    .await;
    act(&state, "annotated", "Expire", serde_json::json!({})).await;
    act(
        &state,
        "annotated",
        "Deleted",
        serde_json::json!({ "Note": "purged" }),
    )
    .await;
    act(
        &state,
        "annotated",
        "Annotate",
        serde_json::json!({ "Note": "after" }),
    )
    .await;
    act(
        &state,
        "bare",
        "Keep",
        serde_json::json!({ "Note": "kept" }),
    )
    .await;
    act(&state, "bare", "Expire", serde_json::json!({})).await;
    act(&state, "bare", "Deleted", serde_json::json!({})).await;
    act(
        &state,
        "live",
        "Keep",
        serde_json::json!({ "Note": "kept" }),
    )
    .await;
    act(
        &state,
        "removed",
        "Keep",
        serde_json::json!({ "Note": "kept" }),
    )
    .await;
    state
        .delete_tenant_entity(&tenant, TYPE, "removed")
        .await
        .expect("kernel delete");

    let survivors = vec![
        "annotated".to_string(),
        "bare".to_string(),
        "live".to_string(),
    ];

    // The store's own listing keeps every record but the kernel-deleted one.
    let mut listed = store
        .list_entity_ids_by_type(tenant.as_str(), TYPE)
        .await
        .expect("list by type");
    listed.sort();
    assert_eq!(listed, survivors, "store listing by type");

    // Restart on the same store.
    let restarted = build_state("spec-deleted-after", store.clone());
    restarted.populate_index_from_store(&tenant).await;
    let mut indexed = restarted.list_entity_ids_lazy(&tenant, TYPE).await;
    indexed.sort();
    assert_eq!(indexed, survivors, "index after restart");

    for id in ["annotated", "bare"] {
        assert!(
            restarted.ensure_entity_loaded(&tenant, TYPE, id).await,
            "{id} must load after restart"
        );
    }
    assert!(
        !restarted
            .ensure_entity_loaded(&tenant, TYPE, "removed")
            .await,
        "the kernel-deleted record must stay gone"
    );

    // Replay continued past the spec's Deleted: the later Annotate applied.
    let annotated = restarted
        .get_tenant_entity_state(&tenant, TYPE, "annotated")
        .await
        .expect("read annotated");
    assert_eq!(annotated.state.status, "Gone");
    assert_eq!(annotated.state.fields["Note"], "after");

    let (status, body) =
        get_json(&restarted, "/tdata/Keepsakes?$filter=Status%20eq%20'Gone'").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(
        listed_ids(&body),
        vec!["annotated".to_string(), "bare".to_string()],
        "$filter after restart"
    );

    let (status, body) = get_json(&restarted, "/tdata/Keepsakes").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(listed_ids(&body), survivors, "collection after restart");

    let (status, _) = get_json(&restarted, "/tdata/Keepsakes('removed')").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "kernel-deleted record GET");

    let _ = std::fs::remove_file(db_path);
}
