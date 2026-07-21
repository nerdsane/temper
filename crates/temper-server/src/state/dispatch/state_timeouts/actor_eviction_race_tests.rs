//! Actor eviction publication-fence regressions.

use std::sync::Arc;
use std::time::Duration;

use temper_runtime::ActorSystem;
use temper_runtime::actor::{Actor, ActorContext, ActorError};
use temper_runtime::scheduler::install_deterministic_context;
use temper_runtime::tenant::TenantId;
use temper_spec::csdl::parse_csdl;

use crate::entity_actor::{EntityActor, EntityMsg, EntityResponse};
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
async fn eviction_keeps_replacement_publication_fenced_through_index_cleanup() {
    let (_guard, _clock, _ids) = install_deterministic_context(245);
    let tenant = TenantId::default();
    let entity_id = "eviction-fences-replacement";
    let actor_key = format!("{tenant}:Ticket:{entity_id}");
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        tenant.as_str(),
        parse_csdl(TICKET_CSDL).expect("ticket CSDL parses"),
        TICKET_CSDL.to_string(),
        &[("Ticket", TIMED_TICKET_IOA)],
    );
    let server = ServerState::from_registry(ActorSystem::new("eviction-fence-race"), registry);
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
            "eviction-fence-source",
            EntityMsg::GetState,
        )
        .expect("source actor starts");
    let response = source_state
        .receive()
        .await
        .expect("source state is readable");
    let source_guard = source_ref
        .stop_and_wait()
        .await
        .expect("source actor stops");
    drop(source_guard);

    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let actor_ref = server.actor_system.spawn(
        DelayedStateActor {
            response,
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
        },
        "eviction-fence-target",
    );
    let actor_uid = actor_ref.id().uid;
    server
        .actor_registry
        .write()
        .expect("actor registry lock")
        .insert(actor_key.clone(), actor_ref.clone());
    server
        .entity_index
        .write()
        .expect("entity index lock")
        .entry(format!("{tenant}:Ticket"))
        .or_default()
        .insert(entity_id.to_string());

    let mut state_read =
        Box::pin(actor_ref.ask::<EntityResponse>(EntityMsg::GetState, Duration::from_secs(1)));
    let entered_wait = entered.notified();
    tokio::pin!(entered_wait);
    loop {
        tokio::select! {
            biased;
            () = &mut entered_wait => break,
            result = &mut state_read => panic!("state read completed before its release: {result:?}"),
            () = tokio::task::yield_now() => {}
        }
    }

    let mut eviction =
        Box::pin(server.stop_and_remove_entity_if_current(&tenant, "Ticket", entity_id, actor_uid));
    tokio::select! {
        biased;
        removed = &mut eviction => panic!("eviction bypassed the in-flight handler: {removed}"),
        () = tokio::task::yield_now() => {}
    }
    assert!(
        actor_ref.is_draining(),
        "eviction closes actor admission first"
    );
    assert!(
        server
            .get_or_spawn_tenant_actor(&tenant, "Ticket", entity_id)
            .is_none(),
        "a replacement cannot publish while the old incarnation is draining"
    );
    assert!(
        server.entity_exists(&tenant, "Ticket", entity_id),
        "index cleanup follows receiver closure under the same publication fence"
    );

    release.notify_one();
    let read = state_read.await.expect("the admitted state read completes");
    assert_eq!(read.state.status, "Open");
    assert!(eviction.await, "the captured incarnation is removed");
    assert!(
        !server
            .actor_registry
            .read()
            .expect("actor registry lock")
            .contains_key(&actor_key)
    );
    assert!(!server.entity_exists(&tenant, "Ticket", entity_id));
}

#[tokio::test(start_paused = true)]
async fn memory_only_reconciliation_cannot_resurrect_synchronously_removed_entity() {
    let (_guard, _clock, _ids) = install_deterministic_context(246);
    let tenant = TenantId::default();
    let entity_id = "synchronous-remove-compatibility";
    let actor_key = format!("{tenant}:Ticket:{entity_id}");
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        tenant.as_str(),
        parse_csdl(TICKET_CSDL).expect("ticket CSDL parses"),
        TICKET_CSDL.to_string(),
        &[("Ticket", TIMED_TICKET_IOA)],
    );
    let server =
        ServerState::from_registry(ActorSystem::new("sync-remove-compatibility"), registry);
    let actor_ref = server
        .get_or_spawn_tenant_actor(&tenant, "Ticket", entity_id)
        .expect("spawn compatibility actor");
    assert!(server.entity_exists(&tenant, "Ticket", entity_id));
    assert!(
        server
            .state_timeout_tracker
            .begin_registry_reconciliation()
            .is_none(),
        "the no-store timeout reconciliation worker must own the initial timed sweep"
    );

    server.remove_entity(&tenant, "Ticket", entity_id);

    assert!(actor_ref.is_drain_fenced());
    for _ in 0..64 {
        if !server
            .actor_registry
            .read()
            .expect("actor registry lock")
            .contains_key(&actor_key)
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(
        !server
            .actor_registry
            .read()
            .expect("actor registry lock")
            .contains_key(&actor_key)
    );
    assert!(!server.entity_exists(&tenant, "Ticket", entity_id));

    // Cross the reconciler's first retry window. A stale scan must neither
    // restore the index nor publish a replacement actor or timeout owner.
    tokio::time::advance(Duration::from_secs(1)).await;
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
    assert!(
        !server
            .actor_registry
            .read()
            .expect("actor registry lock")
            .contains_key(&actor_key)
    );
    assert!(!server.entity_exists(&tenant, "Ticket", entity_id));
    assert!(server.state_timeout_tracker.pending_snapshot().is_empty());
}

