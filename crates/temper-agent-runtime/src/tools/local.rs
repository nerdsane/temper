//! Local tool registry with file I/O, shell, and Temper entity operations.
//!
//! Provides `file_read`, `file_write`, `file_list`, `shell_execute`, and
//! Temper entity operations (`entity_list`, `entity_get`, `entity_action`).

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use serde_json::{Value, json};
use temper_sdk::TemperClient;

use super::{CedarMapping, ToolDef, ToolRegistry, ToolResult};

/// Tool registry with local file/shell access plus Temper entity operations.
///
/// Used by the CLI and local executor deployments where the agent needs
/// filesystem and shell access.
pub struct LocalToolRegistry {
    client: TemperClient,
    /// The current agent's ID, set by the runner.
    agent_id: Arc<Mutex<Option<String>>>,
}

impl LocalToolRegistry {
    /// Create a new local tool registry backed by the given Temper client.
    pub fn new(client: TemperClient) -> Self {
        Self {
            client,
            agent_id: Arc::new(Mutex::new(None)),
        }
    }
}

#[async_trait::async_trait]
impl ToolRegistry for LocalToolRegistry {
    fn set_agent_id(&self, id: &str) {
        *self.agent_id.lock().unwrap() = Some(id.to_string()); // ci-ok: infallible lock
    }

    fn list_tools(&self) -> Vec<ToolDef> {
        vec![
            ToolDef {
                name: "file_read".to_string(),
                description: "Read the contents of a file at the given path.".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Absolute or relative file path to read."
                        }
                    },
                    "required": ["path"]
                }),
            },
            ToolDef {
                name: "file_write".to_string(),
                description: "Write content to a file at the given path. Creates the file if it doesn't exist.".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Absolute or relative file path to write."
                        },
                        "content": {
                            "type": "string",
                            "description": "Content to write to the file."
                        }
                    },
                    "required": ["path", "content"]
                }),
            },
            ToolDef {
                name: "file_list".to_string(),
                description: "List files and directories at the given path.".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Directory path to list. Defaults to current directory."
                        }
                    },
                    "required": []
                }),
            },
            ToolDef {
                name: "shell_execute".to_string(),
                description: "Execute a shell command and return its output.".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "command": {
                            "type": "string",
                            "description": "Shell command to execute."
                        }
                    },
                    "required": ["command"]
                }),
            },
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
            "file_read" => {
                let path = input
                    .get("path")
                    .and_then(|v| v.as_str())
                    .context("file_read: missing 'path' parameter")?;
                match tokio::fs::read_to_string(path).await {
                    Ok(content) => Ok(ToolResult::Success(content)),
                    Err(e) => Ok(ToolResult::Error(format!(
                        "Failed to read file {path}: {e}"
                    ))),
                }
            }
            "file_write" => {
                let path = input
                    .get("path")
                    .and_then(|v| v.as_str())
                    .context("file_write: missing 'path' parameter")?;
                let content = input
                    .get("content")
                    .and_then(|v| v.as_str())
                    .context("file_write: missing 'content' parameter")?;
                if let Some(parent) = Path::new(path).parent() {
                    tokio::fs::create_dir_all(parent).await.ok();
                }
                match tokio::fs::write(path, content).await {
                    Ok(()) => Ok(ToolResult::Success(format!(
                        "Written {} bytes to {path}",
                        content.len()
                    ))),
                    Err(e) => Ok(ToolResult::Error(format!(
                        "Failed to write file {path}: {e}"
                    ))),
                }
            }
            "file_list" => {
                let path = input.get("path").and_then(|v| v.as_str()).unwrap_or(".");
                match tokio::fs::read_dir(path).await {
                    Ok(mut dir) => {
                        let mut entries = Vec::new();
                        while let Ok(Some(entry)) = dir.next_entry().await {
                            let name = entry.file_name().to_string_lossy().to_string();
                            let file_type = entry.file_type().await.ok();
                            let suffix = if file_type.as_ref().is_some_and(|ft| ft.is_dir()) {
                                "/"
                            } else {
                                ""
                            };
                            entries.push(format!("{name}{suffix}"));
                        }
                        entries.sort();
                        Ok(ToolResult::Success(entries.join("\n")))
                    }
                    Err(e) => Ok(ToolResult::Error(format!(
                        "Failed to list directory {path}: {e}"
                    ))),
                }
            }
            "shell_execute" => {
                let command = input
                    .get("command")
                    .and_then(|v| v.as_str())
                    .context("shell_execute: missing 'command' parameter")?;
                match tokio::process::Command::new("sh")
                    .arg("-c")
                    .arg(command)
                    .output()
                    .await
                {
                    Ok(output) => {
                        let stdout = String::from_utf8_lossy(&output.stdout);
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        let mut result = String::new();
                        if !stdout.is_empty() {
                            result.push_str(&stdout);
                        }
                        if !stderr.is_empty() {
                            if !result.is_empty() {
                                result.push('\n');
                            }
                            result.push_str("stderr: ");
                            result.push_str(&stderr);
                        }
                        if !output.status.success() {
                            result.push_str(&format!("\nexit code: {}", output.status));
                        }
                        Ok(ToolResult::Success(result))
                    }
                    Err(e) => Ok(ToolResult::Error(format!("Failed to execute command: {e}"))),
                }
            }
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
            "file_read" => CedarMapping {
                resource_type: "FileSystem".to_string(),
                action: "read".to_string(),
                resource_id: input
                    .get("path")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string(),
            },
            "file_write" => CedarMapping {
                resource_type: "FileSystem".to_string(),
                action: "write".to_string(),
                resource_id: input
                    .get("path")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string(),
            },
            "file_list" => CedarMapping {
                resource_type: "FileSystem".to_string(),
                action: "list".to_string(),
                resource_id: input
                    .get("path")
                    .and_then(|v| v.as_str())
                    .unwrap_or(".")
                    .to_string(),
            },
            "shell_execute" => CedarMapping {
                resource_type: "Shell".to_string(),
                action: "execute".to_string(),
                resource_id: input
                    .get("command")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string(),
            },
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
            "entity_list" | "entity_get" | "entity_action" => CedarMapping {
                resource_type: "Entity".to_string(),
                action: name.to_string(),
                resource_id: input
                    .get("entity_type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string(),
            },
            _ => CedarMapping {
                resource_type: "Unknown".to_string(),
                action: name.to_string(),
                resource_id: "unknown".to_string(),
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
        let registry = LocalToolRegistry::new(client);
        let tools = registry.list_tools();
        assert_eq!(tools.len(), 9);
    }

    #[test]
    fn test_cedar_file_read() {
        let client = TemperClient::new("http://localhost:4200", "default");
        let registry = LocalToolRegistry::new(client);
        let mapping = registry.to_cedar("file_read", &json!({"path": "/tmp/test.txt"}));
        assert_eq!(mapping.resource_type, "FileSystem");
        assert_eq!(mapping.action, "read");
        assert_eq!(mapping.resource_id, "/tmp/test.txt");
    }

    #[test]
    fn test_cedar_entity_action() {
        let client = TemperClient::new("http://localhost:4200", "default");
        let registry = LocalToolRegistry::new(client);
        let mapping = registry.to_cedar(
            "entity_action",
            &json!({"entity_type": "Tasks", "id": "t-1", "action": "Start"}),
        );
        assert_eq!(mapping.resource_type, "Entity");
        assert_eq!(mapping.action, "entity_action");
        assert_eq!(mapping.resource_id, "Tasks");
    }
}
