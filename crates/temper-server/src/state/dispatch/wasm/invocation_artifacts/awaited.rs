//! Awaited callback identity, completion, and atomic receipt construction.

use sha2::{Digest, Sha256};

use super::super::WasmEntityRef;
use crate::request_context::AgentContext;
use temper_runtime::scheduler::sim_now;

pub(super) fn awaited_callback_failure_class(
    error: &crate::state::DispatchError,
) -> crate::trigger::delivery::AwaitedExecutionFailureClass {
    match error {
        crate::state::DispatchError::Transient {
            source: temper_runtime::actor::ActorError::AskTimeout(_),
            ..
        }
        | crate::state::DispatchError::Deferred { .. } => {
            crate::trigger::delivery::AwaitedExecutionFailureClass::CallbackTimeout
        }
        crate::state::DispatchError::Transient { .. }
        | crate::state::DispatchError::Internal(_) => {
            crate::trigger::delivery::AwaitedExecutionFailureClass::CallbackStorageFailure
        }
        _ => crate::trigger::delivery::AwaitedExecutionFailureClass::CallbackRejected,
    }
}

pub(super) fn callback_agent_context(
    agent_ctx: &AgentContext,
    integration_name: &str,
    module_name: &str,
    callback_action: &str,
) -> AgentContext {
    let mut callback_ctx = agent_ctx.clone();
    callback_ctx.idempotency_key = agent_ctx.idempotency_key.as_ref().map(|parent| {
        let mut digest = Sha256::new();
        digest.update(b"temper-wasm-callback-v1\0");
        digest.update(parent.as_bytes());
        digest.update(b"\0");
        digest.update(integration_name.as_bytes());
        digest.update(b"\0");
        digest.update(module_name.as_bytes());
        digest.update(b"\0");
        digest.update(callback_action.as_bytes());
        format!("wasm-callback:{:x}", digest.finalize())
    });
    callback_ctx
}

impl crate::state::ServerState {
    pub(super) async fn complete_awaited_module_failure(
        &self,
        dispatch_idempotency_key: Option<&str>,
        owner_agent_ctx: &AgentContext,
        callback_action: Option<&str>,
        callback_params: Option<serde_json::Value>,
    ) -> Result<(), String> {
        let Some(owner) = dispatch_idempotency_key
            .and_then(|delivery_id| self.awaited_execution_owner(delivery_id, owner_agent_ctx))
        else {
            return Ok(());
        };
        let (record, _) = owner.snapshot().await;
        let Some(evidence) = record.awaited_execution else {
            return Ok(());
        };
        owner
            .complete(
                &evidence.identity.execution_id,
                false,
                callback_action,
                callback_params,
                Some(crate::trigger::delivery::AwaitedExecutionFailureClass::ModuleFailure),
                sim_now(),
            )
            .await
    }

    pub(super) async fn settle_awaited_typed_failure<'a>(
        &self,
        dispatch_idempotency_key: Option<&str>,
        owner_agent_ctx: &AgentContext,
        callback_result: Result<&'a str, String>,
        callback_params: serde_json::Value,
    ) -> Result<&'a str, String> {
        match callback_result {
            Ok(callback) => {
                self.complete_awaited_module_failure(
                    dispatch_idempotency_key,
                    owner_agent_ctx,
                    Some(callback),
                    Some(callback_params),
                )
                .await?;
                Ok(callback)
            }
            Err(error) => {
                self.complete_awaited_module_failure(
                    dispatch_idempotency_key,
                    owner_agent_ctx,
                    None,
                    None,
                )
                .await?;
                Err(error)
            }
        }
    }

    pub(super) async fn awaited_callback_commit_context(
        &self,
        entity_ref: WasmEntityRef<'_>,
        callback_action: &str,
        callback_params: &serde_json::Value,
        agent_ctx: &AgentContext,
    ) -> Result<Option<crate::trigger::delivery::ReactionCommitContext>, String> {
        let Some(delivery_id) = agent_ctx.idempotency_key.as_deref() else {
            return Ok(None);
        };
        let Some(owner) = self.awaited_execution_owner(delivery_id, agent_ctx) else {
            return Ok(None);
        };
        let (delivery, _) = owner.snapshot().await;
        let evidence = delivery
            .awaited_execution
            .as_ref()
            .ok_or_else(|| "awaited callback has no durable execution evidence".to_string())?;
        if evidence.callback_action.as_deref() != Some(callback_action) {
            return Err("awaited callback action does not match completion evidence".to_string());
        }
        let collection = delivery
            .intent
            .collection
            .clone()
            .ok_or_else(|| "awaited callback delivery has no collection fence".to_string())?;
        let rules = if let Some(pin) = agent_ctx.schema_pin.as_ref() {
            self.registry
                .read()
                .map_err(|_| "registry lock poisoned".to_string())?
                .scoped_reaction_candidates_at_digest(
                    entity_ref.tenant,
                    &pin.scope,
                    &pin.bundle_digest,
                    entity_ref.entity_type,
                    callback_action,
                )
        } else {
            self.reaction_dispatcher
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
                .map_or_else(Vec::new, |dispatcher| {
                    dispatcher.candidate_rules(
                        entity_ref.tenant,
                        entity_ref.entity_type,
                        callback_action,
                    )
                })
        };
        let guard_source = match agent_ctx.schema_pin.as_ref() {
            Some(pin) => {
                self.get_or_initialize_scoped_entity_state(
                    entity_ref.tenant,
                    entity_ref.entity_type,
                    entity_ref.entity_id,
                    pin.clone(),
                )
                .await
            }
            None => {
                self.get_tenant_entity_state(
                    entity_ref.tenant,
                    entity_ref.entity_type,
                    entity_ref.entity_id,
                )
                .await
            }
        }
        .map_err(|error| error.to_string())?;
        let expected_source_sequence = guard_source.state.sequence_nr;
        let mut guard_fields = guard_source.state.fields;
        if let (Some(fields), Some(params)) =
            (guard_fields.as_object_mut(), callback_params.as_object())
        {
            fields.extend(params.clone());
        }
        let resolved_guards = crate::trigger::dispatcher::resolve_rule_guard_inputs(
            self,
            entity_ref.tenant,
            &rules,
            &guard_fields,
            agent_ctx.schema_pin.as_ref(),
        )
        .await;
        Ok(Some(crate::trigger::delivery::ReactionCommitContext {
            rules,
            authority: delivery.intent.authority.clone(),
            depth: delivery.intent.depth + 1,
            root_delivery_id: Some(delivery.intent.root_delivery_id.clone()),
            expected_source_sequence,
            resolved_guards,
            receipt: Some(crate::trigger::delivery::ReactionReceipt {
                delivery_id: delivery.intent.delivery_id.clone(),
                fencing_token: delivery.fencing_token,
                received_at: sim_now(),
                state_timeout_state: delivery
                    .intent
                    .state_timeout
                    .as_ref()
                    .map(|timeout| timeout.state.clone()),
                schema_pin: delivery.intent.schema_pin.clone(),
                collection: Some(collection),
                awaited_callback: Some(crate::trigger::delivery::AwaitedCallbackReceiptV1 {
                    execution_id: evidence.identity.execution_id.clone(),
                    callback_action: callback_action.to_string(),
                }),
            }),
        }))
    }
}
