use super::*;
use temper_runtime::ActorSystem;
use temper_spec::csdl::parse_csdl;

#[cfg(feature = "sim")]
const COLLECTION_CSDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices><Schema Namespace="Test" xmlns="http://docs.oasis-open.org/odata/ns/edm">
    <EntityType Name="Batch"><Key><PropertyRef Name="Id"/></Key><Property Name="Id" Type="Edm.String" Nullable="false"/></EntityType>
    <EntityType Name="Member"><Key><PropertyRef Name="Id"/></Key><Property Name="Id" Type="Edm.String" Nullable="false"/></EntityType>
    <EntityContainer Name="Container"><EntitySet Name="Batches" EntityType="Test.Batch"/><EntitySet Name="Members" EntityType="Test.Member"/></EntityContainer>
  </Schema></edmx:DataServices>
</edmx:Edmx>"#;

#[cfg(feature = "sim")]
const COLLECTION_SOURCE: &str = r#"
[automaton]
name = "Batch"
states = ["Idle", "Running", "Done"]
initial = "Idle"
allow_indefinite_states = ["Idle", "Done"]
[[state]]
name = "members"
type = "list"
initial = "[]"
[[action]]
name = "Add"
from = ["Idle"]
to = "Idle"
params = ["members"]
effect = [{ type = "list_append", var = "members" }]
[[action]]
name = "Start"
from = ["Idle"]
to = "Running"
[[action]]
name = "Cancel"
from = ["Running"]
to = "Running"
[[action]]
name = "Timeout"
from = ["Running"]
to = "Running"
[[action]]
name = "Joined1"
from = ["Running"]
to = "Done"
params = [{ name = "workflow_id", type = "string" }, { name = "total_members", type = "int" }, { name = "succeeded_members", type = "int" }, { name = "failed_members", type = "int" }, { name = "cancelled_members", type = "int" }, { name = "timed_out_members", type = "int" }]
[[action]]
name = "Joined2"
from = ["Running"]
to = "Done"
params = [{ name = "workflow_id", type = "string" }, { name = "total_members", type = "int" }, { name = "succeeded_members", type = "int" }, { name = "failed_members", type = "int" }, { name = "cancelled_members", type = "int" }, { name = "timed_out_members", type = "int" }]
[[action]]
name = "Joined3"
from = ["Running"]
to = "Done"
params = [{ name = "workflow_id", type = "string" }, { name = "total_members", type = "int" }, { name = "succeeded_members", type = "int" }, { name = "failed_members", type = "int" }, { name = "cancelled_members", type = "int" }, { name = "timed_out_members", type = "int" }]
[[action]]
name = "Joined4"
from = ["Running"]
to = "Done"
params = [{ name = "workflow_id", type = "string" }, { name = "total_members", type = "int" }, { name = "succeeded_members", type = "int" }, { name = "failed_members", type = "int" }, { name = "cancelled_members", type = "int" }, { name = "timed_out_members", type = "int" }]
[[action]]
name = "Joined5"
from = ["Running"]
to = "Done"
params = [{ name = "workflow_id", type = "string" }, { name = "total_members", type = "int" }, { name = "succeeded_members", type = "int" }, { name = "failed_members", type = "int" }, { name = "cancelled_members", type = "int" }, { name = "timed_out_members", type = "int" }]
[[state_timeout]]
state = "Running"
after_seconds = 60
on_timeout = "Timeout"
reset_on = ["Start"]
[[collection_workflow]]
name = "work"
start_action = "Start"
cancel_action = "Cancel"
timeout_action = "Timeout"
roster_field = "members"
member_entity = "Member"
member_action = "Start"
member_cancel_action = "Cancel"
max_members = 8
max_concurrency = 2
max_attempts = 5
on_success = "Joined1"
on_partial_failure = "Joined2"
on_failure = "Joined3"
on_cancelled = "Joined4"
on_timed_out = "Joined5"
"#;

#[cfg(feature = "sim")]
const COLLECTION_MEMBER: &str = r#"
[automaton]
name = "Member"
states = ["Pending", "Running", "Done", "Cancelled"]
initial = "Pending"
allow_indefinite_states = ["Pending", "Running", "Done", "Cancelled"]
[[action]]
name = "Start"
from = ["Pending"]
to = "Running"
params = [{ name = "workflow_id", type = "string" }, { name = "member_id", type = "string" }, { name = "member_value", type = "string" }, { name = "source_entity_id", type = "string" }, { name = "member_index", type = "int" }]
[[action]]
name = "Cancel"
from = ["Running"]
to = "Cancelled"
params = [{ name = "workflow_id", type = "string" }, { name = "member_id", type = "string" }, { name = "member_value", type = "string" }, { name = "source_entity_id", type = "string" }, { name = "member_index", type = "int" }, { name = "requested_outcome", type = "string" }]
"#;

fn test_state() -> crate::state::ServerState {
    let csdl_xml = include_str!("../../../../../test-fixtures/specs/model.csdl.xml");
    let csdl = parse_csdl(csdl_xml).expect("CSDL should parse");
    crate::state::ServerState::new(
        ActorSystem::new("dispatch-wasm-authz-test"),
        csdl,
        csdl_xml.to_string(),
    )
}

