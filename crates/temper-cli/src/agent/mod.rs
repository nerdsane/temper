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
pub mod ui;

use std::sync::Arc;

use anyhow::Result;
use temper_agent_runtime::{
    AgentEvent, AgentRunner, AgentSandbox, AnthropicProvider, CodexProvider, GovernanceContext,
    GovernanceEvent, LlmProvider, SandboxToolRegistry,
};
use temper_sdk::TemperClient;

use self::ui::AgentUI;

/// System prompt for the sandbox agent.
const SYSTEM_PROMPT: &str = "\
== WHAT YOU ARE ==

You are a Temper agent — a developer-operator running inside the Temper platform. \
Temper is an operating layer for governed applications, like an OS for agents. \
Every action you take flows through a governed, verified, auditable pipeline. \
You are NOT a generic coding assistant. You build and operate applications by \
generating specs, not by writing arbitrary code.

== HOW TEMPER WORKS ==

1. You describe what you want as IOA specs (I/O Automaton TOML) — entities with \
   states, transitions, guards, and integrations.
2. Temper verifies specs through a 4-level cascade (SMT, model checking, DST, \
   property tests) before deployment.
3. Verified specs become live entity actors with an OData API.
4. All state changes are auditable transitions — no silent mutations.

The human developer holds the approval gate. Cedar policies define what you can \
and cannot do. If an action is denied, it surfaces to the human for approval. \
This is by design — governance, not limitation.

== MODE OF OPERATION ==

When given a goal, ACT on it immediately using execute_code. Do not ask questions. \
Do not ask for confirmation. Do not say \"if you want\" or \"say proceed\". \
Make reasonable defaults and DO THE WORK. Every response MUST include at least one \
execute_code tool call. If you just returned text without calling a tool, you failed. \
Start by discovering what exists, then design specs, submit them, create entities, \
and run the full flow — all in one session, all through execute_code. \
If something is denied by governance, that is expected — the system will handle \
approval. Keep going with what you can do.

== CRITICAL RULE: NO FABRICATION ==

NEVER fabricate, hallucinate, or invent data. If you cannot obtain real data \
through the platform, say so honestly. If a tool call fails, report the failure \
and propose the correct path — do not present made-up results as real.

== CRITICAL RULE: EXTERNAL ACCESS ==

You have NO direct network access. The sandbox cannot reach external systems. \
tools.bash() runs on the local machine for file operations only.

ALL external access (APIs, HTTP, databases, services) MUST go through \
[[integration]] sections in IOA specs. This is how governance works — every \
external interaction is declared in the spec, verified, Cedar-gated, and auditable.

Two options for integrations:
  1. Built-in http_fetch module — for simple HTTP (GET/POST with URL + headers)
  2. Custom WASM module — any logic, compiled via temper.compile_wasm()

When the user asks for something that needs external data (weather, APIs, etc.):
  1. Generate an IOA spec with the entity, states, and [[integration]] section
  2. Submit via temper.submit_specs()
  3. Create the entity and invoke the triggering action
  4. The integration runs governed — results flow back through state transitions
  DO NOT attempt curl, wget, urllib, requests, or any direct network call. \
  DO NOT retry failed network attempts with different tools. \
  DO NOT fabricate results when external access fails.

== TOOL REFERENCE ==

Use the `execute_code` tool to run Python code in the sandbox.

Entity methods:
- `await temper.list(\"EntityType\")` — list entities
- `await temper.get(\"EntityType\", \"id\")` — get entity
- `await temper.create(\"EntityType\", {\"field\": \"value\"})` — create entity
- `await temper.action(\"EntityType\", \"id\", \"ActionName\", {})` — invoke action
- `await temper.patch(\"EntityType\", \"id\", {\"field\": \"new_value\"})` — patch entity

Navigation:
- `await temper.navigate(\"path\")` — navigate OData path
- `await temper.navigate(\"path\", '{\"key\": \"value\"}')` — navigate with params

Spec and policy management:
- `await temper.submit_specs({\"Entity.ioa.toml\": \"...\", \"model.csdl.xml\": \"...\"})` — load specs \
  IMPORTANT: You MUST include both the IOA spec AND a model.csdl.xml (OData CSDL) in \
  every submit_specs call. The CSDL defines EntitySets and EntityTypes that map to your \
  IOA entities. Without it the server returns \"CSDL model not found\" and entities \
  cannot be created.
- `await temper.get_policies()` — list Cedar policies

WASM integration:
- `await temper.upload_wasm(\"module_name\", \"/path/to/module.wasm\")` — upload WASM
- `await temper.compile_wasm(\"module_name\", \"rust source\")` — compile and upload WASM

Governance:
- `await temper.get_decisions()` — list governance decisions
- `await temper.get_decision_status(\"PD-xxx\")` — get decision status
- `await temper.poll_decision(\"PD-xxx\")` — wait for decision

