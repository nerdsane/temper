//! Server-owned completion of committed operations, shared by every caller.
use super::effects::PostDispatchContext;
use crate::entity_actor::EntityResponse;
use crate::idempotency::{
    CompletionResult, OperationClaim, OperationOwner, OperationPhase, OperationRequest,
};
use crate::request_context::LocalCompletionEvidence;
use tracing::Instrument;

mod workflow;
use workflow::CompletionWorkflow;

impl OperationRequest {
    fn effects_context(&self) -> PostDispatchContext<'_> {
        PostDispatchContext {
            tenant: &self.tenant,
            entity_type: &self.entity_type,
            entity_id: &self.entity_id,
            action: &self.action,
            agent_ctx: &self.agent_ctx,
            dispatch_idempotency_key: self.idempotency_key.as_deref(),
            action_params: &self.params,
            await_integration: self.await_integration,
        }
    }

    fn actor_key(&self) -> String {
        format!("{}:{}:{}", self.tenant, self.entity_type, self.entity_id)
    }
}

impl crate::state::ServerState {
    pub(super) async fn complete_committed_operation(
        &self,
        request: OperationRequest,
        response: EntityResponse,
        await_reactions: bool,
    ) -> CompletionResult {
        if !response.success {
            let mut workflow = CompletionWorkflow::new(self, &request, &response);
            let response = self
                .run_post_dispatch_effects(&request.effects_context(), response)
                .await;
            workflow.finish(&response);
            return CompletionResult::response(response);
        }
        let actor_key = request.actor_key();
        let ancestor = request
            .idempotency_key
            .as_deref()
            .is_some_and(|key| request.agent_ctx.has_effects_ancestor(&actor_key, key));
        let claim = match request.idempotency_key.as_deref() {
            Some(key) => self
                .idempotency_cache
                .claim_operation(&actor_key, key, request.clone()),
            None => OperationClaim::Uncached,
        };
        let owner = match claim {
            OperationClaim::Join(receipt) => {
                let result = receipt
                    .wait(await_reactions, ancestor, &request.agent_ctx, false)
                    .await;
                CompletionWorkflow::new(self, &request, &result.response).finish(&result.response);
                return result;
            }
            OperationClaim::Historical => {
                let mut response = response;
                response.custom_effects.clear();
                response.scheduled_actions.clear();
                response.spawn_requests.clear();
                CompletionWorkflow::new(self, &request, &response).finish(&response);
                return CompletionResult::response(response);
            }
            OperationClaim::Owner(owner) => owner,
            OperationClaim::Uncached => {
                // An absent entry is not proof of a fresh commit or permission
                // to create unretained work. History recovery remains separate.
                CompletionWorkflow::new(self, &request, &response).finish(&response);
                return CompletionResult::response(EntityResponse {
                    success: false,
                    error: Some("action committed; completion receipt unavailable/unknown; effects may be unstarted".into()),
                    scheduled_actions: Vec::new(), spawn_requests: Vec::new(), ..response
                });
            }
        };
        let receipt = owner.receipt.clone();
        if receipt.permit().is_none() {
            // Descendants share a root lease, including across physical task
            // boundaries. It outlives the root caller until every owned child
            // completes; unknown children pin it. No descendant takes a slot
            // its waiting ancestor needs to release.
            let inherited = request
                .agent_ctx
                .completion_capacity
                .as_ref()
                .and_then(std::sync::Weak::upgrade);
            let permit = inherited.or_else(|| {
                self.idempotency_cache
                    .continuation_slots
                    .clone()
                    .try_acquire_owned()
                    .ok()
                    .map(std::sync::Arc::new)
            });
            if let Some(permit) = permit {
                receipt.retain_permit(permit);
            } else {
                let result =
                    CompletionResult::response(receipt.failure(
                        "action committed; completion capacity exhausted before execution",
                    ));
                CompletionWorkflow::new(self, &request, &result.response).finish(&result.response);
                owner.complete(result.clone());
                return result;
            }
        }
        let state = self.clone();
        // No per-retry task: only a newly owned phase receives a continuation.
        // Caller cancellation detaches a milestone observer, not this work.
        let continuation = async move {
            state.drive_committed_operation(owner).await;
        }
        .instrument(tracing::Span::current());
        let _task = tokio::spawn(continuation); // determinism-ok: owned completion task
        #[cfg(test)]
        receipt.track_worker(_task.abort_handle());
        receipt
            .wait(await_reactions, ancestor, &request.agent_ctx, true)
            .await
    }

