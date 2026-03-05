//! Temper-only tool registry for sandboxed executors.
//!
//! Provides entity CRUD operations only — no local filesystem or shell access.
//! Used by remote/sandboxed executor deployments.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use serde_json::{Value, json};
use temper_sdk::TemperClient;

use super::{CedarMapping, ToolDef, ToolRegistry, ToolResult};

/// Tool registry with Temper entity operations only.
///
/// Used by sandboxed executor deployments where the agent must not have
/// filesystem or shell access.
pub struct TemperToolRegistry {
    client: TemperClient,
    /// The current agent's ID, set by the runner.
    agent_id: Arc<Mutex<Option<String>>>,
}

impl TemperToolRegistry {
    /// Create a new Temper-only tool registry.
    pub fn new(client: TemperClient) -> Self {
        Self {
            client,
            agent_id: Arc::new(Mutex::new(None)),
        }
    }
}

#[async_trait::async_trait]
impl ToolRegistry for TemperToolRegistry {
    fn set_agent_id(&self, id: &str) {
        *self.agent_id.lock().unwrap() = Some(id.to_string()); // ci-ok: infallible lock
    }

    fn list_tools(&self) -> Vec<ToolDef> {
        vec![
            ToolDef {
                name: "entity_list".to_string(),
                description: "List Temper entities of a given type.".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "entity_type": {
                            "type": "string",
                            "description": "Entity type to list (e.g., 'Tasks', 'Agents')."
                        },
                        "filter": {
                            "type": "string",
                            "description": "Optional OData $filter expression."
                        }
                    },
                    "required": ["entity_type"]
                }),
            },
            ToolDef {
                name: "entity_get".to_string(),
                description: "Get a single Temper entity by type and ID.".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "entity_type": {
                            "type": "string",
                            "description": "Entity type (e.g., 'Tasks')."
                        },
                        "id": {
                            "type": "string",
                            "description": "Entity ID."
                        }
                    },
                    "required": ["entity_type", "id"]
                }),
            },
            ToolDef {
                name: "entity_create".to_string(),
                description: "Create a new Temper entity.".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "entity_type": {
                            "type": "string",
                            "description": "Entity type (e.g., 'Tasks')."
                        },
                        "fields": {
                            "type": "object",
                            "description": "Fields for the new entity."
                        }
                    },
                    "required": ["entity_type", "fields"]
                }),
            },
            ToolDef {
                name: "entity_action".to_string(),
                description: "Invoke an action on a Temper entity.".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "entity_type": {
                            "type": "string",
                            "description": "Entity type (e.g., 'Tasks')."
                        },
                        "id": {
                            "type": "string",
                            "description": "Entity ID."
                        },
                        "action": {
                            "type": "string",
                            "description": "Action name (e.g., 'Start', 'Complete')."
                        },
                        "params": {
                            "type": "object",
                            "description": "Action parameters."
                        }
                    },
                    "required": ["entity_type", "id", "action"]
                }),
            },
            ToolDef {
                name: "spawn_child_agent".to_string(),
                description: "Spawn a child agent to handle a delegated sub-task. The child \
                    runs autonomously and must complete before this agent can complete."
                    .to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "role": {
                            "type": "string",
                            "description": "Role for the child agent (e.g., 'researcher', 'tester')."
                        },
                        "goal": {
                            "type": "string",
                            "description": "Goal for the child agent — what it should accomplish."
                        },
                        "model": {
                            "type": "string",
                            "description": "LLM model for the child (e.g., 'claude-sonnet-4-6'). Optional."
                        }
                    },
                    "required": ["role", "goal"]
                }),
            },
            ToolDef {
                name: "check_children_status".to_string(),
                description: "Check the status of all child agents spawned by this agent. \
                    Returns each child's ID, status, role, goal, and result."
                    .to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {},
                    "required": []
                }),
            },
        ]
    }

    async fn execute(&self, name: &str, input: Value) -> Result<ToolResult> {
        match name {
            "entity_list" => {
                let entity_type = input
                    .get("entity_type")
                    .and_then(|v| v.as_str())
                    .context("entity_list: missing 'entity_type' parameter")?;
                let filter = input.get("filter").and_then(|v| v.as_str());
                let entities = if let Some(f) = filter {
                    self.client.list_filtered(entity_type, f).await?
                } else {
                    self.client.list(entity_type).await?
                };
                Ok(ToolResult::Success(
                    serde_json::to_string_pretty(&entities).unwrap_or_default(),
                ))
            }
            "entity_get" => {
                let entity_type = input
                    .get("entity_type")
                    .and_then(|v| v.as_str())
                    .context("entity_get: missing 'entity_type' parameter")?;
                let id = input
                    .get("id")
                    .and_then(|v| v.as_str())
                    .context("entity_get: missing 'id' parameter")?;
                match self.client.get(entity_type, id).await {
                    Ok(entity) => Ok(ToolResult::Success(
                        serde_json::to_string_pretty(&entity).unwrap_or_default(),
                    )),
                    Err(e) => Ok(ToolResult::Error(e.to_string())),
                }
            }
            "entity_create" => {
                let entity_type = input
                    .get("entity_type")
                    .and_then(|v| v.as_str())
                    .context("entity_create: missing 'entity_type' parameter")?;
                let fields = input.get("fields").cloned().unwrap_or_else(|| json!({}));
                match self.client.create(entity_type, fields).await {
                    Ok(result) => Ok(ToolResult::Success(
                        serde_json::to_string_pretty(&result).unwrap_or_default(),
                    )),
                    Err(e) => Ok(ToolResult::Error(e.to_string())),
                }
            }
            "entity_action" => {
                let entity_type = input
                    .get("entity_type")
                    .and_then(|v| v.as_str())
                    .context("entity_action: missing 'entity_type' parameter")?;
                let id = input
                    .get("id")
                    .and_then(|v| v.as_str())
                    .context("entity_action: missing 'id' parameter")?;
                let action = input
                    .get("action")
                    .and_then(|v| v.as_str())
                    .context("entity_action: missing 'action' parameter")?;
                let params = input.get("params").cloned().unwrap_or_else(|| json!({}));
                match self.client.action(entity_type, id, action, params).await {
                    Ok(result) => Ok(ToolResult::Success(
                        serde_json::to_string_pretty(&result).unwrap_or_default(),
                    )),
                    Err(e) => Ok(ToolResult::Error(e.to_string())),
                }
            }
            "spawn_child_agent" => {
                let agent_id = self
                    .agent_id
                    .lock()
                    .unwrap() // ci-ok: infallible lock
                    .clone()
                    .unwrap_or_default();
                if agent_id.is_empty() {
                    return Ok(ToolResult::Error(
                        "spawn_child_agent: no agent_id set — cannot spawn child".to_string(),
                    ));
                }
                let role = input
                    .get("role")
                    .and_then(|v| v.as_str())
                    .context("spawn_child_agent: missing 'role' parameter")?;
                let goal = input
                    .get("goal")
                    .and_then(|v| v.as_str())
                    .context("spawn_child_agent: missing 'goal' parameter")?;
                let model = input
                    .get("model")
                    .and_then(|v| v.as_str())
                    .unwrap_or("claude-sonnet-4-6");
                match self
                    .client
                    .action(
                        "Agents",
                        &agent_id,
                        "SpawnChild",
                        json!({ "role": role, "goal": goal, "model": model }),
                    )
                    .await
                {
                    Ok(result) => Ok(ToolResult::Success(format!(
                        "Child agent spawned. {}",
                        serde_json::to_string_pretty(&result).unwrap_or_default()
                    ))),
                    Err(e) => Ok(ToolResult::Error(format!(
                        "Failed to spawn child agent: {e}"
                    ))),
                }
            }
            "check_children_status" => {
                let agent_id = self
                    .agent_id
                    .lock()
                    .unwrap() // ci-ok: infallible lock
                    .clone()
                    .unwrap_or_default();
                if agent_id.is_empty() {
                    return Ok(ToolResult::Error(
                        "check_children_status: no agent_id set".to_string(),
                    ));
                }
                let agent = self.client.get("Agents", &agent_id).await?;
                let child_ids = agent
                    .get("child_agent_ids")
                    .or_else(|| agent.get("fields").and_then(|f| f.get("child_agent_ids")))
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                if child_ids.is_empty() {
                    return Ok(ToolResult::Success(
                        "No child agents spawned.".to_string(),
                    ));
                }
                let mut statuses = Vec::new();
                for cid in &child_ids {
                    let id = cid.as_str().unwrap_or_default();
                    match self.client.get("Agents", id).await {
                        Ok(child) => {
                            let status = child
                                .get("status")
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown");
                            let role = child
                                .get("role")
                                .or_else(|| child.get("fields").and_then(|f| f.get("role")))
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            let goal = child
                                .get("goal")
                                .or_else(|| child.get("fields").and_then(|f| f.get("goal")))
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            let result = child
                                .get("result")
                                .or_else(|| child.get("fields").and_then(|f| f.get("result")))
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            statuses.push(json!({
                                "id": id,
                                "status": status,
                                "role": role,
                                "goal": goal,
                                "result": result,
                            }));
                        }
                        Err(e) => {
                            statuses.push(json!({
                                "id": id,
                                "status": "error",
                                "error": e.to_string(),
                            }));
                        }
                    }
                }
                Ok(ToolResult::Success(
                    serde_json::to_string_pretty(&statuses).unwrap_or_default(),
                ))
            }
            other => Ok(ToolResult::Error(format!("Unknown tool: {other}"))),
        }
    }

    fn to_cedar(&self, name: &str, input: &Value) -> CedarMapping {
        match name {
            "spawn_child_agent" => CedarMapping {
                resource_type: "Entity".to_string(),
                action: "SpawnChild".to_string(),
                resource_id: "Agents".to_string(),
            },
            "check_children_status" => CedarMapping {
                resource_type: "Entity".to_string(),
                action: "entity_get".to_string(),
                resource_id: "Agents".to_string(),
            },
            _ => CedarMapping {
                resource_type: "Entity".to_string(),
                action: name.to_string(),
                resource_id: input
                    .get("entity_type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_count() {
        let client = TemperClient::new("http://localhost:4200", "default");
        let registry = TemperToolRegistry::new(client);
        let tools = registry.list_tools();
        assert_eq!(tools.len(), 6);
    }

    #[test]
    fn test_cedar_entity_action() {
        let client = TemperClient::new("http://localhost:4200", "default");
        let registry = TemperToolRegistry::new(client);
        let mapping = registry.to_cedar(
            "entity_action",
            &json!({"entity_type": "Tasks", "id": "t-1"}),
        );
        assert_eq!(mapping.resource_type, "Entity");
        assert_eq!(mapping.resource_id, "Tasks");
    }

    #[test]
    fn test_cedar_spawn_child() {
        let client = TemperClient::new("http://localhost:4200", "default");
        let registry = TemperToolRegistry::new(client);
        let mapping = registry.to_cedar("spawn_child_agent", &json!({}));
        assert_eq!(mapping.resource_type, "Entity");
        assert_eq!(mapping.action, "SpawnChild");
        assert_eq!(mapping.resource_id, "Agents");
    }

    #[test]
    fn test_set_agent_id() {
        let client = TemperClient::new("http://localhost:4200", "default");
        let registry = TemperToolRegistry::new(client);
        registry.set_agent_id("agent-123");
        let id = registry.agent_id.lock().unwrap().clone(); // ci-ok: infallible lock
        assert_eq!(id, Some("agent-123".to_string()));
    }
}
