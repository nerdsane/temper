//! Process-local committed-operation receipts (ADR-0183).
use std::sync::{Arc, Mutex};
use tokio::sync::{OwnedSemaphorePermit, watch};

use super::IdempotencyCache;
use super::wait_graph::{Registration, WaitGraph};
use crate::entity_actor::EntityResponse;
use crate::request_context::AgentContext;
use crate::trigger::ReactionResult;
use temper_runtime::tenant::TenantId;

pub(super) enum EffectsState {
    Pending,
    Operation(Arc<OperationReceipt>),
    Historical,
}

impl EffectsState {
    pub(super) fn is_claimed(&self) -> bool {
        matches!(self, Self::Operation(receipt) if receipt.is_live())
    }
}

/// Immutable first-dispatch input, including its integration mode and obligation.
#[derive(Clone)]
pub(crate) struct OperationRequest {
    pub tenant: TenantId,
    pub entity_type: String,
    pub entity_id: String,
    pub action: String,
    pub params: serde_json::Value,
    pub agent_ctx: AgentContext,
    pub idempotency_key: Option<String>,
    pub await_integration: bool,
    pub reaction_depth: Option<u32>,
    pub detach_reactions: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OperationPhase {
    RunningEffects,
    PendingReactions,
    RunningReactions,
    Terminal,
    Failed,
    Interrupted,
}

#[derive(Clone)]
pub(crate) struct CompletionResult {
    pub response: EntityResponse,
    pub reactions: Vec<ReactionResult>,
}

impl CompletionResult {
    pub fn response(response: EntityResponse) -> Self {
        Self {
            response,
            reactions: Vec::new(),
        }
    }
}

#[derive(Clone)]
struct Progress {
    phase: OperationPhase,
    effects_ready: Option<EntityResponse>,
    result: Option<CompletionResult>,
    owned: bool,
    finished_at: Option<chrono::DateTime<chrono::Utc>>,
}

pub(crate) struct OperationReceipt {
    pub attempt: u64,
    graph: Arc<WaitGraph>,
    pub request: OperationRequest,
    pub committed: EntityResponse,
    progress: watch::Sender<Progress>,
    // Interrupted work retains this permit as well as its receipt. Dropping a
    // join handle is not evidence that possibly surviving children stopped.
    permit: Mutex<Option<Arc<OwnedSemaphorePermit>>>,
    #[cfg(test)]
    worker_abort: Mutex<Option<tokio::task::AbortHandle>>,
}

impl OperationReceipt {
    pub fn new(
        request: OperationRequest,
        committed: EntityResponse,
        graph: Arc<WaitGraph>,
    ) -> Arc<Self> {
        let (progress, _) = watch::channel(Progress {
            phase: OperationPhase::RunningEffects,
            effects_ready: None,
            result: None,
            owned: true,
            finished_at: None,
        });
        Arc::new(Self {
            attempt: graph.attempt_id(),
            graph,
            request,
            committed,
            progress,
            permit: Mutex::new(None),
            #[cfg(test)]
            worker_abort: Mutex::new(None),
        })
    }

    #[cfg(test)]
    pub fn track_worker(&self, abort: tokio::task::AbortHandle) {
        *self.worker_abort.lock().unwrap() = Some(abort);
    }

    #[cfg(test)]
    pub fn interrupt_worker(&self) {
        self.worker_abort
            .lock()
            .expect("worker abort lock poisoned")
            .as_ref()
            .expect("owned worker started")
            .abort();
    }

    pub fn phase(&self) -> OperationPhase {
        self.progress.borrow().phase
    }

    fn is_live(&self) -> bool {
        !matches!(
            self.phase(),
            OperationPhase::Terminal | OperationPhase::Failed
        )
    }

    pub(super) fn dependency_pending(&self, terminal: bool) -> bool {
        let progress = self.progress.borrow();
        progress.result.is_none() && (terminal || progress.effects_ready.is_none())
    }

    pub fn finished_at(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        self.progress.borrow().finished_at
    }

