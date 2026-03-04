//! Sandbox-based tool registry with a single `execute_code` tool.
//!
//! The agent's only tool is running Python code in the embedded Monty sandbox.
//! Entity ops go through `temper.*`, local ops through `tools.*` — both
//! dispatched inside the sandbox with Cedar authorization.

use anyhow::Result;
use serde_json::{Value, json};

use super::{CedarMapping, ToolDef, ToolRegistry, ToolResult};
use crate::sandbox::AgentSandbox;

/// Tool registry that provides a single `execute_code` tool backed by the
/// embedded Monty sandbox.
pub struct SandboxToolRegistry {
    sandbox: AgentSandbox,
}

impl SandboxToolRegistry {
    /// Create a new sandbox tool registry.
    pub fn new(sandbox: AgentSandbox) -> Self {
        Self { sandbox }
    }

    /// Set the agent principal ID on the underlying sandbox.
    pub fn set_principal_id(&self, id: String) {
        self.sandbox.set_principal_id(id);
    }
}

#[async_trait::async_trait]
impl ToolRegistry for SandboxToolRegistry {
    fn list_tools(&self) -> Vec<ToolDef> {
        vec![ToolDef {
            name: "execute_code".to_string(),
            description: "Execute Python code in the Temper sandbox. \
                Use `temper.*` for entity operations and `tools.*` for local operations.\n\n\
                Entity methods:\n\
                - `await temper.list(\"EntityType\")` — list entities\n\
                - `await temper.get(\"EntityType\", \"id\")` — get entity\n\
                - `await temper.create(\"EntityType\", {\"field\": \"value\"})` — create entity\n\
                - `await temper.action(\"EntityType\", \"id\", \"ActionName\", {})` — invoke action\n\
                - `await temper.patch(\"EntityType\", \"id\", {\"field\": \"new_value\"})` — patch entity\n\n\
                Navigation:\n\
                - `await temper.navigate(\"path\")` — navigate OData path\n\n\
                Developer methods:\n\
                - `await temper.submit_specs({\"File.ioa.toml\": \"...\"})` — load specs\n\
                - `await temper.get_policies()` — list Cedar policies\n\n\
                WASM integration:\n\
                - `await temper.upload_wasm(\"module_name\", \"/path/to/module.wasm\")` — upload WASM\n\
                - `await temper.compile_wasm(\"module_name\", \"rust source\")` — compile and upload WASM\n\n\
                Governance:\n\
                - `await temper.get_decisions()` — list governance decisions\n\
                - `await temper.get_decision_status(\"PD-xxx\")` — get decision status\n\
                - `await temper.poll_decision(\"PD-xxx\")` — wait for decision\n\n\
                Evolution observability:\n\
                - `await temper.get_trajectories()` — list trajectory spans\n\
                - `await temper.get_insights()` — get evolution insights\n\
                - `await temper.get_evolution_records()` — list evolution records\n\
                - `await temper.check_sentinel()` — trigger sentinel check\n\n\
                Local methods (Cedar-governed):\n\
                - `await tools.bash(\"command\")` — run shell command\n\
                - `await tools.read(\"path\")` — read file\n\
                - `await tools.write(\"path\", \"content\")` — write file\n\
                - `await tools.ls(\"path\")` — list directory"
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "code": {
                        "type": "string",
                        "description": "Python code to execute in the sandbox"
                    }
                },
                "required": ["code"]
            }),
        }]
    }

    async fn execute(&self, name: &str, input: Value) -> Result<ToolResult> {
        if name != "execute_code" {
            return Ok(ToolResult::Error(format!("unknown tool '{name}'")));
        }

        let code = input.get("code").and_then(Value::as_str).unwrap_or("");

        if code.trim().is_empty() {
            return Ok(ToolResult::Success("null".to_string()));
        }

        match self.sandbox.run_code(code).await {
            Ok(output) => Ok(ToolResult::Success(output)),
            Err(e) => Ok(ToolResult::Error(e.to_string())),
        }
    }

    fn to_cedar(&self, _name: &str, _input: &Value) -> CedarMapping {
        // The execute_code tool itself has a top-level Cedar mapping.
        // Individual tools.* calls inside the sandbox do their own checks.
        CedarMapping {
            resource_type: "Sandbox".to_string(),
            action: "execute".to_string(),
            resource_id: "agent-sandbox".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_single_tool_listed() {
        let sandbox = AgentSandbox::new("http://localhost:3000", "default", None);
        let registry = SandboxToolRegistry::new(sandbox);
        let tools = registry.list_tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "execute_code");
    }

    #[test]
    fn test_cedar_mapping() {
        let sandbox = AgentSandbox::new("http://localhost:3000", "default", None);
        let registry = SandboxToolRegistry::new(sandbox);
        let mapping = registry.to_cedar("execute_code", &json!({}));
        assert_eq!(mapping.resource_type, "Sandbox");
        assert_eq!(mapping.action, "execute");
    }
}
