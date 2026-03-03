//! Temper-only tool registry for sandboxed executors.
//!
//! Provides entity CRUD operations only — no local filesystem or shell access.
//! Used by remote/sandboxed executor deployments.

use anyhow::{Context, Result};
use serde_json::{json, Value};
use temper_sdk::TemperClient;

use super::{CedarMapping, ToolDef, ToolRegistry, ToolResult};

/// Tool registry with Temper entity operations only.
///
/// Used by sandboxed executor deployments where the agent must not have
/// filesystem or shell access.
pub struct TemperToolRegistry {
    client: TemperClient,
}

impl TemperToolRegistry {
    /// Create a new Temper-only tool registry.
    pub fn new(client: TemperClient) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl ToolRegistry for TemperToolRegistry {
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
                let fields = input
                    .get("fields")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
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
        CedarMapping {
            resource_type: "Entity".to_string(),
            action: name.to_string(),
            resource_id: input
                .get("entity_type")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string(),
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
        assert_eq!(tools.len(), 4);
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
}