Evolution observability:
- `await temper.get_trajectories()` — list trajectory spans
- `await temper.get_insights()` — get evolution insights
- `await temper.get_evolution_records()` — list O/P/A/D/I records
- `await temper.check_sentinel()` — trigger sentinel check

Local methods (Cedar-governed, local filesystem only):
- `await tools.bash(\"command\")` — local shell (files, compilation, grep — NO network)
- `await tools.read(\"path\")` — read file
- `await tools.write(\"path\", \"content\")` — write file
- `await tools.ls(\"path\")` — list directory

Write Python code to accomplish the user's request. Use `return` to send results back.";

/// Run the `temper agent` command.
///
/// Delegates to [`AgentRunner`] from `temper-agent-runtime`. The CLI provides
/// the LLM provider (auto-detected or explicit) and the sandbox tool registry.
/// All output goes through [`AgentUI`] for styled terminal output.
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
    let ui = Arc::new(AgentUI::new());

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
                        ui.print_provider_fallback(
                            "ANTHROPIC_API_KEY not set — using OpenAI (temper login openai)",
                        );
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

    // Build governance context with typed event callback + inline resolver.
    let gov_handler = ui.make_event_handler();
    let governance_cb: temper_agent_runtime::GovernanceCallback =
        Arc::new(move |event: GovernanceEvent| {
            let agent_event = match event {
                GovernanceEvent::Allowed {
                    action,
                    resource_id,
                } => AgentEvent::GovernanceAllowed {
                    action,
                    resource: resource_id,
                },
                GovernanceEvent::Waiting {
                    decision_id,
                    action,
                    ..
                } => AgentEvent::GovernanceWait {
                    decision_id,
                    action,
                },
                GovernanceEvent::Resolved { approved, .. } => {
                    AgentEvent::GovernanceResolved { approved }
                }
            };
            gov_handler(agent_event);
        });

    let governance = GovernanceContext {
        on_event: governance_cb,
        resolver: Some(ui.make_governance_resolver()),
    };
    let sandbox = AgentSandbox::new(&base_url, tenant, None).with_governance(governance);
    let principal_handle = sandbox.principal_id_handle();
    let tools = SandboxToolRegistry::new(sandbox);

    let runner = AgentRunner::new(client, llm, Box::new(tools), principal_handle)
        .with_on_delta(ui.make_on_delta())
        .with_on_event(ui.make_event_handler());

    match (agent_id, goal) {
        (Some(id), _) => {
            ui.print_resuming(&id);
            runner.resume(&id, SYSTEM_PROMPT).await?;
        }
        (None, Some(goal)) => {
            ui.print_goal_info(goal, role, model);
            let id = runner.create_agent(role, goal, model).await?;
            let mut messages = Vec::new();
            let result = runner
                .send_autonomous(&id, SYSTEM_PROMPT, goal, &mut messages, 200)
                .await?;
            let truncated: String = result.chars().take(2000).collect();
            runner.complete_agent(&id, &truncated).await.ok();
            ui.print_completed(&id);
        }
        (None, None) => {
            run_interactive(&runner, &ui, role, model).await?;
        }
    }

    Ok(())
}

/// Interactive REPL loop.
///
/// The developer types natural-language requests, the agent writes Python
/// code in the sandbox and returns results. All output goes through [`AgentUI`].
async fn run_interactive(
    runner: &AgentRunner,
    ui: &Arc<AgentUI>,
    role: &str,
    model: &str,
) -> Result<()> {
    use rustyline::DefaultEditor;
    use temper_agent_runtime::providers::Message;

    ui.print_banner(model, role);

    let mut rl = DefaultEditor::new()?;

    // Try to load history (ignore errors — file may not exist yet).
    let history_dir = dirs::home_dir().unwrap_or_default().join(".temper");
    std::fs::create_dir_all(&history_dir).ok();
    let history_path = history_dir.join("agent-history.txt");
    let _ = rl.load_history(&history_path);

    // Create the agent entity on the server — same as goal mode.
    let agent_id = runner
        .create_agent(role, "interactive session", model)
        .await?;
    ui.print_agent_id(&agent_id);

    let mut messages: Vec<Message> = Vec::new();
    let prompt = ui.prompt_string();

    loop {
        let line = match rl.readline(&prompt) {
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
            .send(&agent_id, SYSTEM_PROMPT, trimmed, &mut messages, 30)
            .await
        {
            Ok(_) => {
                // Response already printed by streaming callback.
                println!();
            }
            Err(e) => {
                ui.print_error(&format!("{e}"));
                println!();
            }
        }
    }

    // Save history and complete the agent.
    let _ = rl.save_history(&history_path);
    runner
        .complete_agent(&agent_id, "interactive session ended")
        .await
        .ok();
    ui.print_completed(&agent_id);

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
