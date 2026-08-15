//! Trigger graph analysis for composite verification (ADR-0046 slice 8).
//!
//! Walks a set of parsed [`Automaton`]s and builds a directed graph whose
//! nodes are entity types and whose edges are `kind = "entity"` triggers —
//! each edge represents "when the source entity's action X commits, the
//! target entity's action Y fires with a specific resolver".
//!
//! The graph is the input to a future [`CompositeTemperModel`] in
//! `temper-verify`: the verifier composes exactly the entities reachable
//! from a seed entity via this graph, avoiding unnecessary state-space
//! explosion for unrelated entities.
//!
//! Only `kind = "entity"` triggers appear here. `kind = "wasm"` and
//! `kind = "webhook"` triggers are opaque to joint verification (their
//! execution is an I/O side-effect, not a state transition on another
//! entity); their `on_success`/`on_failure` dispatches do show up as edges
//! from the source entity to itself if the follow-up is a declared action.
//!
//! Cycles are permitted (a reaction cascade can feed back) and detected;
//! the verifier bounds cycle exploration by `MAX_TRIGGER_DEPTH = 8`.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use super::types::{ActionTrigger, Automaton, TargetResolver, TriggerKind};

/// Directed edge: `from` entity's `source_action` triggers `to` entity's
/// `target_action` when it fires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriggerEdge {
    /// Source entity type name.
    pub from: String,
    /// Action on the source that fires the trigger.
    pub source_action: String,
    /// Trigger declaration name (for debugging + liveness-property
    /// identification).
    pub trigger_name: String,
    /// Target entity type name.
    pub to: String,
    /// Action dispatched on the target entity.
    pub target_action: String,
    /// Optional `to_state` filter carried from the spec. When `Some`, the
    /// edge only fires if the source action transitions to this state.
    pub to_state: Option<String>,
    /// Whether the spec asks for the composite verifier to emit a
    /// `Property::eventually` for this chain.
    pub liveness_required: bool,
    /// Whether the trigger resolves its target via `type = "create"`, which
    /// spawns a fresh (always-enabled) target instance (ADR-0150). Such a
    /// reaction can never be dropped, so the composite verifier's
    /// `no_dropped_reaction` property exempts it.
    pub creates_target: bool,
    /// Static target parameters carried by this reaction.
    pub params: serde_json::Value,
    /// Target parameter to source-field projections carried by this reaction.
    pub params_from: BTreeMap<String, String>,
    /// Whether the spec marked this reaction as an intentional best-effort
    /// drop (`drop_ok = true`, ADR-0150). When `true`, the composite
    /// verifier suppresses the `no_dropped_reaction` violation for this edge.
    pub drop_ok: bool,
}

/// Graph of entity-kind triggers across a set of automatons.
#[derive(Debug, Clone, Default)]
pub struct TriggerGraph {
    /// All declared entity types (keys) and their full automaton
    /// references. Populated from the input set so the caller can retrieve
    /// metadata (states, invariants) for downstream verification.
    pub entities: BTreeSet<String>,
    /// Outgoing edges, keyed by source entity type.
    pub outgoing: BTreeMap<String, Vec<TriggerEdge>>,
    /// Incoming edges, keyed by target entity type. Useful for "who
    /// dispatches me?" queries.
    pub incoming: BTreeMap<String, Vec<TriggerEdge>>,
}

impl TriggerGraph {
    /// Build a [`TriggerGraph`] from a slice of parsed automatons.
    ///
    /// Iterates each entity's actions; for each action's `[[action.triggers]]`
    /// block with `kind = "entity"`, emits one edge. `kind = "wasm"` /
    /// `kind = "webhook"` triggers are skipped (not part of joint
    /// verification of entity state machines).
    pub fn from_automatons(automatons: &[&Automaton]) -> Self {
        let mut graph = TriggerGraph::default();
        for aut in automatons {
            let entity_type = aut.automaton.name.clone();
            graph.entities.insert(entity_type.clone());
            for action in &aut.actions {
                for trigger in &action.triggers {
                    if let Some(edge) = edge_from_trigger(&entity_type, &action.name, trigger) {
                        graph
                            .outgoing
                            .entry(edge.from.clone())
                            .or_default()
                            .push(edge.clone());
                        graph
                            .incoming
                            .entry(edge.to.clone())
                            .or_default()
                            .push(edge);
                    }
                }
            }
        }
        graph
    }

