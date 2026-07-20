//! Synthetic-commit versus delayed-hydration timeout ownership races.

use std::sync::Arc;
use std::time::Duration;

use temper_runtime::ActorSystem;
use temper_runtime::actor::{Actor, ActorContext, ActorError};
use temper_runtime::scheduler::{install_deterministic_context, sim_now};
use temper_runtime::tenant::TenantId;
use temper_spec::csdl::parse_csdl;

use crate::entity_actor::{EntityActor, EntityEvent, EntityMsg, EntityResponse};
use crate::registry::SpecRegistry;
use crate::state::ServerState;

const TICKET_CSDL: &str = include_str!("../../../../../../test-fixtures/specs/model.csdl.xml");

const TIMED_TICKET_IOA: &str = r#"
[automaton]
name = "Ticket"
states = ["Open", "Closed", "TimedOut"]
initial = "Open"
allow_indefinite_states = ["Closed", "TimedOut"]

[[action]]
name = "Close"
kind = "input"
from = ["Open"]
to = "Closed"

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

struct DelayedStateActor {
    response: EntityResponse,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    replied: Arc<tokio::sync::Notify>,
}

impl Actor for DelayedStateActor {
    type Msg = EntityMsg;
    type State = ();

    async fn pre_start(&self, _ctx: &mut ActorContext<Self>) -> Result<Self::State, ActorError> {
        Ok(())
    }

    async fn handle(
        &self,
        msg: Self::Msg,
        _state: &mut Self::State,
        ctx: &mut ActorContext<Self>,
    ) -> Result<(), ActorError> {
        match msg {
            EntityMsg::GetState => {
                self.entered.notify_one();
                self.release.notified().await;
                ctx.reply(self.response.clone());
                self.replied.notify_one();
                Ok(())
            }
            _ => Err(ActorError::custom(
                "delayed-state test actor only accepts GetState",
            )),
        }
    }

    async fn post_stop(&self, _state: Self::State, _ctx: &mut ActorContext<Self>) {}
}

#[tokio::test(start_paused = true)]
async fn evicted_actor_hydration_cannot_arm_after_a_synthetic_exit() {
    let (_guard, _clock, _ids) = install_deterministic_context(239);
    let tenant = TenantId::default();
    let entity_id = "synthetic-exit-before-hydration";
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        tenant.as_str(),
        parse_csdl(TICKET_CSDL).expect("ticket CSDL parses"),
        TICKET_CSDL.to_string(),
        &[("Ticket", TIMED_TICKET_IOA)],
    );
    let server =
        ServerState::from_registry(ActorSystem::new("synthetic-exit-hydration-race"), registry);

    let table = server
        .registry
        .read()
        .expect("registry lock")
        .get_table_live(&tenant, "Ticket")
        .expect("ticket table is registered");
    let actor_key = format!("{tenant}:Ticket:{entity_id}");
    let actor = EntityActor::new(
        "Ticket",
        entity_id,
        table,
        serde_json::json!({"Id": entity_id}),
    )
    .with_tenant(tenant.as_str());
    let (actor_ref, startup_state) = server
        .actor_system
        .spawn_with_first_ask::<_, EntityResponse>(actor, &actor_key, EntityMsg::GetState)
        .expect("actor starts with a lifecycle-coupled state read");
    let stale_actor_uid = actor_ref.id().uid;
    server
        .actor_registry
        .write()
        .expect("actor registry lock")
        .insert(actor_key, actor_ref);
    let stale_hydration = startup_state
        .receive()
        .await
        .expect("startup state is readable");
    assert_eq!(stale_hydration.state.status, "Open");

    let committed_at = sim_now();
    let mut committed_exit = stale_hydration.state.clone();
    committed_exit.status = "Closed".to_string();
    committed_exit.fields["Status"] = serde_json::json!("Closed");
    committed_exit.events.push_back(EntityEvent {
        action: "Close".to_string(),
        from_status: "Open".to_string(),
        to_status: "Closed".to_string(),
        timestamp: committed_at,
        params: serde_json::json!({}),
        idempotency_key: None,
    });
    committed_exit.total_event_count = 1;
    committed_exit.events_since_snapshot = 1;
    committed_exit.sequence_nr = 1;
    committed_exit.state_timeout_clock_reset_at = None;
    committed_exit.state_timeout_clock_reset_version = None;

    server
        .drain_and_remove_entity(&tenant, "Ticket", entity_id)
        .await;
    let inactive_timeout_fence = server.reconcile_state_timeout_after_synthetic_commit(
        &tenant,
        "Ticket",
        entity_id,
        &committed_exit,
    );
    assert_eq!(
        server.state_timeout_tracker.size(),
        1,
        "an untimed synthetic commit must fence delayed snapshots even without a prior owner"
    );

    server.arm_state_timeouts_on_current_actor_hydration(
        &tenant,
        "Ticket",
        entity_id,
        stale_actor_uid,
        &stale_hydration,
        super::StateTimeoutHydrationTiming {
            observed_at: committed_at,
            readiness_elapsed: Duration::ZERO,
        },
    );

    assert!(
        server.state_timeout_tracker.pending_snapshot().is_empty(),
        "a delayed pre-commit hydration snapshot must not arm after the newer synthetic exit"
    );
    assert_eq!(
        server.state_timeout_tracker.size(),
        1,
        "the synthetic event-order fence remains until eviction cleanup"
    );

    let agent_ctx = crate::request_context::AgentContext::for_service("stale-post-dispatch-test");
    let params = serde_json::json!({});
    let ctx = super::PostDispatchContext {
        tenant: &tenant,
        entity_type: "Ticket",
        entity_id,
        action: "Created",
        agent_ctx: &agent_ctx,
        dispatch_idempotency_key: None,
        action_params: &params,
        await_integration: false,
        actor_uid: Some(stale_actor_uid),
    };
    server.arm_state_timeouts_if_needed(&ctx, &stale_hydration);
    assert_eq!(
        server.state_timeout_tracker.size(),
        1,
        "post-dispatch work from the evicted incarnation must be fenced too"
    );
    server.release_inactive_state_timeout_after_actor_eviction(
        &tenant,
        "Ticket",
        entity_id,
        inactive_timeout_fence,
    );
    assert_eq!(server.state_timeout_tracker.size(), 0);
}

