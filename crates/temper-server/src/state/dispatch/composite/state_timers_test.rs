//! A composite sub-write keeps state timers as its own dispatch would.
//!
//! A sub-write that enters a timed state arms that state's timer, and one
//! listed in the state's `reset_on` moves the deadline, whether the batch is
//! written atomically or one write at a time. The timer fires as the caller
//! whose action set the composite going. Before the fix, the atomic batch
//! appended the sub-write's event and armed nothing, so a record moved into a
//! timed state by a composite stayed there. Time is Tokio's paused clock, so a
//! wait costs no wall time.
use std::time::Duration;

use temper_authz::{PrincipalKind, SecurityContext};

use super::*;
use crate::entity_actor::EntityState;
use crate::registry::SpecRegistry;

#[path = "timer_recovery_test.rs"]
mod recovery;

const SEED: u64 = 23;
const ITEM: &str = "item-1";

const ITEM_CSDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.CompositeTimerTest" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Item">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="moved_by" Type="Edm.String"/>
        <Property Name="ticked_by" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Items" EntityType="Temper.CompositeTimerTest.Item"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

/// `Decide` is a composite on the item itself whose result moves the item.
/// `Moved` times out after three seconds into `Tick`, which records who it
/// ran as; `Nudge` restarts that clock. `MoveAndSchedule` is `Move` with a
/// scheduled action, which the atomic batch cannot stage, so it takes the
/// per-write path.
const ITEM_IOA: &str = r#"
[automaton]
name = "Item"
states = ["Waiting", "Moved", "Ticked"]
initial = "Waiting"
allow_indefinite_states = ["Waiting", "Ticked"]
strict_action_params = true

[[state]]
name = "moved_by"
type = "string"
initial = ""

[[state]]
name = "ticked_by"
type = "string"
initial = ""

[[action]]
name = "Decide"
kind = "composite"
from = ["Waiting", "Moved"]
params = []

[[action.sub_writes]]
target_entity = "Item"
action = "Move"
generated_from = "decision"

[[action.sub_writes]]
target_entity = "Item"
action = "MoveAndSchedule"
generated_from = "decision"

[[action.sub_writes]]
target_entity = "Item"
action = "Nudge"
generated_from = "decision"

[[action.sub_writes]]
target_entity = "Item"
action = "Note"
generated_from = "decision"

[[action]]
name = "Note"
from = ["Moved"]
params = [{ name = "moved_by", type = "string" }]

[[action]]
name = "Move"
from = ["Waiting"]
to = "Moved"
params = [{ name = "moved_by", type = "string", source = "authenticated_subject" }]

[[action]]
name = "MoveAndSchedule"
from = ["Waiting"]
to = "Moved"
params = [{ name = "moved_by", type = "string", source = "authenticated_subject" }]
effect = ["schedule('Linger', 3600)"]

[[action]]
name = "Linger"
from = ["Moved", "Ticked"]
params = []

[[action]]
name = "Nudge"
from = ["Moved"]
params = [{ name = "moved_by", type = "string", source = "authenticated_subject" }]

[[action]]
name = "Tick"
from = ["Moved"]
to = "Ticked"
params = [{ name = "ticked_by", type = "string", source = "authenticated_subject" }, { name = "moved_by", type = "string" }]

[[state_timeout]]
state = "Moved"
after_seconds = 3
on_timeout = "Tick"
reset_on = ["Nudge"]
args = { moved_by = "moved_by" }
"#;

/// How a transition reaches the journal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Path {
    /// The caller's own dispatch, with no composite: the control.
    Direct,
    /// A composite whose sub-writes are appended in one journal batch.
    Atomic,
    /// A composite dispatched one write at a time, because a sub-write starts
    /// a scheduled action.
    FallbackForEffects,
    /// A composite dispatched one write at a time, because the server keeps
    /// no event journal.
    FallbackWithoutJournal,
}

fn item_state(path: Path) -> (ServerState, Option<SimEventStore>) {
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        parse_csdl(ITEM_CSDL).expect("test CSDL should parse"),
        ITEM_CSDL.to_string(),
        &[("Item", ITEM_IOA)],
    );
    let mut state = ServerState::from_registry(ActorSystem::new("composite-timers"), registry);
    let store = (path != Path::FallbackWithoutJournal).then(|| SimEventStore::no_faults(SEED));
    if let Some(store) = &store {
        state.set_storage_stack(StorageStack::from_sim(store.clone(), None));
    }
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .expect("test policy should parse");
    (state, store)
}

/// A signed-in person.
fn person(id: &str) -> AgentContext {
    AgentContext {
        security_ctx: Some(SecurityContext::from_verified_jwt(
            id,
            PrincipalKind::Customer,
            None,
            None,
            None,
            None,
        )),
        agent_id: Some(id.to_string()),
        ..Default::default()
    }
}

