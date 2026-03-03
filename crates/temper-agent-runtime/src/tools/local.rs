//! Local tool registry with file I/O, shell, and Temper entity operations.
//!
//! Provides `file_read`, `file_write`, `file_list`, `shell_execute`, and
//! Temper entity operations (`entity_list`, `entity_get`, `entity_action`).

use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use temper_sdk::TemperClient;

use super::{CedarMapping, ToolDef, ToolRegistry, ToolResult};

/// Tool registry with local file/shell access plus Temper entity operations.
///
/// Used by the CLI and local executor deployments where the agent needs
/// filesystem and shell access.
pub struct LocalToolRegistry {
    client: TemperClient,
}

impl LocalToolRegistry {
    /// Create a new local tool registry backed by the given Temper client.
    pub fn new(client: TemperClient) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl ToolRegistry for LocalToolRegistry {
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
                    Err(e) => Ok(ToolResult::Error(format!("Failed to read file {path}: {e}"))),
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
                let path = input
                    .get("path")
                    .and_then(|v| v.as_str())
                    .unwrap_or(".");
                match tokio::fs::read_dir(path).await {
                    Ok(mut dir) => {
                        let mut entries = Vec::new();
                        while let Ok(Some(entry)) = dir.next_entry().await {
                            let name = entry.file_name().to_string_lossy().to_string();
                            let file_type = entry.file_type().await.ok();
                            let suffix =
                                if file_type.as_ref().is_some_and(|ft| ft.is_dir()) {
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
                    Err(e) => Ok(ToolResult::Error(format!(
                        "Failed to execute command: {e}"
                    ))),
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
                let params = input
                    .get("params")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                match self.client.action(entity_type, id, action, params).await {
                    Ok(result) => Ok(ToolResult::Success(
                        serde_json::to_string_pretty(&result).unwrap_or_default(),
                    )),
                    Err(e) => Ok(ToolResult::Error(e.to_string())),
                }
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
        assert_eq!(tools.len(), 7);
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
