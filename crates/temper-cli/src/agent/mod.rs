//! `temper agent` — entity-native agent built on Temper entities.
//!
//! Thin wrapper around [`temper_agent_runtime::AgentRunner`]. The agent loop
//! creates and transitions Temper entities (Agent, Plan, Task, ToolCall)
//! while executing tools locally. All durable state lives on the server.
//! The CLI is stateless — crash and restart with `--agent-id` to resume.

pub mod login;

use anyhow::Result;
use temper_agent_runtime::{AgentRunner, AnthropicProvider, CodexProvider, LlmProvider, LocalToolRegistry};
use temper_sdk::TemperClient;

/// Run the `temper agent` command.
///
/// Delegates to [`AgentRunner`] from `temper-agent-runtime`. The CLI provides
/// the LLM provider (auto-detected or explicit) and local tool registry.
pub async fn run(
    port: u16,
    tenant: &str,
    goal: &str,
    role: &str,
    model: &str,
    agent_id: Option<String>,
    provider: Option<&str>,
) -> Result<()> {
    let base_url = format!("http://127.0.0.1:{port}");
    let client = TemperClient::new(&base_url, tenant);

    let llm: Box<dyn LlmProvider> = match resolve_provider(provider, model) {
        "openai-codex" => Box::new(CodexProvider::new(model)?),
        _ => Box::new(AnthropicProvider::new(model)?),
    };

    let tools = LocalToolRegistry::new(TemperClient::new(&base_url, tenant));
    let runner = AgentRunner::new(client, llm, Box::new(tools));

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

/// Resolve which LLM provider to use based on explicit flag or model name.
fn resolve_provider<'a>(explicit: Option<&'a str>, model: &str) -> &'a str {
    if let Some(p) = explicit {
        return p;
    }
    if model.starts_with("gpt-")
        || model.starts_with("o1")
        || model.starts_with("o3")
        || model.starts_with("o4")
    {
        // Leak a static str for the auto-detected case.
        // This is fine — it's a CLI binary, not a library.
        return "openai-codex";
    }
    "anthropic"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_provider_explicit() {
        assert_eq!(resolve_provider(Some("openai-codex"), "anything"), "openai-codex");
        assert_eq!(resolve_provider(Some("anthropic"), "gpt-4o"), "anthropic");
    }

    #[test]
    fn test_resolve_provider_auto_detect_openai() {
        assert_eq!(resolve_provider(None, "gpt-5.1-codex"), "openai-codex");
        assert_eq!(resolve_provider(None, "gpt-4o"), "openai-codex");
        assert_eq!(resolve_provider(None, "o1-preview"), "openai-codex");
        assert_eq!(resolve_provider(None, "o3-mini"), "openai-codex");
        assert_eq!(resolve_provider(None, "o4-mini"), "openai-codex");
    }

    #[test]
    fn test_resolve_provider_auto_detect_anthropic() {
        assert_eq!(resolve_provider(None, "claude-sonnet-4-20250514"), "anthropic");
        assert_eq!(resolve_provider(None, "claude-opus-4-20250514"), "anthropic");
    }
}
