/// ChildCompletionIntegration — notifies parent when a child Process completes.
///
/// Watches for CompleteProcess/Terminate on a Process that has `parent_pid` set.
/// When found, sends `UnblockFromChild { child_result }` to the parent.
use async_trait::async_trait;
use std::sync::Arc;
use temper_actor_runtime::{Actor, ActorContext, ActorError, ActorHandle, ActorSystem, Message};

use crate::common::message_action;

pub struct ChildCompletionIntegration {
    pub actor_system: Arc<ActorSystem>,
}

#[async_trait]
impl Actor for ChildCompletionIntegration {
    fn actor_type(&self) -> &str {
        "ChildCompletionIntegration"
    }

    fn initial_state(&self) -> Vec<u8> {
        vec![]
    }

    async fn handle(
        &self,
        _ctx: &ActorContext,
        _state: &mut Vec<u8>,
        message: &Message,
    ) -> Result<(), ActorError> {
        let action = message_action(message);
        if action != "notify_parent_completion" {
            return Ok(());
        }

        // Params: { parent_pid, child_result, tenant }
        let params: serde_json::Value =
            serde_json::from_slice(&message.payload).unwrap_or_default();

        let parent_pid = match params["parent_pid"].as_str() {
            Some(p) if !p.is_empty() => p.to_string(),
            _ => return Ok(()), // No parent — nothing to notify.
        };
        let child_result = params["child_result"].clone();
        let tenant = params["tenant"].as_str().unwrap_or("default").to_string();

        let parent_namespace = format!("{tenant}/{parent_pid}");
        let parent = ActorHandle::new(parent_namespace, "Process".to_string());

        tracing::info!(parent_pid, "ChildCompletion: notifying parent");
        self.actor_system
            .tell(
                None,
                &parent,
                temper_actor_runtime::spec_actor::SpecMessage::with_params(
                    "UnblockFromChild",
                    serde_json::json!({ "child_result": child_result }),
                ),
            )
            .await
            .map_err(|e| ActorError::Internal(format!("notify parent: {e}")))?;

        Ok(())
    }
}
