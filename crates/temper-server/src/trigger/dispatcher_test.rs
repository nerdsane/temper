//! A reaction chain stops the first time it would repeat a step, and only
//! then: the stopped reaction is a failed result and an observe event on the
//! entity whose action would have fired it, and a chain that never repeats
//! runs to its end however long it is.

use super::*;
use crate::registry::SpecRegistry;
use temper_runtime::ActorSystem;
use temper_spec::csdl::parse_csdl;

const CSDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.ReactionChain" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Ticker">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
      </EntityType>
      <EntityType Name="Node">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
        <Property Name="next_id" Type="Edm.String"/>
      </EntityType>
      <EntityType Name="Seed">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
      </EntityType>
      <EntityType Name="Cell">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
        <Property Name="child_id" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Tickers" EntityType="Temper.ReactionChain.Ticker"/>
        <EntitySet Name="Nodes" EntityType="Temper.ReactionChain.Node"/>
        <EntitySet Name="Seeds" EntityType="Temper.ReactionChain.Seed"/>
        <EntitySet Name="Cells" EntityType="Temper.ReactionChain.Cell"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

/// `Tick` reacts to itself: a loop nothing else ends.
const TICKER_IOA: &str = r#"
[automaton]
name = "Ticker"
states = ["On"]
initial = "On"

[[state]]
name = "ticks"
type = "counter"
initial = 0

[[action]]
name = "Tick"
kind = "input"
from = ["On"]
effect = ["ticks += 1"]

[[action.triggers]]
name = "tick_again"
kind = "entity"
target_entity = "Ticker"
target_action = "Tick"
resolve_target = { kind = "same_id" }
"#;

/// `Pass` hands on to the node `next_id` names: the same action, a different
/// entity at every step, so a list of nodes is a chain that never repeats.
const NODE_IOA: &str = r#"
[automaton]
name = "Node"
states = ["Ready"]
initial = "Ready"

[[state]]
name = "passes"
type = "counter"
initial = 0

[[state]]
name = "next_id"
type = "string"
initial = ""

[[action]]
name = "Pass"
kind = "input"
from = ["Ready"]
effect = ["passes += 1"]

[[action.triggers]]
name = "pass_on"
kind = "entity"
target_entity = "Node"
target_action = "Pass"

[action.triggers.resolve_target]
kind = "field"
id_field = "next_id"
"#;

/// `Sprout` makes a new Seed that sprouts in turn: a loop whose every lap is
/// a new entity, so no id ever repeats.
const SEED_IOA: &str = r#"
[automaton]
name = "Seed"
states = ["Sown"]
initial = "Sown"

[[action]]
name = "Sprout"
kind = "input"
from = ["Sown"]

[[action.triggers]]
name = "sprout_again"
kind = "entity"
target_entity = "Seed"
target_action = "Sprout"
resolve_target = { kind = "create" }
"#;

/// `Divide` spawns a new Cell that divides in turn: a loop through spawns,
/// with no reaction in it.
const CELL_IOA: &str = r#"
[automaton]
name = "Cell"
states = ["Alive"]
initial = "Alive"

[[state]]
name = "child_id"
type = "string"
initial = ""

[[action]]
name = "Divide"
kind = "input"
from = ["Alive"]
effect = ["spawn('Cell', 'Divide', child_id)"]
"#;

fn chain_state() -> crate::ServerState {
    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(CSDL).expect("CSDL parses");
    registry.register_tenant(
        "default",
        csdl,
        CSDL.to_string(),
        &[
            ("Ticker", TICKER_IOA),
            ("Node", NODE_IOA),
            ("Seed", SEED_IOA),
            ("Cell", CELL_IOA),
        ],
    );
    let state = crate::ServerState::from_registry(ActorSystem::new("reaction-chain"), registry);
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .expect("policy loads");
    state.rebuild_reaction_dispatcher();
    state
}

fn stops(state: &crate::ServerState, entity_type: &str, entity_id: &str) -> Vec<serde_json::Value> {
    state
        .entity_observe_replay(TenantId::default().as_str(), entity_type, entity_id, 0)
        .events
        .into_iter()
        .filter(|event| event.event_name == "reaction_cycle_stopped")
        .map(|event| event.data)
        .collect()
}