/// Apply `action` to the item as `caller`: dispatched directly, or as the one
/// sub-write of a `Decide` integration result.
async fn transition(state: &ServerState, path: Path, action: &str, caller: &AgentContext) {
    let tenant = TenantId::default();
    if path == Path::Direct {
        let response = state
            .dispatch_tenant_action(&tenant, "Item", ITEM, action, json!({}), caller)
            .await
            .expect("direct dispatch should run");
        assert!(response.success, "{path:?}: {action} refused: {response:?}");
        return;
    }
    let result = json!({"sub_writes": [{
        "entity_type": "Item", "entity_id": ITEM, "action": action, "params": {}
    }]});
    let applied = state
        .apply_composite_integration_result(&tenant, "Item", ITEM, "Decide", &result, caller)
        .await
        .expect("composite result should apply");
    assert!(applied, "{path:?}: composite result not applied");
}

async fn item(state: &ServerState) -> EntityState {
    state
        .get_tenant_entity_state(&TenantId::default(), "Item", ITEM)
        .await
        .expect("item should be readable")
        .state
}

/// The item once it leaves `Moved`, or after `within`.
async fn settle(state: &ServerState, within: Duration) -> EntityState {
    let step = Duration::from_millis(250);
    let mut waited = Duration::ZERO;
    loop {
        let current = item(state).await;
        if current.status != "Moved" || waited >= within {
            return current;
        }
        tokio::time::sleep(step).await;
        waited += step;
    }
}

/// The path the composite took, read from the item's journal: only the atomic
/// batch records the parent's `CompositeEvent`.
fn assert_took(path: Path, store: Option<&SimEventStore>) {
    let Some(store) = store else {
        return;
    };
    let journal = store.dump_journal(&format!("default:Item:{ITEM}"));
    let atomic = journal
        .iter()
        .any(|event| event.event_type == COMPOSITE_EVENT_TYPE);
    match path {
        Path::Atomic => assert!(
            atomic,
            "{path:?}: the composite did not take the atomic batch"
        ),
        Path::FallbackForEffects => {
            assert!(
                !atomic,
                "{path:?}: the composite did not take the per-write path"
            )
        }
        Path::Direct | Path::FallbackWithoutJournal => {}
    }
}

#[tokio::test(start_paused = true)]
async fn composite_sub_write_into_a_timed_state_fires_its_timer_as_the_caller() {
    // The atomic batch runs last, so a failure there shows the paths that
    // dispatch one write at a time already held.
    for path in [
        Path::Direct,
        Path::FallbackForEffects,
        Path::FallbackWithoutJournal,
        Path::Atomic,
    ] {
        let (state, store) = item_state(path);
        let tenant = TenantId::default();
        let opened = state
            .dispatch_tenant_action(
                &tenant,
                "Item",
                ITEM,
                "Decide",
                json!({}),
                &AgentContext::system(),
            )
            .await
            .expect("the parent transition should run");
        assert!(opened.success, "{path:?}: {opened:?}");

        let alice = person("alice");
        let action = match path {
            Path::FallbackForEffects => "MoveAndSchedule",
            _ => "Move",
        };
        transition(&state, path, action, &alice).await;
        assert_took(path, store.as_ref());
        let moved = item(&state).await;
        assert_eq!(moved.status, "Moved", "{path:?}");
        assert_eq!(
            moved.fields["moved_by"], "alice",
            "{path:?}: the sub-write acts as its caller"
        );

        let after = settle(&state, Duration::from_secs(10)).await;
        assert_eq!(
            after.status, "Ticked",
            "{path:?}: the timer of the state the transition entered never fired"
        );
        assert_eq!(
            after.fields["ticked_by"], "alice",
            "{path:?}: the timer fires as the caller whose action entered the state"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn composite_sub_write_reset_moves_the_timer_deadline() {
    for path in [Path::Direct, Path::FallbackWithoutJournal, Path::Atomic] {
        let (state, store) = item_state(path);
        let tenant = TenantId::default();
        let moved = state
            .dispatch_tenant_action(&tenant, "Item", ITEM, "Move", json!({}), &person("bob"))
            .await
            .expect("bob's move should run");
        assert!(moved.success, "{path:?}: {moved:?}");

        // Two seconds into bob's three, alice's transition restarts the clock.
        tokio::time::sleep(Duration::from_secs(2)).await;
        transition(&state, path, "Nudge", &person("alice")).await;
        assert_took(path, store.as_ref());

        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(
            item(&state).await.status,
            "Moved",
            "{path:?}: the deadline from before the reset expired the item"
        );

        let after = settle(&state, Duration::from_secs(10)).await;
        assert_eq!(
            after.status, "Ticked",
            "{path:?}: the reset timer never fired"
        );
        assert_eq!(
            after.fields["ticked_by"], "alice",
            "{path:?}: the reset timer fires as the caller who reset it"
        );
    }
}
