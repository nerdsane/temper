//! Live/restart authority parity for timeout-triggered reactions.

use super::*;
use temper_server::request_context::AgentContext;

const TIMEOUT_AUTHORITY_CSDL_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.TimeoutAuthority" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="TimeoutSource">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
      </EntityType>
      <EntityType Name="TimeoutTarget">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="TimeoutSources" EntityType="Temper.TimeoutAuthority.TimeoutSource"/>
        <EntitySet Name="TimeoutTargets" EntityType="Temper.TimeoutAuthority.TimeoutTarget"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

const TIMEOUT_SOURCE_IOA: &str = r#"
[automaton]
name = "TimeoutSource"
states = ["Idle", "Waiting", "Expired"]
initial = "Idle"
allow_indefinite_states = ["Idle", "Expired"]

[[action]]
name = "Start"
kind = "input"
from = ["Idle"]
to = "Waiting"

[[action]]
name = "TimeoutFail"
kind = "internal"
from = ["Waiting"]
to = "Expired"

[[action.triggers]]
name = "timeout_marks_target"
kind = "entity"
target_entity = "TimeoutTarget"
target_action = "Mark"

[action.triggers.resolve_target]
type = "same_id"

[[state_timeout]]
state = "Waiting"
after_seconds = 60
on_timeout = "TimeoutFail"
"#;

const TIMEOUT_TARGET_IOA: &str = r#"
[automaton]
name = "TimeoutTarget"
states = ["Pending", "Marked"]
initial = "Pending"
allow_indefinite_states = ["Pending", "Marked"]

[[action]]
name = "Mark"
kind = "internal"
from = ["Pending"]
to = "Marked"
"#;

const TIMEOUT_AUTHORITY_POLICY: &str = r#"
permit(
    principal is Agent,
    action == Action::"Mark",
    resource is TimeoutTarget
) when {
    principal.agent_type == "state-timeout-hydration"
};
"#;

fn timeout_authority_server(store: SimEventStore, system_name: &str) -> ServerState {
    let csdl = parse_csdl(TIMEOUT_AUTHORITY_CSDL_XML).expect("authority CSDL parse");
    let mut registry = SpecRegistry::new();
    registry
        .try_register_tenant_with_reactions(
            "default",
            csdl,
            TIMEOUT_AUTHORITY_CSDL_XML.to_string(),
            &[
                ("TimeoutSource", TIMEOUT_SOURCE_IOA),
                ("TimeoutTarget", TIMEOUT_TARGET_IOA),
            ],
            Vec::new(),
        )
        .expect("register timeout authority specs");

    let mut state = ServerState::from_registry(ActorSystem::new(system_name), registry);
    state.set_storage_stack(StorageStack::from_sim(store, None));
    state
        .authz
        .reload_tenant_policies("default", TIMEOUT_AUTHORITY_POLICY)
        .expect("load timeout authority policy");
    state.rebuild_reaction_dispatcher();
    state
}

async fn seed_timeout_authority_entities(
    store: &SimEventStore,
    entity_id: &str,
    entered_waiting_at: chrono::DateTime<chrono::Utc>,
) {
    let target_pid = format!("default:TimeoutTarget:{entity_id}");
    let source_pid = format!("default:TimeoutSource:{entity_id}");
    let target_created = EntityEvent {
        action: "Created".to_string(),
        from_status: String::new(),
        to_status: "Pending".to_string(),
        timestamp: entered_waiting_at,
        params: serde_json::json!({"Id": entity_id}),
        idempotency_key: None,
    };
    store
        .append(
            &target_pid,
            0,
            &[persisted_event(&target_pid, 1, target_created)],
        )
        .await
        .expect("seed the timeout reaction target");

    let source_created = EntityEvent {
        action: "Created".to_string(),
        from_status: String::new(),
        to_status: "Idle".to_string(),
        timestamp: entered_waiting_at,
        params: serde_json::json!({"Id": entity_id}),
        idempotency_key: None,
    };
    let source_started = EntityEvent {
        action: "Start".to_string(),
        from_status: "Idle".to_string(),
        to_status: "Waiting".to_string(),
        timestamp: entered_waiting_at,
        params: serde_json::json!({}),
        idempotency_key: None,
    };
    store
        .append(
            &source_pid,
            0,
            &[
                persisted_event(&source_pid, 1, source_created),
                persisted_event(&source_pid, 2, source_started),
            ],
        )
        .await
        .expect("seed the timed reaction source");
}

