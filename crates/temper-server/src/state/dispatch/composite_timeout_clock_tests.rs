use std::collections::BTreeMap;

use serde_json::json;
use temper_runtime::ActorSystem;
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;

use crate::request_context::AgentContext;
use crate::state::ServerState;
use crate::storage::StorageStack;

use super::*;

const CSDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.CompositeTimeoutTest" xmlns="http://docs.oasis.org/odata/ns/edm">
      <EntityType Name="Parent">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityType Name="TimedChild">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Parents" EntityType="Temper.CompositeTimeoutTest.Parent"/>
        <EntitySet Name="TimedChildren" EntityType="Temper.CompositeTimeoutTest.TimedChild"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

const PARENT_IOA: &str = r#"
[automaton]
name = "Parent"
states = ["Active"]
initial = "Active"

[[action]]
name = "CreateTimedChild"
kind = "Composite"
from = ["Active"]
to = "Active"
record_parent_event = false

[[action.sub_writes]]
target_entity = "TimedChild"
action = "Create"
generated_from = "timed_child"

[[action]]
name = "HeartbeatTimedChild"
kind = "Composite"
from = ["Active"]
to = "Active"
record_parent_event = false

[[action.sub_writes]]
target_entity = "TimedChild"
action = "Heartbeat"
generated_from = "timed_child"
"#;

const TIMED_CHILD_WITH_RESET_IOA: &str = r#"
[automaton]
name = "TimedChild"
states = ["Open", "TimedOut"]
initial = "Open"
allow_indefinite_states = ["TimedOut"]

[[action]]
name = "Create"
kind = "input"
from = ["Open"]
to = "Open"

[[action]]
name = "Heartbeat"
kind = "input"
from = ["Open"]
to = "Open"

[[action]]
name = "TimeoutFail"
kind = "internal"
from = ["Open"]
to = "TimedOut"

[[state_timeout]]
state = "Open"
after_seconds = 60
on_timeout = "TimeoutFail"
reset_on = ["Heartbeat"]
"#;

const TIMED_CHILD_WITHOUT_RESET_IOA: &str = r#"
[automaton]
name = "TimedChild"
states = ["Open", "TimedOut"]
initial = "Open"
allow_indefinite_states = ["TimedOut"]

[[action]]
name = "Create"
kind = "input"
from = ["Open"]
to = "Open"

[[action]]
name = "Heartbeat"
kind = "input"
from = ["Open"]
to = "Open"

[[action]]
name = "TimeoutFail"
kind = "internal"
from = ["Open"]
to = "TimedOut"

[[state_timeout]]
state = "Open"
after_seconds = 60
on_timeout = "TimeoutFail"
"#;

fn state_with_timed_child(
    store: SimEventStore,
    timed_child_ioa: &str,
    system_name: &str,
) -> ServerState {
    let csdl = parse_csdl(CSDL).expect("composite timeout CSDL parses");
    let specs = BTreeMap::from([
        ("Parent".to_string(), PARENT_IOA.to_string()),
        ("TimedChild".to_string(), timed_child_ioa.to_string()),
    ]);
    ServerState::with_storage_stack(
        ActorSystem::new(system_name),
        csdl,
        CSDL.to_string(),
        specs,
        StorageStack::from_sim(store, None),
    )
    .expect("composite timeout state builds")
}

