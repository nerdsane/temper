//! Shared agent operations: spawn child agents and check children status.
//!
//! These free functions are used by both [`TemperToolRegistry`] and
//! [`LocalToolRegistry`] to avoid duplicating the spawn/check logic.

use anyhow::Result;
use serde_json::{Value, json};
use temper_sdk::TemperClient;
use tracing::warn;

use super::{CedarMapping, ToolResult};

/// Maximum number of child agents to query in a single `check_children_status`
/// call. Prevents unbounded sequential HTTP calls.
const MAX_CHILDREN_BUDGET: usize = 50;

/// Execute the `spawn_child_agent` tool.
///
/// Issues a `SpawnChild` action on the parent agent entity, which creates
/// a new child agent entity that the executor event loop picks up.
pub async fn execute_spawn_child(
    client: &TemperClient,
    agent_id: &str,
    input: &Value,
) -> Result<ToolResult> {
    if agent_id.is_empty() {
        return Ok(ToolResult::Error(
            "spawn_child_agent: no agent_id set — cannot spawn child".to_string(),
        ));
    }

    let role = input
        .get("role")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("spawn_child_agent: missing 'role' parameter"))?;
    let goal = input
        .get("goal")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("spawn_child_agent: missing 'goal' parameter"))?;
    let model = input
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("claude-sonnet-4-6");

    match client
        .action(
            "Agents",
            agent_id,
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

/// Execute the `check_children_status` tool.
///
/// Fetches the parent agent, reads `child_agent_ids`, then queries each
/// child (up to [`MAX_CHILDREN_BUDGET`]) for its status.
pub async fn execute_check_children(
    client: &TemperClient,
    agent_id: &str,
) -> Result<ToolResult> {
    if agent_id.is_empty() {
        return Ok(ToolResult::Error(
            "check_children_status: no agent_id set".to_string(),
        ));
    }

    let agent = client.get("Agents", agent_id).await?;
    let child_ids = agent
        .get("child_agent_ids")
        .or_else(|| agent.get("fields").and_then(|f| f.get("child_agent_ids")))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    if child_ids.is_empty() {
        return Ok(ToolResult::Success("No child agents spawned.".to_string()));
    }

    if child_ids.len() > MAX_CHILDREN_BUDGET {
        warn!(
            "Agent {agent_id} has {} children, capped at {MAX_CHILDREN_BUDGET}",
            child_ids.len()
        );
    }

    let mut statuses = Vec::new();
    for cid in child_ids.iter().take(MAX_CHILDREN_BUDGET) {
        let id = cid.as_str().unwrap_or_default();
        match client.get("Agents", id).await {
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

/// Cedar mapping for `spawn_child_agent`.
pub fn cedar_spawn_child() -> CedarMapping {
    CedarMapping {
        resource_type: "Entity".to_string(),
        action: "SpawnChild".to_string(),
        resource_id: "Agents".to_string(),
    }
}

/// Cedar mapping for `check_children_status`.
pub fn cedar_check_children() -> CedarMapping {
    CedarMapping {
        resource_type: "Entity".to_string(),
        action: "entity_get".to_string(),
        resource_id: "Agents".to_string(),
    }
}