#[tokio::test(start_paused = true)]
async fn stale_memory_only_scan_cannot_cross_index_only_removal_fence() {
    let (_guard, _clock, _ids) = install_deterministic_context(257);
    let tenant = TenantId::default();
    let entity_id = "stale-index-only-removal";
    let actor_key = format!("{tenant}:Ticket:{entity_id}");
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        tenant.as_str(),
        parse_csdl(TICKET_CSDL).expect("ticket CSDL parses"),
        TICKET_CSDL.to_string(),
        &[("Ticket", TIMED_TICKET_IOA)],
    );
    let server = ServerState::from_registry(ActorSystem::new("stale-index-only"), registry);
    server
        .entity_index
        .write()
        .expect("entity index lock")
        .entry(format!("{tenant}:Ticket"))
        .or_default()
        .insert(entity_id.to_string());
    assert!(
        !server
            .actor_registry
            .read()
            .expect("actor registry lock")
            .contains_key(&actor_key),
        "the memory-only entity begins passivated and index-only"
    );

    let (scan_captured, release_scan) = server
        .state_timeout_tracker
        .pause_next_registry_entity_scan();
    server.ensure_registry_timeout_reconciliation_started();
    scan_captured
        .await
        .expect("reconciler captures the stale index snapshot");

    // Removal's no-actor decision and index deletion share the publication
    // fence. The paused stale scan can resume only after absence is committed.
    server.remove_entity(&tenant, "Ticket", entity_id);
    assert!(!server.entity_exists(&tenant, "Ticket", entity_id));
    release_scan
        .send(())
        .expect("release the stale reconciliation scan");
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_secs(1)).await;
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }

    assert!(
        !server
            .actor_registry
            .read()
            .expect("actor registry lock")
            .contains_key(&actor_key)
    );
    assert!(!server.entity_exists(&tenant, "Ticket", entity_id));
    assert!(server.state_timeout_tracker.pending_snapshot().is_empty());
}

#[tokio::test(start_paused = true)]
async fn synchronous_remove_preserves_live_registration_when_stop_mailbox_is_full() {
    let (_guard, _clock, _ids) = install_deterministic_context(256);
    let tenant = TenantId::default();
    let entity_id = "synchronous-remove-full-mailbox";
    let actor_key = format!("{tenant}:Ticket:{entity_id}");
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        tenant.as_str(),
        parse_csdl(TICKET_CSDL).expect("ticket CSDL parses"),
        TICKET_CSDL.to_string(),
        &[("Ticket", TIMED_TICKET_IOA)],
    );
    let server = ServerState::from_registry(ActorSystem::new("sync-remove-full-mailbox"), registry);
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
            "sync-remove-full-mailbox-source",
            EntityMsg::GetState,
        )
        .expect("source actor starts");
    let response = source_state
        .receive()
        .await
        .expect("source state is readable");
    drop(
        source_ref
            .stop_and_wait()
            .await
            .expect("source actor stops"),
    );

    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let actor_ref = server.actor_system.spawn(
        DelayedStateActor {
            response,
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
        },
        "sync-remove-full-mailbox-target",
    );
    let actor_uid = actor_ref.id().uid;
    server
        .actor_registry
        .write()
        .expect("actor registry lock")
        .insert(actor_key.clone(), actor_ref.clone());
    server
        .entity_index
        .write()
        .expect("entity index lock")
        .entry(format!("{tenant}:Ticket"))
        .or_default()
        .insert(entity_id.to_string());

    let mut blocked_read =
        Box::pin(actor_ref.ask::<EntityResponse>(EntityMsg::GetState, Duration::from_secs(1)));
    let entered_wait = entered.notified();
    tokio::pin!(entered_wait);
    tokio::select! {
        biased;
        () = &mut entered_wait => {}
        result = &mut blocked_read => panic!("state read completed before its release: {result:?}"),
    }
    for _ in 0..actor_ref.mailbox_capacity() {
        actor_ref
            .tell(EntityMsg::GetState)
            .expect("fill the bounded mailbox exactly");
    }
    assert!(matches!(
        actor_ref.tell(EntityMsg::GetState),
        Err(ActorError::MailboxFull)
    ));

    server.remove_entity(&tenant, "Ticket", entity_id);

    assert!(
        !actor_ref.is_drain_fenced(),
        "a rejected stop barrier must restore mailbox admission"
    );
    assert!(
        server
            .actor_registry
            .read()
            .expect("actor registry lock")
            .get(&actor_key)
            .is_some_and(|current| current.id().uid == actor_uid),
        "failed stop admission must not unregister the live actor"
    );
    assert!(
        server.entity_exists(&tenant, "Ticket", entity_id),
        "failed stop admission must not erase the entity index"
    );

    release.notify_one();
    let _ = blocked_read.await.expect("the admitted read completes");
}
