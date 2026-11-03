//! Tests for cross-entity guard resolution: required refs (ARN-92 #2) and lists.

use crate::registry::SpecRegistry;
use crate::state::ServerState;
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_spec::csdl::parse_csdl;

const CSDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.RequiredRefTest" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Doc">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
        <Property Name="landing_file_id" Type="Edm.String"/>
        <Property Name="child_ids" Type="Edm.String"/>
      </EntityType>
      <EntityType Name="File">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Docs" EntityType="Temper.RequiredRefTest.Doc"/>
        <EntitySet Name="Files" EntityType="Temper.RequiredRefTest.File"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

/// Doc with a *required* scalar cross-entity guard on `landing_file_id`.
const DOC_REQUIRED_SCALAR: &str = r#"
[automaton]
name = "Doc"
states = ["Draft", "Submitted"]
initial = "Draft"

[[state]]
name = "landing_file_id"
type = "string"
initial = ""

[[action]]
name = "Submit"
from = ["Draft"]
to = "Submitted"
guard = "File[landing_file_id].status in ['Ready']"
"#;

/// Doc with a *required* list cross-entity guard on `child_ids`.
const DOC_REQUIRED_LIST: &str = r#"
[automaton]
name = "Doc"
states = ["Draft", "Submitted"]
initial = "Draft"

[[state]]
name = "child_ids"
type = "string"
initial = "[]"

[[action]]
name = "Submit"
from = ["Draft"]
to = "Submitted"
guard = "File[child_ids].status in ['Ready']"
"#;

/// Doc with an *optional* list cross-entity guard on `child_ids` (legacy
/// vacuous-true blast radius preserved).
const DOC_OPTIONAL_LIST: &str = r#"
[automaton]
name = "Doc"
states = ["Draft", "Submitted"]
initial = "Draft"

[[state]]
name = "child_ids"
type = "string"
initial = "[]"

[[action]]
name = "Submit"
from = ["Draft"]
to = "Submitted"
guard = "empty(child_ids) || File[child_ids].status in ['Ready']"
"#;

/// Doc with a *denylist* cross-entity guard on `landing_file_id`: the action
/// is rejected only when the referenced File is in a forbidden status, while a
/// missing/unresolved File is allowed (the workspace-freeze shape).
const DOC_FORBIDDEN_SCALAR: &str = r#"
[automaton]
name = "Doc"
states = ["Draft", "Submitted"]
initial = "Draft"

[[state]]
name = "landing_file_id"
type = "string"
initial = ""

[[action]]
name = "Submit"
from = ["Draft"]
to = "Submitted"
guard = "File[landing_file_id].status not in ['Archived', 'Locked']"
"#;

/// Doc with an *optional* denylist scalar guard pointing at an UNREGISTERED
/// entity type (`Ghost`). `resolve_entity_status` returns `None` for a type
/// with no spec, exercising the genuine missing-target branch (no auto-spawn).
/// A non-required denylist guard must ALLOW: there is no resolvable container
/// in a bad state.
const DOC_FORBIDDEN_GHOST: &str = r#"
[automaton]
name = "Doc"
states = ["Draft", "Submitted"]
initial = "Draft"

[[state]]
name = "ghost_id"
type = "string"
initial = ""

[[action]]
name = "Submit"
from = ["Draft"]
to = "Submitted"
guard = "Ghost[ghost_id].status not in ['Archived', 'Locked']"
"#;

/// Same as `DOC_FORBIDDEN_GHOST` but the ref is mandatory (`required = true`),
/// so a non-empty ref to an unresolvable `Ghost` must FAIL — pinning the
/// scalar/list symmetry for required denylist refs.
const DOC_FORBIDDEN_REQUIRED_GHOST: &str = r#"
[automaton]
name = "Doc"
states = ["Draft", "Submitted"]
initial = "Draft"

[[state]]
name = "ghost_id"
type = "string"
initial = ""

[[action]]
name = "Submit"
from = ["Draft"]
to = "Submitted"
guard = "Ghost[ghost_id].status != null && Ghost[ghost_id].status not in ['Archived', 'Locked']"
"#;

/// A File automaton with the statuses the denylist test references, plus an
/// action to drive it into a forbidden state.
const FILE_IOA: &str = r#"
[automaton]
name = "File"
states = ["Ready", "Locked", "Archived"]
initial = "Ready"

[[action]]
name = "Lock"
from = ["Ready"]
to = "Locked"
"#;