async fn run_restart_case(
    seed: u64,
    commit_ioa: &str,
    restart_ioa: &str,
    heartbeat_resets_at_commit: bool,
) {
    let (_guard, clock, _ids) = temper_runtime::scheduler::install_deterministic_context(seed);
    let store = SimEventStore::no_faults(seed);
    let tenant = TenantId::default();
    let entity_id = format!("timed-composite-{seed}");
    let persistence_id = format!("default:TimedChild:{entity_id}");
    let state = state_with_timed_child(
        store.clone(),
        commit_ioa,
        "composite-timeout-before-restart",
    );
    state
        .get_or_create_tenant_entity(&tenant, "TimedChild", &entity_id, json!({}))
        .await
        .expect("create timed composite target");
    let entered = state
        .get_tenant_entity_state(&tenant, "TimedChild", &entity_id)
        .await
        .expect("read timed target entry");
    let entered_at = entered
        .state
        .state_timeout_clock_reset_at
        .expect("Created establishes the timed-state clock");

    tokio::time::advance(std::time::Duration::from_secs(30)).await;
    clock.advance_by(300);
    state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            &format!("parent-{seed}"),
            "HeartbeatTimedChild",
            &json!({
                "sub_writes": [{
                    "entity_type": "TimedChild",
                    "entity_id": entity_id,
                    "action": "Heartbeat",
                    "params": {}
                }]
            }),
            &AgentContext::for_service("composite-timeout-clock-test"),
        )
        .await
        .expect("atomic composite Heartbeat commits");
    assert!(
        state
            .actor_registry
            .read()
            .expect("actor registry lock")
            .contains_key(&persistence_id),
        "successful existing-target composite writes must reload the actor before returning"
    );
    let after_heartbeat = state
        .get_tenant_entity_state(&tenant, "TimedChild", &entity_id)
        .await
        .expect("read composite-updated target");
    let committed_anchor = after_heartbeat
        .state
        .state_timeout_clock_reset_at
        .expect("timed target retains an anchor");
    let expected_version = if heartbeat_resets_at_commit { 2 } else { 1 };
    if heartbeat_resets_at_commit {
        assert!(committed_anchor > entered_at);
    } else {
        assert_eq!(committed_anchor, entered_at);
    }
    assert_eq!(
        after_heartbeat.state.state_timeout_clock_reset_version,
        Some(expected_version)
    );

    let journal = store.dump_journal(&persistence_id);
    assert_eq!(
        journal
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        vec!["Created", "Heartbeat"]
    );
    assert!(
        journal[1]
            .payload
            .get("__temper_state_timeout_clock")
            .is_some(),
        "atomic composite events use the shared current payload encoder"
    );

    state
        .state_timeout_tracker
        .forget(&tenant, "TimedChild", &entity_id);
    state
        .drain_and_remove_entity(&tenant, "TimedChild", &entity_id)
        .await;
    drop(state);

    let restarted = state_with_timed_child(
        store.clone(),
        restart_ioa,
        "composite-timeout-after-restart",
    );
    restarted.populate_index_from_store(&tenant).await;
    let hydrated = restarted
        .get_tenant_entity_state(&tenant, "TimedChild", &entity_id)
        .await
        .expect("hydrate composite target after declaration change");
    assert_eq!(
        (
            hydrated.state.state_timeout_clock_reset_at,
            hydrated.state.state_timeout_clock_reset_version,
        ),
        (Some(committed_anchor), Some(expected_version))
    );

    tokio::time::advance(std::time::Duration::from_secs(30)).await;
    clock.advance_by(300);
    for _ in 0..128 {
        tokio::task::yield_now().await;
        if restarted
            .get_tenant_entity_state(&tenant, "TimedChild", &entity_id)
            .await
            .expect("read first deadline segment")
            .state
            .status
            == "TimedOut"
        {
            break;
        }
    }
    let first_deadline_status = restarted
        .get_tenant_entity_state(&tenant, "TimedChild", &entity_id)
        .await
        .expect("read first deadline status")
        .state
        .status;

    if heartbeat_resets_at_commit {
        assert_eq!(
            first_deadline_status, "Open",
            "removing reset_on must preserve the later composite reset"
        );
        tokio::time::advance(std::time::Duration::from_secs(30)).await;
        clock.advance_by(300);
        for _ in 0..128 {
            tokio::task::yield_now().await;
            if restarted
                .get_tenant_entity_state(&tenant, "TimedChild", &entity_id)
                .await
                .expect("read committed composite deadline")
                .state
                .status
                == "TimedOut"
            {
                break;
            }
        }
    } else {
        assert_eq!(
            first_deadline_status, "TimedOut",
            "adding reset_on must not reinterpret the composite Heartbeat"
        );
    }

    assert_eq!(
        restarted
            .get_tenant_entity_state(&tenant, "TimedChild", &entity_id)
            .await
            .expect("read final timed target")
            .state
            .status,
        "TimedOut"
    );
    let final_journal = store.dump_journal(&persistence_id);
    assert_eq!(
        final_journal
            .iter()
            .filter(|event| event.event_type == "TimeoutFail")
            .count(),
        1,
        "restart delivers exactly one timeout"
    );
    assert_eq!(final_journal.len(), 3, "Created, Heartbeat, TimeoutFail");
}

#[tokio::test(start_paused = true)]
async fn atomic_composite_creation_arms_timed_target_without_later_access() {
    let seed = 233;
    let (_guard, clock, _ids) = temper_runtime::scheduler::install_deterministic_context(seed);
    let store = SimEventStore::no_faults(seed);
    let tenant = TenantId::default();
    let entity_id = "timed-composite-created-atomically";
    let persistence_id = format!("default:TimedChild:{entity_id}");
    let state = state_with_timed_child(
        store.clone(),
        TIMED_CHILD_WITH_RESET_IOA,
        "composite-timeout-new-target",
    );

    state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "parent-creates-timed-child",
            "CreateTimedChild",
            &json!({
                "sub_writes": [{
                    "entity_type": "TimedChild",
                    "entity_id": entity_id,
                    "action": "Create",
                    "params": {}
                }]
            }),
            &AgentContext::for_service("composite-timeout-new-target-test"),
        )
        .await
        .expect("atomic composite creates the timed target");

    assert_eq!(
        state.state_timeout_tracker.pending_snapshot(),
        vec![("TimedChild".to_string(), 1)],
        "the atomic commit must arm the new timed target without a later read"
    );

    tokio::time::advance(std::time::Duration::from_secs(60)).await;
    clock.advance_by(600);
    for _ in 0..128 {
        tokio::task::yield_now().await;
        if store
            .dump_journal(&persistence_id)
            .iter()
            .any(|event| event.event_type == "TimeoutFail")
        {
            break;
        }
    }

    let journal = store.dump_journal(&persistence_id);
    assert_eq!(
        journal
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        vec!["Created", "Create", "TimeoutFail"],
        "the new target must time out without intervening entity access"
    );

    tokio::time::advance(std::time::Duration::from_secs(60)).await;
    clock.advance_by(600);
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        store
            .dump_journal(&persistence_id)
            .iter()
            .filter(|event| event.event_type == "TimeoutFail")
            .count(),
        1,
        "the synthetic creation path must deliver the timeout exactly once"
    );
}

#[tokio::test(start_paused = true)]
async fn composite_reset_on_removal_preserves_clock_across_abrupt_restart() {
    run_restart_case(
        231,
        TIMED_CHILD_WITH_RESET_IOA,
        TIMED_CHILD_WITHOUT_RESET_IOA,
        true,
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn composite_reset_on_addition_does_not_reinterpret_clock_after_abrupt_restart() {
    run_restart_case(
        232,
        TIMED_CHILD_WITHOUT_RESET_IOA,
        TIMED_CHILD_WITH_RESET_IOA,
        false,
    )
    .await;
}

#[path = "composite_timeout_clock_projection_fault_tests.rs"]
mod projection_fault_tests;