#[tokio::test(start_paused = true)]
async fn stale_actor_cleanup_cannot_evict_a_replacement_incarnation() {
    let (_guard, _clock, _ids) = install_deterministic_context(242);
    let tenant = TenantId::default();
    let entity_id = "replacement-survives-stale-cleanup";
    let actor_key = format!("{tenant}:Ticket:{entity_id}");
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        tenant.as_str(),
        parse_csdl(TICKET_CSDL).expect("ticket CSDL parses"),
        TICKET_CSDL.to_string(),
        &[("Ticket", TIMED_TICKET_IOA)],
    );
    let server = ServerState::from_registry(ActorSystem::new("stale-cleanup-race"), registry);
    let table = server
        .registry
        .read()
        .expect("registry lock")
        .get_table_live(&tenant, "Ticket")
        .expect("ticket table is registered");
    let stale_actor = EntityActor::new(
        "Ticket",
        entity_id,
        table.clone(),
        serde_json::json!({"Id": entity_id}),
    )
    .with_tenant(tenant.as_str());
    let replacement_actor = EntityActor::new(
        "Ticket",
        entity_id,
        table,
        serde_json::json!({"Id": entity_id}),
    )
    .with_tenant(tenant.as_str());
    let stale_ref = server.actor_system.spawn(stale_actor, "stale-incarnation");
    let replacement_ref = server
        .actor_system
        .spawn(replacement_actor, "replacement-incarnation");
    let stale_uid = stale_ref.id().uid;
    let replacement_uid = replacement_ref.id().uid;
    server
        .actor_registry
        .write()
        .expect("actor registry lock")
        .insert(actor_key.clone(), replacement_ref);
    server
        .entity_index
        .write()
        .expect("entity index lock")
        .entry(format!("{tenant}:Ticket"))
        .or_default()
        .insert(entity_id.to_string());

    assert!(
        !server
            .stop_and_remove_entity_if_current(&tenant, "Ticket", entity_id, stale_uid)
            .await,
        "a result from the stale actor must not clean up by key"
    );
    assert!(
        server
            .actor_registry
            .read()
            .expect("actor registry lock")
            .get(&actor_key)
            .is_some_and(|current| current.id().uid == replacement_uid),
        "the replacement incarnation must remain registered"
    );
    assert!(server.entity_exists(&tenant, "Ticket", entity_id));
    let _ = stale_ref.stop();
}