/// Whether `Doc('doc-1').Submit`'s guard passes, through the production path:
/// resolve the related entities, build the evaluation context, evaluate.
async fn submit_guard_passes(state: &ServerState, tenant: &TenantId) -> bool {
    let related = state
        .resolve_related_statuses(tenant, "Doc", "doc-1", "Submit")
        .await;
    let current = state
        .get_tenant_entity_state(tenant, "Doc", "doc-1")
        .await
        .expect("Doc state")
        .state;
    let table = state
        .registry
        .read()
        .unwrap()
        .get_spec(tenant, "Doc")
        .expect("Doc spec")
        .table();
    let ctx = crate::entity_actor::effects::build_eval_context_with_xref(
        &current,
        &related,
        &table,
        Some("Submit"),
    );
    table
        .evaluate_ctx(&current.status, &ctx, "Submit")
        .expect("Submit rule")
        .success
}

async fn state_with(doc_ioa: &str, test_name: &str) -> (ServerState, TenantId) {
    let csdl = parse_csdl(CSDL).expect("CSDL parses");
    let mut registry = SpecRegistry::new();
    registry.register_tenant("default", csdl, CSDL.to_string(), &[("Doc", doc_ioa)]);
    let state = ServerState::from_registry(ActorSystem::new(test_name), registry);
    (state, TenantId::default())
}

async fn state_with_doc_and_file(doc_ioa: &str, test_name: &str) -> (ServerState, TenantId) {
    let csdl = parse_csdl(CSDL).expect("CSDL parses");
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        csdl,
        CSDL.to_string(),
        &[("Doc", doc_ioa), ("File", FILE_IOA)],
    );
    let state = ServerState::from_registry(ActorSystem::new(test_name), registry);
    (state, TenantId::default())
}

#[tokio::test]
async fn required_empty_scalar_ref_fails_guard() {
    let (state, tenant) = state_with(DOC_REQUIRED_SCALAR, "required-empty-scalar").await;
    state
        .get_or_create_tenant_entity(
            &tenant,
            "Doc",
            "doc-1",
            serde_json::json!({ "landing_file_id": "" }),
        )
        .await
        .expect("create Doc");

    assert!(
        !submit_guard_passes(&state, &tenant).await,
        "an empty required scalar ref must fail the guard, not pass vacuously"
    );
}

#[tokio::test]
async fn required_empty_list_ref_fails_guard() {
    let (state, tenant) = state_with(DOC_REQUIRED_LIST, "required-empty-list").await;
    state
        .get_or_create_tenant_entity(
            &tenant,
            "Doc",
            "doc-1",
            serde_json::json!({ "child_ids": [] }),
        )
        .await
        .expect("create Doc");

    assert!(
        !submit_guard_passes(&state, &tenant).await,
        "an empty required list ref must fail the guard"
    );
}

#[tokio::test]
async fn optional_empty_list_ref_stays_vacuous_true() {
    let (state, tenant) = state_with(DOC_OPTIONAL_LIST, "optional-empty-list").await;
    state
        .get_or_create_tenant_entity(
            &tenant,
            "Doc",
            "doc-1",
            serde_json::json!({ "child_ids": [] }),
        )
        .await
        .expect("create Doc");

    assert!(
        submit_guard_passes(&state, &tenant).await,
        "an empty optional list ref must stay vacuous-true (preserve blast radius)"
    );
}

#[tokio::test]
async fn forbidden_status_missing_target_allows() {
    // A denylist guard pointing at a non-empty ref whose target entity does not
    // resolve (an UNREGISTERED type → `resolve_entity_status` returns `None`,
    // no auto-spawn) must ALLOW the action: there is no container in a bad state
    // to honour. This is the regression shape (system-doc Files referencing an
    // app-docs Workspace that may not be resolvable at the moment of write).
    let (state, tenant) = state_with(DOC_FORBIDDEN_GHOST, "forbidden-ghost").await;
    state
        .get_or_create_tenant_entity(
            &tenant,
            "Doc",
            "doc-1",
            serde_json::json!({ "ghost_id": "ghost-does-not-exist" }),
        )
        .await
        .expect("create Doc");

    assert!(
        submit_guard_passes(&state, &tenant).await,
        "a denylist guard must allow a non-empty ref to an UNRESOLVABLE target"
    );
}

#[tokio::test]
async fn forbidden_status_allowed_target_allows() {
    // Target resolvable and NOT in a forbidden status (Ready) → allow.
    let (state, tenant) = state_with_doc_and_file(DOC_FORBIDDEN_SCALAR, "forbidden-ready").await;
    state
        .get_or_create_tenant_entity(&tenant, "File", "file-1", serde_json::json!({}))
        .await
        .expect("create File (Ready)");
    state
        .get_or_create_tenant_entity(
            &tenant,
            "Doc",
            "doc-1",
            serde_json::json!({ "landing_file_id": "file-1" }),
        )
        .await
        .expect("create Doc");

    assert!(
        submit_guard_passes(&state, &tenant).await,
        "a denylist guard must allow a target whose status is not forbidden"
    );
}