async fn run(state: &crate::ServerState, entity_type: &str, entity_id: &str, action: &str) {
    run_as(
        state,
        entity_type,
        entity_id,
        action,
        &AgentContext::for_service("reaction-chain-test"),
    )
    .await;
}

async fn run_as(
    state: &crate::ServerState,
    entity_type: &str,
    entity_id: &str,
    action: &str,
    agent_ctx: &AgentContext,
) {
    let response = state
        .dispatch_tenant_action(
            &TenantId::default(),
            entity_type,
            entity_id,
            action,
            serde_json::json!({}),
            agent_ctx,
        )
        .await
        .expect("dispatch");
    assert!(response.success, "{action}: {:?}", response.error);
}

/// Nodes `n0..n{count}`, each naming the next as `next_id`.
async fn seed_nodes(state: &crate::ServerState, count: usize) {
    for index in 0..count {
        let fields = if index + 1 < count {
            serde_json::json!({ "next_id": format!("n{}", index + 1) })
        } else {
            serde_json::json!({})
        };
        state
            .get_or_create_tenant_entity(&TenantId::default(), "Node", &format!("n{index}"), fields)
            .await
            .expect("seed node");
    }
}

/// The old bound let `Tick` run itself eight more times before stopping it.
#[tokio::test]
async fn a_reaction_that_repeats_its_own_step_is_stopped_and_named() {
    let state = chain_state();
    run(&state, "Ticker", "t1", "Tick").await;

    let ticker = state
        .get_tenant_entity_state(&TenantId::default(), "Ticker", "t1")
        .await
        .expect("Ticker state")
        .state;
    assert_eq!(ticker.counters.get("ticks").copied(), Some(1));

    let stopped = stops(&state, "Ticker", "t1");
    assert_eq!(stopped.len(), 1, "the stopped reaction is reported once");
    // A spec trigger's rule is named `{entity}:{action}:{trigger}`.
    assert_eq!(stopped[0]["rule"], "Ticker:Tick:tick_again");
    assert_eq!(stopped[0]["target_entity_id"], "t1");
    assert_eq!(
        stopped[0]["cycle"],
        serde_json::json!(["Ticker(t1).Tick", "Ticker(t1).Tick"])
    );
}

/// The old bound stopped this chain at its ninth node.
#[tokio::test]
async fn a_chain_that_never_repeats_runs_to_its_end() {
    const NODES: usize = 200;
    let state = chain_state();
    let tenant = TenantId::default();
    seed_nodes(&state, NODES).await;

    run(&state, "Node", "n0", "Pass").await;

    for index in 0..NODES {
        let node = state
            .get_tenant_entity_state(&tenant, "Node", &format!("n{index}"))
            .await
            .expect("Node state")
            .state;
        assert_eq!(
            node.counters.get("passes").copied(),
            Some(1),
            "node n{index} did not pass"
        );
        assert!(stops(&state, "Node", &format!("n{index}")).is_empty());
    }
}

/// A loop that makes a new entity every lap never repeats an id; the made
/// entity is known by type and action, so the second lap is the repeat. The
/// old bound let it make eight Seeds.
#[tokio::test]
async fn a_loop_that_makes_an_entity_each_lap_stops_on_its_second_lap() {
    let state = chain_state();
    let tenant = TenantId::default();
    run(&state, "Seed", "s1", "Sprout").await;

    let mut seeds = state.list_entity_ids(&tenant, "Seed");
    seeds.sort();
    assert_eq!(seeds.len(), 2, "s1 and the one Seed it made: {seeds:?}");
    let made = seeds
        .iter()
        .find(|id| id.as_str() != "s1")
        .expect("the made Seed");
    assert!(stops(&state, "Seed", "s1").is_empty());
    let stopped = stops(&state, "Seed", made);
    assert_eq!(stopped.len(), 1, "{stopped:?}");
    assert_eq!(
        stopped[0]["cycle"],
        serde_json::json!(["Seed(new).Sprout", "Seed(new).Sprout"])
    );
}