#[tokio::test(start_paused = true)]
async fn stale_table_reconciliation_cannot_rearm_after_a_synthetic_exit() {
    let (_guard, _clock, _ids) = install_deterministic_context(244);
    let tenant = TenantId::default();
    let entity_id = "table-reconcile-before-synthetic-exit";
    let actor_key = format!("{tenant}:Ticket:{entity_id}");
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        tenant.as_str(),
        parse_csdl(TICKET_CSDL).expect("ticket CSDL parses"),
        TICKET_CSDL.to_string(),
        &[("Ticket", TIMED_TICKET_IOA)],
    );
    let server = ServerState::from_registry(ActorSystem::new("stale-table-reconcile"), registry);
    let table = server
        .registry
        .read()
        .expect("registry lock")
        .get_table_live(&tenant, "Ticket")
        .expect("ticket table is registered");

    let source_actor = EntityActor::new(
        "Ticket",
        entity_id,
        table,
        serde_json::json!({"Id": entity_id}),
    )
    .with_tenant(tenant.as_str());
    let (source_ref, source_state) = server
        .actor_system
        .spawn_with_first_ask::<_, EntityResponse>(
            source_actor,
            "table-reconcile-source",
            EntityMsg::GetState,
        )
        .expect("source actor starts");
    let stale_response = source_state
        .receive()
        .await
        .expect("source timed state is readable");
    let _drain_guard = source_ref
        .stop_and_wait()
        .await
        .expect("source actor stops");
    assert_eq!(stale_response.state.status, "Open");

    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let replied = Arc::new(tokio::sync::Notify::new());
    let delayed_ref = server.actor_system.spawn(
        DelayedStateActor {
            response: stale_response.clone(),
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
            replied: Arc::clone(&replied),
        },
        "delayed-table-reconcile",
    );
    server
        .actor_registry
        .write()
        .expect("actor registry lock")
        .insert(actor_key, delayed_ref);

    let agent_ctx = crate::request_context::AgentContext::for_service("table-reconcile-race");
    let reconciliation =
        server.reconcile_state_timeout_after_table_change(&tenant, "Ticket", entity_id, &agent_ctx);
    tokio::pin!(reconciliation);
    let entered_wait = entered.notified();
    tokio::pin!(entered_wait);
    loop {
        tokio::select! {
            biased;
            () = &mut entered_wait => break,
            result = &mut reconciliation => panic!("reconciliation completed before the controlled reply: {result:?}"),
            () = tokio::task::yield_now() => {}
        }
    }

    release.notify_one();
    replied.notified().await;
    server
        .drain_and_remove_entity(&tenant, "Ticket", entity_id)
        .await;
    let mut committed_exit = stale_response.state.clone();
    committed_exit.status = "Closed".to_string();
    committed_exit.fields["Status"] = serde_json::json!("Closed");
    committed_exit.sequence_nr = committed_exit.sequence_nr.saturating_add(1);
    committed_exit.total_event_count = committed_exit.total_event_count.saturating_add(1);
    committed_exit.events_since_snapshot = committed_exit.events_since_snapshot.saturating_add(1);
    committed_exit.state_timeout_clock_reset_at = None;
    committed_exit.state_timeout_clock_reset_version = None;
    let inactive_timeout_fence = server.reconcile_state_timeout_after_synthetic_commit(
        &tenant,
        "Ticket",
        entity_id,
        &committed_exit,
    );
    assert_eq!(
        server.state_timeout_tracker.size(),
        1,
        "the newer synthetic exit must retain an event-order fence"
    );

    reconciliation
        .await
        .expect("the delayed state read itself succeeded");
    assert!(
        server.state_timeout_tracker.pending_snapshot().is_empty(),
        "an evicted actor's stale table snapshot must not resurrect timeout ownership"
    );
    assert_eq!(server.state_timeout_tracker.size(), 1);
    server.release_inactive_state_timeout_after_actor_eviction(
        &tenant,
        "Ticket",
        entity_id,
        inactive_timeout_fence,
    );
    assert_eq!(server.state_timeout_tracker.size(), 0);
}
