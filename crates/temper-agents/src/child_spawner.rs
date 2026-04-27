/// ChildSpawnerIntegration — drives the SpawnChild primitive.
///
/// When a Process executes `SpawnChild { child_definition_id, child_user_prompt }`,
/// this actor:
/// 1. Creates a new Process actor in the same namespace with a child ID
/// 2. Sets `parent_pid` in the child's fields so it can notify the parent on completion
/// 3. Sends StartProcess to the child with the user_prompt
use async_trait::async_trait;
use std::sync::Arc;
use temper_actor_runtime::{Actor, ActorContext, ActorError, ActorHandle, ActorSystem, Message};

use crate::common::message_action;
use crate::common::session_id_from_namespace;

pub struct ChildSpawnerIntegration {
    pub actor_system: Arc<ActorSystem>,
}

#[async_trait]
impl Actor for ChildSpawnerIntegration {
    fn actor_type(&self) -> &str {
        "ChildSpawnerIntegration"
    }

    fn initial_state(&self) -> Vec<u8> {
        vec![]
    }

    async fn handle(
        &self,
        ctx: &ActorContext,
        _state: &mut Vec<u8>,
        message: &Message,
    ) -> Result<(), ActorError> {
        if message_action(message) != "spawn_child" {
            return Ok(());
        }

        let params: serde_json::Value =
            serde_json::from_slice(&message.payload).unwrap_or_default();

        let parent_namespace = ctx.self_handle().namespace.clone();
        let parent_id = session_id_from_namespace(&parent_namespace).to_string();
        let child_user_prompt = params["child_user_prompt"]
            .as_str()
            .unwrap_or("")
            .to_string();

        // Generate a child process ID: parent_id + child counter.
        let child_id = format!("{parent_id}-child-{}", uuid::Uuid::new_v4());

        // Extract tenant from namespace: "tenant/parent_id" → "tenant"
        let tenant = parent_namespace
            .split('/')
            .next()
            .unwrap_or("default")
            .to_string();
        let child_namespace = format!("{tenant}/{child_id}");

        tracing::info!(parent_id, child_id, "ChildSpawner: spawning child process");

        // Spawn all session actors for the child (same pattern as Process creation).
        self.actor_system
            .spawn_all_registered(&child_namespace)
            .await
            .map_err(|e| ActorError::Internal(format!("spawn child: {e}")))?;

        let child_process = ActorHandle::new(child_namespace.clone(), "Process".to_string());

        // Initialize with parent_pid so child can notify parent on completion.
        self.actor_system
            .tell(
                None,
                &child_process,
                temper_actor_runtime::spec_actor::SpecMessage::with_params(
                    "Initialize",
                    serde_json::json!({ "parent_pid": parent_id }),
                ),
            )
            .await
            .map_err(|e| ActorError::Internal(format!("init child: {e}")))?;

        // Start the child process.
        self.actor_system
            .tell(
                None,
                &child_process,
                temper_actor_runtime::spec_actor::SpecMessage::with_params(
                    "StartProcess",
                    serde_json::json!({ "user_prompt": child_user_prompt }),
                ),
            )
            .await
            .map_err(|e| ActorError::Internal(format!("start child: {e}")))?;

        tracing::info!(parent_id, child_id, "ChildSpawner: child process started");
        Ok(())
    }
}
