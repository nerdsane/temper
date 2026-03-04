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
You are a Temper agent with full access to the local filesystem and shell. \
You MUST use the `execute_code` tool to accomplish tasks — never say you can't access files. \
Always act first by exploring with tools, then report what you found or did.

IMPORTANT: When the user asks you to do something, call `execute_code` immediately. \
Do NOT ask clarifying questions unless truly ambiguous. Explore the filesystem to find what you need.

Available namespaces inside execute_code:

Entity methods:
- `await temper.list(\"EntityType\")` — list entities
- `await temper.get(\"EntityType\", \"id\")` — get entity
- `await temper.create(\"EntityType\", {\"field\": \"value\"})` — create entity
- `await temper.action(\"EntityType\", \"id\", \"ActionName\", {})` — invoke action
- `await temper.patch(\"EntityType\", \"id\", {\"field\": \"new_value\"})` — patch entity

Navigation:
- `await temper.navigate(\"path\")` — navigate OData path
- `await temper.navigate(\"path\", '{\"key\": \"value\"}')` — navigate with params

Developer methods:
- `await temper.submit_specs({\"File.ioa.toml\": \"...\"})` — load specs
- `await temper.get_policies()` — list Cedar policies

WASM integration:
- `await temper.upload_wasm(\"module_name\", \"/path/to/module.wasm\")` — upload WASM module
- `await temper.compile_wasm(\"module_name\", \"rust source code\")` — compile and upload WASM

Governance:
- `await temper.get_decisions()` — list governance decisions
- `await temper.get_decision_status(\"PD-xxx\")` — get single decision status
- `await temper.poll_decision(\"PD-xxx\")` — wait for decision

Evolution observability:
- `await temper.get_trajectories()` — list trajectory spans
- `await temper.get_insights()` — get evolution insights
- `await temper.get_evolution_records()` — list O/P/A/D/I records
- `await temper.check_sentinel()` — trigger sentinel check

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

    let resolved = resolve_provider(provider, model);
    let llm: Box<dyn LlmProvider> = match resolved {
        "openai-codex" => Box::new(CodexProvider::new(model)?),
        _ => {
            // Try Anthropic first; if no API key, fall back to OpenAI if logged in.
            match AnthropicProvider::new(model) {
                Ok(p) => Box::new(p),
                Err(_) if provider.is_none() => {
                    use temper_agent_runtime::providers::codex::auth;
                    if auth::load_credentials()?.is_some() {
                        let openai_model = if model.starts_with("claude") {
                            "gpt-5.3-codex"
                        } else {
                            model
                        };
                        eprintln!("  ANTHROPIC_API_KEY not set — using OpenAI (temper login openai)");
                        Box::new(CodexProvider::new(openai_model)?)
                    } else {
                        anyhow::bail!(
                            "No LLM credentials found.\n\n\
                             Either:\n  \
                             - Set ANTHROPIC_API_KEY in your environment, or\n  \
                             - Run `temper login openai` to authenticate with your ChatGPT subscription"
                        );
                    }
                }
                Err(e) => return Err(e),
            }
        }
    };

    let sandbox = AgentSandbox::new(&base_url, tenant, None);
    let principal_handle = sandbox.principal_id_handle();
    let tools = SandboxToolRegistry::new(sandbox);
    let runner = AgentRunner::new(client, llm, Box::new(tools), principal_handle);

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
            Err(rustyline::error::ReadlineError::Interrupted) => break,
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
            Ok(_) => {
                // Response already printed by streaming callback.
                println!();
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