#[cfg(feature = "sim")]
fn collection_dispatch_state() -> crate::state::ServerState {
    use crate::state::StorageStack;
    let csdl = parse_csdl(COLLECTION_CSDL).expect("collection CSDL should parse");
    let mut registry = crate::registry::SpecRegistry::new();
    registry.register_tenant(
        "default",
        csdl,
        COLLECTION_CSDL.to_string(),
        &[("Batch", COLLECTION_SOURCE), ("Member", COLLECTION_MEMBER)],
    );
    let store = temper_store_sim::SimEventStore::no_faults(39);
    let mut state = crate::state::ServerState::from_registry(
        ActorSystem::new("collection-normal-dispatch-test"),
        registry,
    );
    state.collection_workflow_mode =
        crate::trigger::collection_workflow::CollectionWorkflowMode::Enabled;
    state.set_storage_stack(StorageStack::from_sim(store, None));
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .expect("test policy should parse");
    state
        .reaction_recovery_generation
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    state
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn normal_dispatch_atomically_starts_and_controls_collection_workflow() {
    use temper_runtime::tenant::TenantId;
    let state = collection_dispatch_state();
    let tenant = TenantId::default();
    let agent = AgentContext::for_service("collection-test");
    state
        .dispatch_tenant_action(
            &tenant,
            "Batch",
            "batch-1",
            "Add",
            serde_json::json!({"members": "raw-1"}),
            &agent,
        )
        .await
        .expect("roster member should register");
    let started = state
        .dispatch_tenant_action(
            &tenant,
            "Batch",
            "batch-1",
            "Start",
            serde_json::json!({}),
            &agent,
        )
        .await
        .expect("collection start should dispatch normally");
    assert!(started.success);
    assert_eq!(started.state.status, "Running");
    let (store, _) = state.event_journal().expect("test journal");
    let workflow_id = crate::trigger::collection_workflow::load_active_source_workflow_id(
        &store, "default", "Batch", "batch-1", "work", None,
    )
    .await
    .expect("active pointer read")
    .expect("active workflow");
    let (record, sequence) = crate::trigger::collection_workflow::load_collection_record(
        &store,
        "default",
        &workflow_id,
    )
    .await
    .expect("workflow read")
    .expect("workflow record");
    assert_eq!(record.sealed_roster, vec!["raw-1"]);
    assert_eq!(record.counts.in_flight, 1);
    assert_eq!(sequence, 1);

    let cancelled = state
        .dispatch_tenant_action(
            &tenant,
            "Batch",
            "batch-1",
            "Cancel",
            serde_json::json!({}),
            &agent,
        )
        .await
        .expect("collection control should dispatch normally");
    assert!(cancelled.success, "collection cancel failed: {cancelled:?}");
    let (controlled, controlled_sequence) =
        crate::trigger::collection_workflow::load_collection_record(
            &store,
            "default",
            &workflow_id,
        )
        .await
        .expect("controlled workflow read")
        .expect("controlled workflow record");
    assert_eq!(
        controlled.requested_outcome,
        Some(crate::trigger::collection_workflow::CollectionRequestedOutcome::Cancelled)
    );
    assert_eq!(controlled_sequence, 2);
    let source_events = store
        .read_events("default:Batch:batch-1", 0)
        .await
        .expect("source events");
    assert_eq!(source_events.len(), 4);
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn normal_dispatch_rejects_new_starts_in_non_enabled_modes() {
    use crate::trigger::collection_workflow::CollectionWorkflowMode;
    use temper_runtime::tenant::TenantId;
    for (mode, code) in [
        (
            CollectionWorkflowMode::Draining,
            "CollectionWorkflowDraining",
        ),
        (
            CollectionWorkflowMode::Disabled,
            "CollectionWorkflowDisabled",
        ),
    ] {
        let mut state = collection_dispatch_state();
        state.collection_workflow_mode = mode;
        let error = state
            .dispatch_tenant_action(
                &TenantId::default(),
                "Batch",
                "batch-mode",
                "Start",
                serde_json::json!({}),
                &AgentContext::for_service("collection-test"),
            )
            .await
            .expect_err("non-enabled start should fail");
        assert!(error.contains(code), "unexpected error: {error}");
    }
}

#[test]
fn wasm_authz_gate_evaluates_cedar_when_policy_set_is_empty() {
    let state = test_state();
    state
        .authz
        .reload_tenant_policies("test-tenant", "")
        .expect("empty policy set should parse");
    let gate = state.wasm_authz_gate();
    let decision = gate.authorize_http_call(
        "api.example.com",
        "GET",
        "https://api.example.com/v1/ping",
        &WasmAuthzContext::test_fixture(),
    );
    assert_eq!(
        decision,
        WasmAuthzDecision::Deny("no matching permit policy".to_string())
    );
}

#[test]
fn wasm_authz_gate_allows_when_cedar_policy_matches() {
    let state = test_state();
    state
        .authz
        .reload_tenant_policies(
            "test-tenant",
            r#"
            permit(
                principal is Agent,
                action == Action::"http_call",
                resource is HttpEndpoint
            ) when {
                context.module == "stripe_charge"
            };
            "#,
        )
        .expect("policy should parse");
    let gate = state.wasm_authz_gate();
    let decision = gate.authorize_http_call(
        "api.stripe.com",
        "POST",
        "https://api.stripe.com/v1/charges",
        &WasmAuthzContext::test_fixture(),
    );
    assert_eq!(decision, WasmAuthzDecision::Allow);
}
