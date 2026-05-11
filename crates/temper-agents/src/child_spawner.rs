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

use crate::common::{decode_params, message_action, session_id_from_namespace};

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

        let params = decode_params(message);

        let parent_namespace = ctx.self_handle().namespace.clone();
        let parent_id = session_id_from_namespace(&parent_namespace).to_string();
        let child_user_prompt = params["child_user_prompt"]
            .as_str()
            .unwrap_or("")
            .to_string();
        // Parent-selected tool allowlist for the child. If absent, child gets no tools.
        let child_tool_names: Vec<String> = params["tool_names"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();

        // Generate child process ID (or use caller-provided id).
        let child_id = params["child_process_id"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| format!("{parent_id}-child-{}", uuid::Uuid::new_v4()));

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

        // Parent decides child tool access. Copy only explicitly allowed tools from
        // parent ToolRegistry to child ToolRegistry.
        if !child_tool_names.is_empty() {
            let parent_registry =
                ActorHandle::new(parent_namespace.clone(), "ToolRegistry".to_string());
            let child_registry =
                ActorHandle::new(child_namespace.clone(), "ToolRegistry".to_string());
            if let Ok(resp) = self
                .actor_system
                .ask(
                    ctx.self_handle(),
                    &parent_registry,
                    temper_actor_runtime::spec_actor::SpecMessage::new("GetAllTools"),
                    std::time::Duration::from_secs(3),
                )
                .await
            {
                let all = resp
                    .decode::<temper_actor_runtime::spec_actor::SpecMessage>()
                    .ok()
                    .and_then(|m| serde_json::from_slice::<serde_json::Value>(&m.params).ok())
                    .unwrap_or(serde_json::json!({}));
                if let Some(tools) = all["tools"].as_array() {
                    for tool in tools {
                        let Some(name) = tool["name"].as_str() else {
                            continue;
                        };
                        if !child_tool_names.iter().any(|n| n == name) {
                            continue;
                        }
                        let source = tool["source"].as_str().unwrap_or("builtin");
                        if source == "client" {
                            let client_id = tool["client_id"].as_str().unwrap_or("");
                            let _ = self.actor_system.tell(
                                None,
                                &child_registry,
                                temper_actor_runtime::spec_actor::SpecMessage::with_params(
                                    "RegisterTool",
                                    serde_json::json!({ "client_id": client_id, "tool_names": [name] }),
                                ),
                            ).await;
                        } else {
                            let mut info = tool.clone();
                            info["name"] = serde_json::json!(name);
                            let _ = self
                                .actor_system
                                .tell(
                                    None,
                                    &child_registry,
                                    temper_actor_runtime::spec_actor::SpecMessage::with_params(
                                        "RegisterServerTool",
                                        info,
                                    ),
                                )
                                .await;
                        }
                    }
                    let _ = self.actor_system.activate_now(&child_registry).await;
                }
            }
        }

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
