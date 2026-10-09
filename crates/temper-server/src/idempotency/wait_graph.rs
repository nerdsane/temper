//! Exact-attempt, process-local wait edges. No reaction delivery is stored here.
use super::effects::{OperationPhase, OperationReceipt};
use crate::request_context::MAX_CALLBACK_HOPS;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex, Weak};

const EDGE_BUDGET: usize = super::ROOT_COMPLETION_BUDGET * MAX_CALLBACK_HOPS as usize;

#[derive(Default)]
struct Graph {
    sequence: u64,
    edges: BTreeMap<u64, Edge>,
}
struct Edge {
    source: Weak<OperationReceipt>,
    target: Weak<OperationReceipt>,
    terminal: bool,
}

#[derive(Default)]
pub(crate) struct WaitGraph(Mutex<Graph>);

pub(crate) enum Registration {
    Observe(WaitEdge),
    Satisfied,
    /// This dependency closes a cycle of ready reaction phases. Reusing the
    /// target's ready effects cuts this edge only, not either terminal phase.
    ReactionBackEdge,
    UnfinishedEffectsCycle,
    BudgetExhausted,
}

pub(crate) struct WaitEdge {
    graph: Arc<WaitGraph>,
    ticket: u64,
}

impl Drop for WaitEdge {
    fn drop(&mut self) {
        self.graph
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .edges
            .remove(&self.ticket);
    }
}

impl WaitGraph {
    pub fn attempt_id(&self) -> u64 {
        let mut graph = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        graph.sequence = graph
            .sequence
            .checked_add(1)
            .expect("wait identity overflow");
        graph.sequence
    }

    pub fn finish(&self, attempt: u64) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .edges
            .retain(
                |_, edge| match (edge.source.upgrade(), edge.target.upgrade()) {
                    (Some(source), Some(target)) => {
                        source.attempt != attempt && target.attempt != attempt
                    }
                    _ => false,
                },
            );
    }

    pub fn register(
        self: &Arc<Self>,
        source: &Arc<OperationReceipt>,
        target: &Arc<OperationReceipt>,
        terminal: bool,
    ) -> Registration {
        let mut graph = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // A signalled observer may not have been polled yet. Its fulfilled
        // edge is no longer a dependency and must not manufacture a cycle.
        graph.edges.retain(|_, edge| {
            edge.source.upgrade().is_some()
                && edge
                    .target
                    .upgrade()
                    .is_some_and(|target| target.dependency_pending(edge.terminal))
        });
        if !target.dependency_pending(terminal) {
            return Registration::Satisfied;
        }
        // Registration and cycle decision share this lock. A path consists of
        // exact receipts, not keys that a later quiescent retry can replace.
        let ready = terminal && source.phase() == OperationPhase::RunningReactions;
        let mut queue = VecDeque::from([(target.clone(), ready)]);
        let mut visited = BTreeSet::new();
        while let Some((node, path_ready)) = queue.pop_front() {
            let path_ready = path_ready && node.phase() == OperationPhase::RunningReactions;
            if node.attempt == source.attempt {
                return if path_ready {
                    Registration::ReactionBackEdge
                } else {
                    Registration::UnfinishedEffectsCycle
                };
            }
            if !visited.insert(node.attempt) {
                continue;
            }
            if visited.len() > MAX_CALLBACK_HOPS as usize {
                return Registration::BudgetExhausted;
            }
            for edge in graph.edges.values() {
                if let (Some(from), Some(to)) = (edge.source.upgrade(), edge.target.upgrade())
                    && from.attempt == node.attempt
                {
                    queue.push_back((to, path_ready && edge.terminal));
                }
            }
        }
        if graph.edges.len() >= EDGE_BUDGET {
            return Registration::BudgetExhausted;
        }
        graph.sequence = graph
            .sequence
            .checked_add(1)
            .expect("wait identity overflow");
        let ticket = graph.sequence;
        graph.edges.insert(
            ticket,
            Edge {
                source: Arc::downgrade(source),
                target: Arc::downgrade(target),
                terminal,
            },
        );
        Registration::Observe(WaitEdge {
            graph: self.clone(),
            ticket,
        })
    }
}

#[cfg(test)]
#[path = "wait_graph_test.rs"]
mod tests;