#[tokio::test]
async fn forbidden_status_forbidden_target_rejects() {
    // Target resolvable and IN a forbidden status (Locked) → reject. This is the
    // intent that must remain enforced (the Frozen/Archived-workspace analogue).
    let (state, tenant) = state_with_doc_and_file(DOC_FORBIDDEN_SCALAR, "forbidden-locked").await;
    state
        .get_or_create_tenant_entity(&tenant, "File", "file-1", serde_json::json!({}))
        .await
        .expect("create File");
    state
        .dispatch_tenant_action(
            &tenant,
            "File",
            "file-1",
            "Lock",
            serde_json::json!({}),
            &crate::request_context::AgentContext::for_service("test"),
        )
        .await
        .expect("lock File");
    state
        .get_or_create_tenant_entity(
            &tenant,
            "Doc",
            "doc-1",
            serde_json::json!({ "landing_file_id": "file-1" }),
        )
        .await
        .expect("create Doc");

    assert!(
        !submit_guard_passes(&state, &tenant).await,
        "a denylist guard must reject a target that IS in a forbidden status"
    );
}

#[tokio::test]
async fn forbidden_status_required_missing_target_rejects() {
    // A *required* denylist scalar ref to an UNRESOLVABLE target must fail,
    // matching the required-list branch (the relation was declared mandatory).
    // This pins the scalar/list symmetry so the two paths never diverge.
    let (state, tenant) =
        state_with(DOC_FORBIDDEN_REQUIRED_GHOST, "forbidden-required-ghost").await;
    state
        .get_or_create_tenant_entity(
            &tenant,
            "Doc",
            "doc-1",
            serde_json::json!({ "ghost_id": "ghost-does-not-exist" }),
        )
        .await
        .expect("create Doc");

    assert!(
        !submit_guard_passes(&state, &tenant).await,
        "a REQUIRED denylist scalar ref to an unresolvable target must fail (mandatory relation)"
    );
}

#[tokio::test]
async fn list_ref_resolves_every_id_past_sixteen() {
    // A guard over a list reads every entity in it. Until this change a list
    // past 16 ids became unresolved and failed every guard that read it, so
    // "all 17 files are Ready" could never be true.
    let (state, tenant) = state_with_doc_and_file(DOC_REQUIRED_LIST, "list-ref-17").await;
    let ids: Vec<String> = (1..=17).map(|n| format!("file-{n}")).collect();
    for id in &ids {
        state
            .get_or_create_tenant_entity(&tenant, "File", id, serde_json::json!({}))
            .await
            .expect("create File (Ready)");
    }
    state
        .get_or_create_tenant_entity(
            &tenant,
            "Doc",
            "doc-1",
            serde_json::json!({ "child_ids": ids }),
        )
        .await
        .expect("create Doc");
    assert!(
        submit_guard_passes(&state, &tenant).await,
        "17 Ready files must pass a guard that every file is Ready"
    );

    // The last id is read too: one Locked file fails the guard.
    state
        .dispatch_tenant_action(
            &tenant,
            "File",
            "file-17",
            "Lock",
            serde_json::json!({}),
            &crate::request_context::AgentContext::for_service("test"),
        )
        .await
        .expect("lock File");
    assert!(
        !submit_guard_passes(&state, &tenant).await,
        "the 17th file's status must be read"
    );
}

#[tokio::test]
async fn every_spawn_in_a_transition_runs() {
    // Nine spawn effects in one action: every child is created and initialized.
    // Until this change the ninth was dropped with only a log line.
    let spawns: Vec<String> = (1..=9)
        .map(|n| format!("\"spawn('File', 'Lock', file_{n}_id, params.f{n})\""))
        .collect();
    let params: Vec<String> = (1..=9).map(|n| format!("\"f{n}\"")).collect();
    let doc = format!(
        r#"
[automaton]
name = "Doc"
states = ["Draft", "Submitted"]
initial = "Draft"

[[action]]
name = "Fan"
from = ["Draft"]
to = "Submitted"
params = [{}]
effect = [{}]
"#,
        params.join(", "),
        spawns.join(", ")
    );
    let (state, tenant) = state_with_doc_and_file(&doc, "spawn-nine").await;
    let args: serde_json::Map<String, serde_json::Value> = (1..=9)
        .map(|n| (format!("f{n}"), serde_json::json!(format!("child-{n}"))))
        .collect();
    let response = state
        .dispatch_tenant_action(
            &tenant,
            "Doc",
            "doc-1",
            "Fan",
            serde_json::Value::Object(args),
            &crate::request_context::AgentContext::for_service("test"),
        )
        .await
        .expect("Fan");
    assert!(response.success, "Fan: {:?}", response.error);

    for n in 1..=9 {
        let id = format!("child-{n}");
        let mut status = String::new();
        for _ in 0..100 {
            status = state
                .get_tenant_entity_state(&tenant, "File", &id)
                .await
                .expect("File state")
                .state
                .status;
            if status == "Locked" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(status, "Locked", "{id} was not spawned and initialized");
    }
}
