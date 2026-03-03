//! `temper agent` — entity-native agent built on Temper entities.
//!
//! Thin wrapper around [`temper_agent_runtime::AgentRunner`]. The agent loop
//! creates and transitions Temper entities (Agent, Plan, Task, ToolCall)
//! while executing tools locally. All durable state lives on the server.
//! The CLI is stateless — crash and restart with `--agent-id` to resume.

use anyhow::Result;
use temper_agent_runtime::{AgentRunner, AnthropicProvider, LocalToolRegistry};
use temper_sdk::TemperClient;

/// Run the `temper agent` command.
///
/// Delegates to [`AgentRunner`] from `temper-agent-runtime`. The CLI provides
/// the Anthropic LLM provider and local tool registry (file I/O + shell + entities).
pub async fn run(
    port: u16,
    tenant: &str,
    goal: &str,
    role: &str,
    model: &str,
    agent_id: Option<String>,
) -> Result<()> {
    let base_url = format!("http://127.0.0.1:{port}");
    let client = TemperClient::new(&base_url, tenant);
    let provider = AnthropicProvider::new(model)?;
    let tools = LocalToolRegistry::new(TemperClient::new(&base_url, tenant));
    let runner = AgentRunner::new(client, Box::new(provider), Box::new(tools));

    match agent_id {
        Some(id) => {
            println!("Resuming agent: {id}");
            runner.resume(&id).await?;
        }
        None => {
            println!("Goal:  {goal}");
            println!("Role:  {role}");
            println!("Model: {model}");
            println!();
            let id = runner.run_with_model(goal, role, model).await?;
            println!("\nAgent completed: {id}");
        }
    }

    Ok(())
}
