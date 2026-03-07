//! Agent type resolution and single-agent execution.

use anyhow::Result;
use temper_agent_runtime::{AgentRunner, AnthropicProvider, LocalToolRegistry, TemperToolRegistry};
use temper_sdk::TemperClient;
use tracing::{info, warn};

/// Default system prompt when no AgentType is configured.
pub const DEFAULT_SYSTEM_PROMPT: &str = "You are a Temper agent. Accomplish your assigned goal \
    using the tools available to you. Report results clearly.\n\n\
    ## Delegation\n\
    For complex tasks, you can delegate sub-tasks to child agents:\n\
    - `spawn_child_agent(role, goal, model)` — spawns a child that runs autonomously\n\
    - `check_children_status()` — check progress of all spawned children\n\
    You cannot complete until all children have finished (Completed or Failed).";

/// Resolve an AgentType entity for the given agent, returning (system_prompt, tool_set, model).
///
/// Falls back to CLI defaults when the agent has no agent_type_id or the AgentType
/// entity is not found.
pub async fn resolve_agent_type(
    client: &TemperClient,
    agent: &serde_json::Value,
    default_tool_mode: &str,
    default_model: &str,
) -> (String, String, String) {
    let agent_type_id = agent
        .get("agent_type_id")
        .or_else(|| agent.get("fields").and_then(|f| f.get("agent_type_id")))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if agent_type_id.is_empty() {
        return (
            DEFAULT_SYSTEM_PROMPT.to_string(),
            default_tool_mode.to_string(),
            default_model.to_string(),
        );
    }

    match client.get("AgentTypes", agent_type_id).await {
        Ok(at) => {
            let resolve = |key: &str, default: &str| -> String {
                at.get(key)
                    .or_else(|| at.get("fields").and_then(|f| f.get(key)))
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .unwrap_or(default)
                    .to_string()
            };
            let prompt = resolve("system_prompt", DEFAULT_SYSTEM_PROMPT);
            let tool_set = resolve("tool_set", default_tool_mode);
            let model = resolve("model", default_model);
            info!(
                agent_type_id = %agent_type_id,
                model = %model,
                tool_set = %tool_set,
                "Resolved AgentType"
            );
            (prompt, tool_set, model)
        }
        Err(e) => {
            warn!(
                agent_type_id = %agent_type_id,
                "Failed to resolve AgentType: {e}. Using defaults."
            );
            (
                DEFAULT_SYSTEM_PROMPT.to_string(),
                default_tool_mode.to_string(),
                default_model.to_string(),
            )
        }
    }
}

/// Resolve only the model from an AgentType entity by its ID.
///
/// Returns the model field from the AgentType, or the provided default.
pub async fn resolve_agent_type_model(
    client: &TemperClient,
    agent_type_id: &str,
    default_model: &str,
) -> String {
    if agent_type_id.is_empty() {
        return default_model.to_string();
    }

    match client.get("AgentTypes", agent_type_id).await {
        Ok(at) => at
            .get("model")
            .or_else(|| at.get("fields").and_then(|f| f.get("model")))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(default_model)
            .to_string(),
        Err(e) => {
            warn!(
                agent_type_id = %agent_type_id,
                "Failed to resolve AgentType model: {e}. Using default."
            );
            default_model.to_string()
        }
    }
}

/// Run a single agent to completion.
pub async fn run_agent(
    temper_url: &str,
    tenant: &str,
    agent_id: &str,
    tool_mode: &str,
    model: &str,
) -> Result<()> {
    info!(agent_id = %agent_id, "Starting agent execution");

    let client = TemperClient::new(temper_url, tenant);

    // Fetch agent entity to resolve AgentType.
    let agent = client.get("Agents", agent_id).await?;
    let (system_prompt, resolved_tool_mode, resolved_model) =
        resolve_agent_type(&client, &agent, tool_mode, model).await;

    let provider = AnthropicProvider::new(&resolved_model)?;

    let tools: Box<dyn temper_agent_runtime::ToolRegistry> = match resolved_tool_mode.as_str() {
        "temper" => Box::new(TemperToolRegistry::new(TemperClient::new(
            temper_url, tenant,
        ))),
        _ => Box::new(LocalToolRegistry::new(TemperClient::new(
            temper_url, tenant,
        ))),
    };

    let principal_id = std::sync::Arc::new(std::sync::Mutex::new(Some(agent_id.to_string())));
    let runner = AgentRunner::new(client, Box::new(provider), tools, principal_id);
    runner.resume(agent_id, &system_prompt).await?;

    info!(agent_id = %agent_id, "Agent execution completed");
    Ok(())
}
