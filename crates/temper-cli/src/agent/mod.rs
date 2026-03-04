//! `temper agent` — entity-native agent built on Temper entities.
//!
//! Thin wrapper around [`temper_agent_runtime::AgentRunner`]. The agent loop
//! creates and transitions Temper entities (Agent, Plan, Task, ToolCall)
//! while executing tools in the embedded Monty sandbox. All durable state
//! lives on the server. The CLI is stateless — crash and restart with
//! `--agent-id` to resume.
//!
//! When `--goal` is omitted the CLI enters an interactive REPL where the
//! developer types natural-language requests and the agent writes Python
//! code in the sandbox to accomplish them.

pub mod login;

use anyhow::Result;
use temper_agent_runtime::{
    AgentRunner, AgentSandbox, AnthropicProvider, CodexProvider, LlmProvider, SandboxToolRegistry,
};
use temper_sdk::TemperClient;

/// System prompt for the sandbox agent.
const SYSTEM_PROMPT: &str = "\
You are a Temper agent. You interact with the world through Python code execution.

Available namespaces:
- `await temper.list(\"EntityType\")` — list entities
- `await temper.get(\"EntityType\", \"id\")` — get entity
- `await temper.create(\"EntityType\", {\"field\": \"value\"})` — create entity
- `await temper.action(\"EntityType\", \"id\", \"ActionName\", {})` — invoke action
- `await temper.submit_specs({\"File.ioa.toml\": \"...\"})` — load specs
- `await temper.get_decisions()` — list governance decisions
- `await temper.poll_decision(\"PD-xxx\")` — wait for decision

Local methods (Cedar-governed):
- `await tools.bash(\"command\")` — run shell command
- `await tools.read(\"path\")` — read file
- `await tools.write(\"path\", \"content\")` — write file
- `await tools.ls(\"path\")` — list directory

Write Python code to accomplish the user's request. Use `return` to send results back.";

/// Run the `temper agent` command.
///
/// Delegates to [`AgentRunner`] from `temper-agent-runtime`. The CLI provides
/// the LLM provider (auto-detected or explicit) and the sandbox tool registry.
pub async fn run(
    port: u16,
    tenant: &str,
    goal: Option<&str>,
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

    let sandbox = AgentSandbox::new(&base_url, tenant, None);
    let tools = SandboxToolRegistry::new(sandbox);
    let runner = AgentRunner::new(client, llm, Box::new(tools));

    match (agent_id, goal) {
        (Some(id), _) => {
            println!("Resuming agent: {id}");
            runner.resume(&id).await?;
        }
        (None, Some(goal)) => {
            println!("Goal:  {goal}");
            println!("Role:  {role}");
            println!("Model: {model}");
            println!();
            let id = runner.run_with_model(goal, role, model).await?;
            println!("\nAgent completed: {id}");
        }
        (None, None) => {
            run_interactive(&runner, role, model).await?;
        }
    }

    Ok(())
}

/// Interactive REPL loop.
///
/// The developer types natural-language requests, the agent writes Python
/// code in the sandbox and returns results.
async fn run_interactive(runner: &AgentRunner, role: &str, model: &str) -> Result<()> {
    use rustyline::DefaultEditor;
    use temper_agent_runtime::providers::Message;

    println!("Temper Agent — interactive mode");
    println!("Model: {model}  Role: {role}");
    println!("Type your request. Ctrl+D to exit.\n");

    let mut rl = DefaultEditor::new()?;

    // Try to load history (ignore errors — file may not exist yet).
    let history_dir = dirs::home_dir().unwrap_or_default().join(".temper");
    std::fs::create_dir_all(&history_dir).ok();
    let history_path = history_dir.join("agent-history.txt");
    let _ = rl.load_history(&history_path);

    // Create the agent entity on the server.
    let agent_id = runner.create_interactive_agent(role, model).await?;
    println!("Agent: {agent_id}\n");

    let mut messages: Vec<Message> = Vec::new();

    loop {
        let line = match rl.readline("temper> ") {
            Ok(line) => line,
            Err(rustyline::error::ReadlineError::Eof) => break,
            Err(rustyline::error::ReadlineError::Interrupted) => continue,
            Err(e) => return Err(e.into()),
        };

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        rl.add_history_entry(&line)?;

        match runner
            .run_turn(&agent_id, SYSTEM_PROMPT, trimmed, &mut messages)
            .await
        {
            Ok(response) => {
                println!("\n{response}\n");
            }
            Err(e) => {
                eprintln!("Error: {e}\n");
            }
        }
    }

    // Save history and complete the agent.
    let _ = rl.save_history(&history_path);
    runner.complete_agent(&agent_id).await.ok();
    println!("\nAgent completed: {agent_id}");

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
        return "openai-codex";
    }
    "anthropic"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_provider_explicit() {
        assert_eq!(
            resolve_provider(Some("openai-codex"), "anything"),
            "openai-codex"
        );
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
        assert_eq!(
            resolve_provider(None, "claude-sonnet-4-20250514"),
            "anthropic"
        );
        assert_eq!(
            resolve_provider(None, "claude-opus-4-20250514"),
            "anthropic"
        );
    }
}