/// A spawned child's initial action is a step on an entity the chain made, so
/// a Cell whose child divides in turn stops at the second division, on the
/// Cell whose spawn would repeat it. Before, Cells were made for as long as
/// the server ran.
#[tokio::test]
async fn a_spawn_loop_stops_at_its_first_repeat() {
    let state = chain_state();
    let tenant = TenantId::default();
    run(&state, "Cell", "c0", "Divide").await;
    let child = state
        .get_tenant_entity_state(&tenant, "Cell", "c0")
        .await
        .expect("Cell c0")
        .state
        .fields["child_id"]
        .as_str()
        .expect("c0 names its child")
        .to_string();

    let mut refused = Vec::new();
    for _ in 0..250 {
        refused = state
            .entity_observe_replay(tenant.as_str(), "Cell", &child, 0)
            .events
            .into_iter()
            .filter(|event| event.event_name == "integration_callback_rejected")
            .map(|event| event.data)
            .collect();
        if !refused.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(refused.len(), 1, "the child's own spawn is refused once");
    assert_eq!(refused[0]["action"], "Divide");
    let error = refused[0]["error"].as_str().expect("refusal says why");
    assert!(
        error.contains("Cell(new).Divide -> Cell(new).Divide"),
        "{error}"
    );
    let cells = state.list_entity_ids(&tenant, "Cell");
    assert_eq!(cells.len(), 2, "c0 and the one Cell it made: {cells:?}");
}

/// The first eight reactions of a chain keep the keys a server stopping chains
/// at eight reactions wrote, so a retry across the upgrade finds them done.
/// Past them a key stays one length: a key that grew by a suffix at every
/// reaction would make the keys a chain stores cost the square of its length.
#[tokio::test]
async fn reaction_keys_match_earlier_ones_then_stop_growing() {
    const NODES: usize = 40;
    let state = chain_state();
    seed_nodes(&state, NODES).await;
    let caller = AgentContext {
        idempotency_key: Some(crate::idempotency::IdempotencyKey::Request(
            "caller-key".to_string(),
        )),
        ..AgentContext::for_service("reaction-chain-test")
    };
    run_as(&state, "Node", "n0", "Pass", &caller).await;

    let mut keys = Vec::new();
    for index in 1..NODES {
        let node = state
            .get_tenant_entity_state(&TenantId::default(), "Node", &format!("n{index}"))
            .await
            .expect("Node state")
            .state;
        let pass = node
            .events
            .iter()
            .rev()
            .find(|event| event.action == "Pass")
            .unwrap_or_else(|| panic!("node n{index} did not pass"));
        keys.push(pass.idempotency_key.clone().expect("a reaction key"));
    }
    let mut earlier = "caller-key".to_string();
    for key in &keys[..8] {
        earlier = format!("{earlier}:reaction:Node:Pass:pass_on");
        assert_eq!(key, &earlier);
    }
    assert!(
        keys[8..].iter().all(|key| key.len() == keys[8].len()),
        "{keys:?}"
    );
    let distinct: std::collections::BTreeSet<_> = keys.iter().collect();
    assert_eq!(distinct.len(), keys.len(), "each reaction has its own key");
}

/// A reaction stopped as a repeat is a failed result, not a missing one.
#[tokio::test]
async fn a_stopped_reaction_is_a_failed_result() {
    let state = chain_state();
    let tenant = TenantId::default();
    let dispatcher = state
        .reaction_dispatcher
        .read()
        .expect("dispatcher lock")
        .clone()
        .expect("dispatcher built");
    let results = dispatcher
        .dispatch_reactions(
            &state,
            &tenant,
            "Ticker",
            "t1",
            "Tick",
            "On",
            &serde_json::json!({}),
            0,
            &AgentContext::for_service("reaction-chain-test"),
        )
        .await;
    assert_eq!(results.len(), 1, "{results:?}");
    assert!(!results[0].success);
    assert_eq!(results[0].rule_name, "Ticker:Tick:tick_again");
    assert!(
        results[0]
            .error
            .as_deref()
            .is_some_and(|error| error.contains("Ticker(t1).Tick -> Ticker(t1).Tick")),
        "{results:?}"
    );
}