    /// Compute the reachability set from a seed entity, following entity-kind
    /// trigger edges. Used by the composite verifier to scope the set of
    /// entities it must compose — only entities reachable from the seed
    /// participate in the product state space.
    ///
    /// Includes the seed itself. Handles cycles via a visited set.
    pub fn reachable_from(&self, seed: &str) -> BTreeSet<String> {
        let mut visited: BTreeSet<String> = BTreeSet::new();
        let mut queue: VecDeque<String> = VecDeque::new();

        if !self.entities.contains(seed) {
            return visited;
        }
        queue.push_back(seed.to_string());

        while let Some(entity) = queue.pop_front() {
            if !visited.insert(entity.clone()) {
                continue;
            }
            if let Some(edges) = self.outgoing.get(&entity) {
                for edge in edges {
                    if !visited.contains(&edge.to) {
                        queue.push_back(edge.to.clone());
                    }
                }
            }
        }
        visited
    }

    /// Returns `true` if the graph contains any cycle reachable from `seed`.
    ///
    /// Cycles are not errors (trigger cascades can legally feed back) but
    /// callers like the composite verifier may want to know so they can
    /// enforce `MAX_TRIGGER_DEPTH` vs compose a DAG differently.
    pub fn has_cycle_from(&self, seed: &str) -> bool {
        let mut visited: BTreeSet<String> = BTreeSet::new();
        let mut in_stack: BTreeSet<String> = BTreeSet::new();
        self.dfs_cycle_check(seed, &mut visited, &mut in_stack)
    }

    fn dfs_cycle_check(
        &self,
        node: &str,
        visited: &mut BTreeSet<String>,
        in_stack: &mut BTreeSet<String>,
    ) -> bool {
        if in_stack.contains(node) {
            return true;
        }
        if !visited.insert(node.to_string()) {
            return false;
        }
        in_stack.insert(node.to_string());
        if let Some(edges) = self.outgoing.get(node) {
            for edge in edges {
                if self.dfs_cycle_check(&edge.to, visited, in_stack) {
                    return true;
                }
            }
        }
        in_stack.remove(node);
        false
    }
}