async fn status(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
) -> String {
    state
        .get_tenant_entity_state(tenant, entity_type, entity_id)
        .await
        .unwrap_or_else(|error| panic!("read {entity_type}:{entity_id}: {error}"))
        .state
        .status
}

#[tokio::test(start_paused = true)]
async fn timeout_reaction_authority_is_identical_live_and_after_restart() {
    let (_guard, _clock, _ids) = install_deterministic_context(222);
    let tenant = TenantId::default();
    let entity_id = "timeout-authority-parity";
    let entered_waiting_at = sim_now();

    let live_store = SimEventStore::no_faults(222);
    let live_state = timeout_authority_server(live_store, "timeout-authority-live");
    let live_target = live_state
        .get_or_create_tenant_entity(&tenant, "TimeoutTarget", entity_id, serde_json::json!({}))
        .await
        .expect("create the live reaction target");
    assert_eq!(live_target.state.status, "Pending");
    let live_source = live_state
        .get_or_create_tenant_entity(&tenant, "TimeoutSource", entity_id, serde_json::json!({}))
        .await
        .expect("create the live timeout source");
    assert_eq!(live_source.state.status, "Idle");
    let initiating_caller = AgentContext::for_service("initiating-caller");
    let started = live_state
        .dispatch_tenant_action(
            &tenant,
            "TimeoutSource",
            entity_id,
            "Start",
            serde_json::json!({}),
            &initiating_caller,
        )
        .await
        .expect("enter the live timed state");
    assert_eq!(started.state.status, "Waiting");

    let restarted_store = SimEventStore::no_faults(223);
    seed_timeout_authority_entities(&restarted_store, entity_id, entered_waiting_at).await;
    let restarted_state = timeout_authority_server(restarted_store, "timeout-authority-restarted");
    restarted_state.populate_index_from_store(&tenant).await;

    for _ in 0..128 {
        if live_state.state_timeout_tracker.pending_snapshot()
            == vec![("TimeoutSource".to_string(), 1)]
            && restarted_state.state_timeout_tracker.pending_snapshot()
                == vec![("TimeoutSource".to_string(), 1)]
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(
        live_state.state_timeout_tracker.pending_snapshot(),
        vec![("TimeoutSource".to_string(), 1)]
    );
    assert_eq!(
        restarted_state.state_timeout_tracker.pending_snapshot(),
        vec![("TimeoutSource".to_string(), 1)]
    );

    tokio::time::advance(std::time::Duration::from_secs(60)).await;
    let mut live_source_status = String::new();
    let mut restarted_source_status = String::new();
    let mut live_target_status = String::new();
    let mut restarted_target_status = String::new();
    for _ in 0..128 {
        tokio::task::yield_now().await;
        live_source_status = status(&live_state, &tenant, "TimeoutSource", entity_id).await;
        restarted_source_status =
            status(&restarted_state, &tenant, "TimeoutSource", entity_id).await;
        live_target_status = status(&live_state, &tenant, "TimeoutTarget", entity_id).await;
        restarted_target_status =
            status(&restarted_state, &tenant, "TimeoutTarget", entity_id).await;
        if live_target_status == "Marked" && restarted_target_status == "Marked" {
            break;
        }
    }
    assert_eq!(live_source_status, "Expired");
    assert_eq!(restarted_source_status, "Expired");
    assert_eq!(
        (
            live_target_status.as_str(),
            restarted_target_status.as_str()
        ),
        ("Marked", "Marked"),
        "timeout-triggered reactions must use the same service authority live and after restart"
    );
}