    pub fn terminal(&self) -> Option<EntityResponse> {
        let progress = self.progress.borrow();
        (progress.phase == OperationPhase::Terminal).then(|| {
            progress
                .result
                .as_ref()
                .expect("terminal result")
                .response
                .clone()
        })
    }

    pub fn retain_permit(&self, permit: Arc<OwnedSemaphorePermit>) {
        *self
            .permit
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(permit);
    }

    pub fn permit(&self) -> Option<Arc<OwnedSemaphorePermit>> {
        self.permit
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn failure(&self, error: &str) -> EntityResponse {
        EntityResponse {
            success: false,
            error: Some(error.into()),
            scheduled_actions: Vec::new(),
            spawn_requests: Vec::new(),
            ..self.committed.clone()
        }
    }

    pub async fn wait(
        self: &Arc<Self>,
        await_reactions: bool,
        ancestor: bool,
        ctx: &AgentContext,
        owner_observer: bool,
    ) -> CompletionResult {
        let mut progress = self.progress.subscribe();
        loop {
            {
                let state = progress.borrow_and_update();
                if (!await_reactions || ancestor)
                    && let Some(response) = &state.effects_ready
                {
                    return CompletionResult::response(response.clone());
                }
                if let Some(result) = &state.result {
                    if state.phase == OperationPhase::Interrupted {
                        ctx.local_completion.mark_unknown();
                    }
                    // A replay observes completion, not a fresh rule execution.
                    return if owner_observer {
                        result.clone()
                    } else {
                        CompletionResult::response(result.response.clone())
                    };
                }
                if ancestor {
                    return CompletionResult::response(self.failure(
                        "inline callback cannot await an ancestor's post-dispatch effects claim",
                    ));
                }
            }
            // Rebuild on every phase notification: release and recovery can
            // coalesce, and an old phase's ticket may already have been removed.
            let _edge = if let Some(source) = ctx.local_completion.operation()
                && Arc::ptr_eq(&source.graph, &self.graph)
            {
                match self.graph.register(&source, self, await_reactions) {
                    Registration::Observe(edge) => Some(edge),
                    Registration::Satisfied => continue,
                    Registration::ReactionBackEdge => {
                        return CompletionResult::response(
                            self.progress
                                .borrow()
                                .effects_ready
                                .clone()
                                .expect("a proven reaction cycle has ready effects"),
                        );
                    }
                    Registration::UnfinishedEffectsCycle => {
                        return CompletionResult::response(self.failure(
                            "inline completion dependency cannot await an unfinished-effects cycle",
                        ));
                    }
                    Registration::BudgetExhausted => {
                        ctx.local_completion.mark_unknown();
                        return CompletionResult::response(self.failure(
                            "completion dependency budget exhausted; child completion is unknown",
                        ));
                    }
                }
            } else {
                None
            };
            progress
                .changed()
                .await
                .expect("receipt retains progress sender");
        }
    }
}

pub(crate) enum OperationClaim {
    Owner(OperationOwner),
    Join(Arc<OperationReceipt>),
    Historical,
    Uncached,
}

/// Owned by a continuation, never by a requesting future. Unexpected Drop
/// interrupts rather than releasing work whose children may still be alive.
pub(crate) struct OperationOwner {
    pub receipt: Arc<OperationReceipt>,
    finished: bool,
}

impl OperationOwner {
    pub fn new(receipt: Arc<OperationReceipt>) -> Self {
        Self {
            receipt,
            finished: false,
        }
    }

    pub fn publish_effects(&self, response: EntityResponse) {
        assert_eq!(self.receipt.phase(), OperationPhase::RunningEffects);
        self.receipt.progress.send_modify(|state| {
            state.effects_ready = Some(response);
            state.phase = OperationPhase::PendingReactions;
        });
    }