fn edge_from_trigger(
    source_entity: &str,
    source_action: &str,
    trigger: &ActionTrigger,
) -> Option<TriggerEdge> {
    if trigger.kind != TriggerKind::Entity {
        return None;
    }
    let target_entity = trigger.target_entity.as_ref()?.clone();
    let target_action = trigger.target_action.as_ref()?.clone();
    let creates_target = matches!(trigger.resolve_target, Some(TargetResolver::Create));
    Some(TriggerEdge {
        from: source_entity.to_string(),
        source_action: source_action.to_string(),
        trigger_name: trigger.name.clone(),
        to: target_entity,
        target_action,
        to_state: trigger.to_state.clone(),
        liveness_required: matches!(trigger.liveness, super::types::TriggerLiveness::Required),
        creates_target,
        params: trigger.params.clone(),
        params_from: trigger.params_from.clone(),
        drop_ok: trigger.drop_ok,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automaton::parser::parse_automaton;

    fn order_ioa() -> &'static str {
        r#"
[automaton]
name = "Order"
states = ["Draft", "Submitted", "Confirmed"]
initial = "Draft"

[[action]]
name = "SubmitOrder"
from = ["Draft"]
to = "Submitted"

[[action]]
name = "ConfirmOrder"
from = ["Submitted"]
to = "Confirmed"

[[action.triggers]]
name = "confirm_triggers_auth"
kind = "entity"
principal = "payment-service"
target_entity = "Payment"
target_action = "AuthorizePayment"

[action.triggers.resolve_target]
type = "field"
field = "payment_id"
"#
    }

    fn payment_ioa() -> &'static str {
        r#"
[automaton]
name = "Payment"
states = ["Pending", "Authorized"]
initial = "Pending"

[[action]]
name = "AuthorizePayment"
from = ["Pending"]
to = "Authorized"
"#
    }

    fn wiki_ioa() -> &'static str {
        // No triggers — isolated entity.
        r#"
[automaton]
name = "Wiki"
states = ["Draft", "Published"]
initial = "Draft"

[[action]]
name = "Publish"
from = ["Draft"]
to = "Published"
"#
    }

    #[test]
    fn builds_edge_from_entity_trigger() {
        let order = parse_automaton(order_ioa()).unwrap();
        let payment = parse_automaton(payment_ioa()).unwrap();
        let graph = TriggerGraph::from_automatons(&[&order, &payment]);

        assert!(graph.entities.contains("Order"));
        assert!(graph.entities.contains("Payment"));

        let outgoing = graph.outgoing.get("Order").expect("Order has outgoing");
        assert_eq!(outgoing.len(), 1);
        let e = &outgoing[0];
        assert_eq!(e.from, "Order");
        assert_eq!(e.to, "Payment");
        assert_eq!(e.source_action, "ConfirmOrder");
        assert_eq!(e.target_action, "AuthorizePayment");
        assert_eq!(e.trigger_name, "confirm_triggers_auth");

        let incoming = graph.incoming.get("Payment").expect("Payment has incoming");
        assert_eq!(incoming.len(), 1);
    }

    #[test]
    fn reachability_from_seed_collects_chain() {
        let order = parse_automaton(order_ioa()).unwrap();
        let payment = parse_automaton(payment_ioa()).unwrap();
        let wiki = parse_automaton(wiki_ioa()).unwrap();
        let graph = TriggerGraph::from_automatons(&[&order, &payment, &wiki]);

        let reach = graph.reachable_from("Order");
        assert!(reach.contains("Order"));
        assert!(reach.contains("Payment"));
        assert!(!reach.contains("Wiki"), "Wiki isolated from Order's chain");

        let wiki_reach = graph.reachable_from("Wiki");
        assert_eq!(wiki_reach.len(), 1);
        assert!(wiki_reach.contains("Wiki"));
    }

    #[test]
    fn reachable_from_unknown_is_empty() {
        let order = parse_automaton(order_ioa()).unwrap();
        let graph = TriggerGraph::from_automatons(&[&order]);
        assert!(graph.reachable_from("Missing").is_empty());
    }

    #[test]
    fn no_cycle_in_linear_chain() {
        let order = parse_automaton(order_ioa()).unwrap();
        let payment = parse_automaton(payment_ioa()).unwrap();
        let graph = TriggerGraph::from_automatons(&[&order, &payment]);
        assert!(!graph.has_cycle_from("Order"));
    }

    #[test]
    fn detects_self_trigger_cycle() {
        // Entity triggers itself (e.g., generic Agent.Assign fires Start on
        // the same Agent) — a bounded self-loop, which is a cycle in the
        // graph sense.
        let spec = r#"
[automaton]
name = "Agent"
states = ["Idle", "Assigned", "Working"]
initial = "Idle"

[[action]]
name = "Assign"
from = ["Idle"]
to = "Assigned"

[[action.triggers]]
name = "auto_start"
kind = "entity"
principal = "agent-supervisor"
target_entity = "Agent"
target_action = "Start"
to_state = "Assigned"

[action.triggers.resolve_target]
type = "same_id"

[[action]]
name = "Start"
from = ["Assigned"]
to = "Working"
"#;
        let agent = parse_automaton(spec).expect("agent should parse");
        let graph = TriggerGraph::from_automatons(&[&agent]);
        assert!(graph.has_cycle_from("Agent"), "self-trigger is a cycle");
    }

    #[test]
    fn wasm_and_webhook_triggers_skipped() {
        let spec = r#"
[automaton]
name = "Order"
states = ["Draft", "Confirmed", "Notified"]
initial = "Draft"

[[action]]
name = "ConfirmOrder"
from = ["Draft"]
to = "Confirmed"

[[action.triggers]]
name = "charge_wasm"
kind = "wasm"
module = "stripe_charge"
on_success = "NotifyUser"

[[action.triggers]]
name = "notify_webhook"
kind = "webhook"
url = "https://example.com"
method = "POST"

[[action]]
name = "NotifyUser"
from = ["Confirmed"]
to = "Notified"
"#;
        let order = parse_automaton(spec).unwrap();
        let graph = TriggerGraph::from_automatons(&[&order]);
        // wasm + webhook triggers contribute no edges; Order has no
        // entity-kind triggers here, so outgoing should be absent.
        assert!(!graph.outgoing.contains_key("Order"));
    }

    #[test]
    fn liveness_required_propagates_to_edge() {
        let spec = r#"
[automaton]
name = "X"
states = ["A", "B"]
initial = "A"

[[action]]
name = "Go"
from = ["A"]
to = "B"

[[action.triggers]]
name = "must_fire"
kind = "entity"
liveness = "required"
target_entity = "Y"
target_action = "Do"

[action.triggers.resolve_target]
type = "same_id"
"#;
        let x = parse_automaton(spec).unwrap();
        let graph = TriggerGraph::from_automatons(&[&x]);
        let edge = &graph.outgoing.get("X").unwrap()[0];
        assert!(edge.liveness_required);
    }
}