    fn drive_committed_operation(
        &self,
        owner: OperationOwner,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = CompletionResult> + Send + '_>> {
        Box::pin(async move {
            let receipt = owner.receipt.clone();
            let mut request = receipt.request.clone();
            let mut workflow = CompletionWorkflow::new(self, &request, &receipt.committed);
            request.agent_ctx.local_completion = LocalCompletionEvidence::for_operation(&receipt);
            request.agent_ctx.completion_capacity =
                receipt.permit().as_ref().map(std::sync::Arc::downgrade);
            if let Some(key) = request.idempotency_key.as_deref() {
                match request
                    .agent_ctx
                    .with_effects_ancestor(&request.actor_key(), key)
                {
                    Ok(ctx) => request.agent_ctx = ctx,
                    Err(error) => {
                        let result = CompletionResult::response(receipt.failure(error));
                        owner.complete(result.clone());
                        return result;
                    }
                }
            }
            // Test-only interruption of an adopted worker, before its effects.
            #[cfg(test)]
            {
                let mut fault = self.idempotency_cache.interrupt_action_once.lock().unwrap();
                if fault.as_ref().is_some_and(|(tenant, kind, action)| {
                    tenant == request.tenant.as_str()
                        && kind == &request.entity_type
                        && action == &request.action
                }) {
                    fault.take();
                    owner.interrupt();
                    return CompletionResult::response(
                        receipt.failure("injected adopted-worker interruption"),
                    );
                }
            }
            if receipt.phase() == OperationPhase::RunningEffects {
                let response = self
                    .run_post_dispatch_effects(
                        &request.effects_context(),
                        receipt.committed.clone(),
                    )
                    .await;
                workflow.finish(&response);
                if request.agent_ctx.local_completion.is_unknown() {
                    owner.interrupt();
                    return receipt.wait(true, false, &request.agent_ctx, false).await;
                }
                if !response.success {
                    let result = CompletionResult::response(response);
                    owner.complete(result.clone());
                    return result;
                }
                self.clear_commons_storage_projection_cache_for_entity(&request.entity_type);
                // reaction_depth was stored before starting effects. A weaker caller
                // can now leave without losing the already-registered obligation.
                owner.publish_effects(response);
                // Test-only phase failure; production adds no await here.
                #[cfg(test)]
                {
                    let mut fault = self.idempotency_cache.reaction_handoff_once.lock().unwrap();
                    if fault.as_ref().is_some_and(|(actor, key)| {
                        actor == &request.actor_key()
                            && Some(key.as_str()) == request.idempotency_key.as_deref()
                    }) {
                        fault.take();
                        return CompletionResult::response(
                            receipt.failure("injected unstarted-reaction handoff"),
                        );
                    }
                }
            }
            if request.detach_reactions {
                // This continuation actually survives the effects-only caller.
                // Clear incoming dependencies, but retain this receipt as an
                // ancestor so A -> B -> A can reuse A's ready effects.
                request.agent_ctx.effects_ancestors.clear();
                if let Some(key) = request.idempotency_key.as_deref() {
                    request.agent_ctx = request
                        .agent_ctx
                        .with_effects_ancestor(&request.actor_key(), key)
                        .expect("empty ancestry admits the current receipt");
                }
            }
            let response = owner.start_reactions();
            workflow.finish(&response);
            let mut result = CompletionResult::response(response);
            let dispatcher = self
                .reaction_dispatcher
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            if let (Some(depth), Some(dispatcher)) = (request.reaction_depth, dispatcher) {
                result.reactions = Box::pin(dispatcher.dispatch_reactions(
                    self,
                    &request.tenant,
                    &request.entity_type,
                    &request.entity_id,
                    &request.action,
                    &result.response.state.status,
                    &serde_json::to_value(&result.response.state.fields).unwrap_or_default(),
                    depth,
                    &request.agent_ctx,
                ))
                .await;
            }
            if request.agent_ctx.local_completion.is_unknown() {
                owner.interrupt();
                return receipt.wait(true, false, &request.agent_ctx, false).await;
            }
            owner.complete(result.clone());
            result
        })
    }
}

#[cfg(test)]
#[path = "completion_test.rs"]
mod tests;

#[cfg(test)]
#[path = "completion_review_test.rs"]
mod review_tests;

#[cfg(test)]
#[path = "completion_workflow_test.rs"]
mod workflow_tests;

#[cfg(test)]
#[path = "completion_local_wasm_test.rs"]
mod local_wasm_tests;

#[cfg(test)]
#[path = "completion_native_result_test.rs"]
mod native_result_tests;

#[cfg(test)]
#[path = "completion_acceptance_test.rs"]
mod acceptance_tests;

#[cfg(test)]
#[path = "completion_precheck_test.rs"]
mod precheck_tests;

#[cfg(test)]
#[path = "completion_compensation_test.rs"]
mod compensation_tests;

#[cfg(test)]
#[path = "completion_reply_proof_test.rs"]
mod reply_proof_tests;

#[cfg(test)]
#[path = "completion_native_reply_test.rs"]
mod native_reply_tests;
