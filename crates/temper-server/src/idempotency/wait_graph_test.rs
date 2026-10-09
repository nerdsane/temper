use super::*;
use crate::idempotency::{CompletionResult, OperationOwner, OperationRequest};
use crate::request_context::AgentContext;

fn operation(graph: &Arc<WaitGraph>, name: &str) -> OperationOwner {
    OperationOwner::new(OperationReceipt::new(
        OperationRequest {
            tenant: temper_runtime::tenant::TenantId::default(),
            entity_type: name.into(),
            entity_id: "one".into(),
            action: "Start".into(),
            params: serde_json::json!({}),
            agent_ctx: AgentContext::system(),
            idempotency_key: Some("K".into()),
            await_integration: true,
            reaction_depth: Some(0),
            detach_reactions: false,
        },
        super::super::tests::make_response("Running"),
        graph.clone(),
    ))
}
fn ready(owner: &OperationOwner) {
    owner.publish_effects(owner.receipt.committed.clone());
    owner.start_reactions();
}
fn edge(graph: &Arc<WaitGraph>, source: &OperationOwner, target: &OperationOwner) -> WaitEdge {
    match graph.register(&source.receipt, &target.receipt, true) {
        Registration::Observe(edge) => edge,
        _ => panic!("expected a real dependency, not a cycle shortcut"),
    }
}

#[test]
fn cancelled_observers_and_old_tickets_cannot_manufacture_cycles() {
    let graph = Arc::new(WaitGraph::default());
    let a = operation(&graph, "A");
    let b = operation(&graph, "B");
    ready(&a);
    ready(&b);
    let old = edge(&graph, &a, &b);
    graph.finish(a.receipt.attempt);
    let current = edge(&graph, &a, &b);
    drop(old);
    assert!(matches!(
        graph.register(&b.receipt, &a.receipt, true),
        Registration::ReactionBackEdge
    ));
    drop(current);
    let reverse = edge(&graph, &b, &a);
    drop(reverse);
    assert!(graph.0.lock().unwrap().edges.is_empty());
}

#[test]
fn a_replaced_attempt_is_not_the_old_wait_target() {
    let graph = Arc::new(WaitGraph::default());
    let a = operation(&graph, "A");
    let old_b = operation(&graph, "B");
    ready(&a);
    let _old_edge = edge(&graph, &a, &old_b);
    let old_id = old_b.receipt.attempt;
    let mut failed = old_b.receipt.committed.clone();
    failed.success = false;
    old_b.complete(CompletionResult::response(failed));
    let new_b = operation(&graph, "B");
    ready(&new_b);
    assert_ne!(old_id, new_b.receipt.attempt);
    let _new_edge = edge(&graph, &new_b, &a);
}

#[test]
fn unfinished_effects_are_never_a_ready_reaction_back_edge() {
    let graph = Arc::new(WaitGraph::default());
    let a = operation(&graph, "A");
    let b = operation(&graph, "B");
    ready(&a);
    let _edge = edge(&graph, &a, &b);
    assert!(matches!(
        graph.register(&b.receipt, &a.receipt, true),
        Registration::UnfinishedEffectsCycle
    ));
    ready(&b);
    assert!(matches!(
        graph.register(&b.receipt, &a.receipt, true),
        Registration::ReactionBackEdge
    ));
}

#[test]
fn graph_budget_exhaustion_is_not_reaction_success() {
    let graph = Arc::new(WaitGraph::default());
    let owners: Vec<_> = (0..=MAX_CALLBACK_HOPS)
        .map(|n| {
            let owner = operation(&graph, &n.to_string());
            ready(&owner);
            owner
        })
        .collect();
    let _edges: Vec<_> = owners
        .windows(2)
        .map(|pair| edge(&graph, &pair[0], &pair[1]))
        .collect();
    let source = operation(&graph, "independent");
    ready(&source);
    assert!(matches!(
        graph.register(&source.receipt, &owners[0].receipt, true),
        Registration::BudgetExhausted
    ));
    assert_eq!(
        graph.0.lock().unwrap().edges.len(),
        MAX_CALLBACK_HOPS as usize
    );
}

#[test]
fn signalled_effects_only_observer_does_not_create_a_false_cycle() {
    let graph = Arc::new(WaitGraph::default());
    let a = operation(&graph, "A");
    let b = operation(&graph, "B");
    let Registration::Observe(_not_yet_polled) = graph.register(&a.receipt, &b.receipt, false)
    else {
        panic!("effects pending")
    };
    ready(&b);
    // A's effects-only wait is fulfilled, even before A polls and drops it.
    let _legitimate_wait = edge(&graph, &b, &a);
}