    pub fn start_reactions(&self) -> EntityResponse {
        let mut response = None;
        self.receipt.progress.send_modify(|state| {
            assert!(state.phase == OperationPhase::PendingReactions);
            state.phase = OperationPhase::RunningReactions;
            response = state.effects_ready.clone();
        });
        response.expect("reactions require ready effects")
    }

    pub fn complete(mut self, result: CompletionResult) {
        self.receipt.progress.send_modify(|state| {
            state.phase = if result.response.success {
                OperationPhase::Terminal
            } else {
                OperationPhase::Failed
            };
            state.finished_at = Some(temper_runtime::scheduler::sim_now());
            state.result = Some(result);
            state.owned = false;
        });
        self.receipt.graph.finish(self.receipt.attempt);
        self.receipt
            .permit
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        self.finished = true;
    }

    pub fn interrupt(mut self) {
        self.publish_interruption();
        self.finished = true;
    }

    fn publish_interruption(&self) {
        self.receipt.progress.send_modify(|state| {
            state.phase = OperationPhase::Interrupted;
            state.result = Some(CompletionResult::response(self.receipt.failure(
                "committed operation interrupted: child completion is unknown; automatic retry is disabled")));
            state.owned = false;
        });
        self.receipt.graph.finish(self.receipt.attempt);
    }
}

impl Drop for OperationOwner {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        if self.receipt.phase() == OperationPhase::PendingReactions {
            // No reaction has been started. Its effects milestone is proven;
            // another continuation can drive ONLY the unstarted reaction phase.
            self.receipt.progress.send_modify(|state| {
                state.owned = false;
                state.result = Some(CompletionResult::response(self.receipt.failure(
                    "committed effects ready; continuation ended before reactions started",
                )));
            });
            self.receipt.graph.finish(self.receipt.attempt);
        } else {
            self.publish_interruption();
        }
    }
}

impl IdempotencyCache {
    pub(crate) fn claim_operation(
        &self,
        actor_key: &str,
        idem_key: &str,
        request: OperationRequest,
    ) -> OperationClaim {
        let mut entries = self
            .entries
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        #[cfg(test)]
        {
            let mut fault = self.missing_receipt_once.lock().unwrap();
            if fault
                .as_ref()
                .is_some_and(|(actor, key)| actor == actor_key && key == idem_key)
            {
                fault.take();
                if let Some(actor) = entries.get_mut(actor_key) {
                    actor.remove(idem_key);
                }
            }
        }
        let Some(entry) = entries
            .get_mut(actor_key)
            .and_then(|entries| entries.get_mut(idem_key))
        else {
            return OperationClaim::Uncached;
        };
        match &entry.effects {
            EffectsState::Pending => {
                let mut request = request;
                if let Some(super::ActionReplyMode::Dispatch {
                    await_integration,
                    await_reactions,
                    reaction_depth,
                }) = entry.first_reply_mode
                {
                    request.await_integration = await_integration;
                    request.reaction_depth = reaction_depth;
                    request.detach_reactions = !await_reactions;
                }
                let receipt =
                    OperationReceipt::new(request, entry.response.clone(), self.wait_graph.clone());
                entry.effects = EffectsState::Operation(receipt.clone());
                OperationClaim::Owner(OperationOwner::new(receipt))
            }
            EffectsState::Operation(receipt) => {
                let progress = receipt.progress.borrow().clone();
                if progress.phase == OperationPhase::Failed {
                    let receipt = OperationReceipt::new(
                        receipt.request.clone(),
                        receipt.committed.clone(),
                        self.wait_graph.clone(),
                    );
                    entry.effects = EffectsState::Operation(receipt.clone());
                    OperationClaim::Owner(OperationOwner::new(receipt))
                } else if progress.phase == OperationPhase::PendingReactions && !progress.owned {
                    receipt.progress.send_modify(|state| {
                        state.owned = true;
                        state.result = None;
                    });
                    OperationClaim::Owner(OperationOwner::new(receipt.clone()))
                } else {
                    OperationClaim::Join(receipt.clone())
                }
            }
            EffectsState::Historical => OperationClaim::Historical,
        }
    }
}
