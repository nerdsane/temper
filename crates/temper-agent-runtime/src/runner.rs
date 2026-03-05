//! Core agent execution loop.
//!
//! [`AgentRunner`] wraps an LLM, a tool registry, and a Temper client.
//! The public API is three methods:
//!
//! - [`AgentRunner::create_agent`] — create + start an Agent entity
//! - [`AgentRunner::send`] — push a message, run the tool-call loop, return
//! - [`AgentRunner::complete_agent`] — transition to terminal state
//!
//! Goal mode and interactive mode are the same agent — one sends a goal
//! once, the other sends user messages in a REPL loop. Both use the same
//! sandbox, same system prompt, same code-mode capabilities.

use std::io::Write;
use std::sync::Arc;

use anyhow::Result;
use serde_json::json;
use temper_sdk::TemperClient;
use tracing::{info, warn};

use crate::providers::{ContentBlock, LlmProvider, Message};
use crate::tools::{ToolRegistry, ToolResult};

/// Events emitted by the agent runner for UI rendering.
///
/// The runtime stays UI-free — it fires events and the CLI renders them.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// LLM call started (show thinking spinner).
    LlmCallStart,
    /// LLM call ended with full response text.
    LlmCallEnd { full_text: String },
    /// Tool execution started.
    ToolStart { name: String },
    /// Tool execution ended.
    ToolEnd {
        name: String,
        success: bool,
        duration_ms: u64,
    },
    /// Governance allowed — Cedar policy permitted the action.
    GovernanceAllowed { action: String, resource: String },
    /// Governance wait — human approval needed.
    GovernanceWait { decision_id: String, action: String },
    /// Governance resolved.
    GovernanceResolved { approved: bool },
}

/// Core agent execution runner.
///
/// Wraps a [`TemperClient`], an [`LlmProvider`], and a [`ToolRegistry`]
/// to run the full agent lifecycle against a Temper server.
pub struct AgentRunner {
    client: TemperClient,
    provider: Box<dyn LlmProvider>,
    tools: Box<dyn ToolRegistry>,
    /// Shared handle to set the agent principal ID on the sandbox.
    principal_id: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    /// Callback for streaming text deltas from the LLM.
    on_delta: Arc<dyn Fn(String) + Send + Sync>,
    /// Callback for agent lifecycle events (tool start/end, governance, etc.).
    on_event: Option<Arc<dyn Fn(AgentEvent) + Send + Sync>>,
}

impl AgentRunner {
    /// Create a new agent runner with default (stdout) streaming.
    pub fn new(
        client: TemperClient,
        provider: Box<dyn LlmProvider>,
        tools: Box<dyn ToolRegistry>,
        principal_id: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    ) -> Self {
        Self {
            client,
            provider,
            tools,
            principal_id,
            on_delta: Arc::new(|text| {
                print!("{text}");
                std::io::stdout().flush().ok();
            }),
            on_event: None,
        }
    }

    /// Set the streaming text delta callback.
    pub fn with_on_delta(mut self, f: Arc<dyn Fn(String) + Send + Sync>) -> Self {
        self.on_delta = f;
        self
    }

    /// Set the event callback for UI rendering.
    pub fn with_on_event(mut self, f: Arc<dyn Fn(AgentEvent) + Send + Sync>) -> Self {
        self.on_event = Some(f);
        self
    }

    /// Emit an agent event if a handler is registered.
    fn emit(&self, event: AgentEvent) {
        if let Some(ref handler) = self.on_event {
            handler(event);
        }
    }

    /// Set the agent principal ID (propagates to the sandbox and tools).
    fn bind_agent_id(&self, agent_id: &str) {
        *self.principal_id.lock().unwrap() = Some(agent_id.to_string()); // ci-ok: infallible lock
        self.tools.set_agent_id(agent_id);
    }

    /// Create an Agent entity and transition it to Working.
    ///
    /// Returns the agent ID. Both goal mode and interactive mode use this
    /// — there is no separate "interactive agent" concept.
    pub async fn create_agent(
        &self,
        role: &str,
        goal: &str,
        model: &str,
    ) -> Result<String> {
        let agent_id = uuid::Uuid::now_v7().to_string();
        info!(agent_id = %agent_id, "Creating agent");
        self.client
            .create("Agents", json!({ "id": &agent_id }))
            .await?;
        self.client
            .action(
                "Agents",
                &agent_id,
                "Assign",
                json!({ "role": role, "goal": goal, "model": model }),
            )
            .await?;
        self.client
            .action("Agents", &agent_id, "Start", json!({}))
            .await?;
        self.bind_agent_id(&agent_id);
        Ok(agent_id)
    }

    /// Send a message and run the tool-call loop (interactive mode).
    ///
    /// In interactive mode, the loop exits when the LLM produces a text
    /// response without tool calls — that's the agent's reply to the user.
    pub async fn send(
        &self,
        agent_id: &str,
        system_prompt: &str,
        message: &str,
        messages: &mut Vec<Message>,
        max_turns: usize,
    ) -> Result<String> {
        messages.push(Message {
            role: "user".to_string(),
            content: vec![ContentBlock::Text {
                text: message.to_string(),
            }],
        });
        self.run_loop(agent_id, system_prompt, messages, max_turns, false)
            .await
    }

    /// Send a goal and run the tool-call loop (autonomous mode).
    ///
    /// In autonomous mode, the loop does NOT exit on text-only responses.
    /// The agent must exhaust its turn budget or the caller decides when
    /// to stop. Text-only responses get a continuation prompt injected
    /// so the agent keeps working.
    pub async fn send_autonomous(
        &self,
        agent_id: &str,
        system_prompt: &str,
        goal: &str,
        messages: &mut Vec<Message>,
        max_turns: usize,
    ) -> Result<String> {
        messages.push(Message {
            role: "user".to_string(),
            content: vec![ContentBlock::Text {
                text: goal.to_string(),
            }],
        });
        self.run_loop(agent_id, system_prompt, messages, max_turns, true)
            .await
    }

    /// Resume an existing agent by ID.
    ///
    /// Reads the agent's current state and continues the tool-call loop
    /// from where it left off.
    pub async fn resume(&self, agent_id: &str, system_prompt: &str) -> Result<()> {
        info!(agent_id = %agent_id, "Resuming agent");

        let agent = self.client.get("Agents", agent_id).await?;
        let status = agent.get("status").and_then(|v| v.as_str()).unwrap_or("");
        let goal = entity_field(&agent, "goal")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        info!(status = %status, "Agent status");

        if status == "Completed" || status == "Failed" {
            info!("Agent already in terminal state: {status}");
            return Ok(());
        }

        self.bind_agent_id(agent_id);
        let mut messages = Vec::new();
        let result = self
            .send_autonomous(agent_id, system_prompt, goal, &mut messages, 200)
            .await?;

        let truncated: String = result.chars().take(2000).collect();
        self.complete_with_retry(agent_id, &truncated).await;
        Ok(())
    }

    /// Complete an agent (transition to terminal state).
    pub async fn complete_agent(&self, agent_id: &str, result: &str) -> Result<()> {
        self.client
            .action(
                "Agents",
                agent_id,
                "Complete",
                json!({ "result": result }),
            )
            .await?;
        Ok(())
    }

    /// Complete the agent, retrying if cross-entity guard blocks (children not finished).
    ///
    /// The Agent.Complete action has a cross-entity guard requiring all child agents
    /// to be in Completed or Failed state. If children are still running, this retries
    /// with 5-second intervals up to a budget of 60 retries (5 minutes).
    async fn complete_with_retry(&self, agent_id: &str, result: &str) {
        const MAX_RETRIES: u32 = 60;
        const RETRY_INTERVAL_SECS: u64 = 5;

        for attempt in 0..=MAX_RETRIES {
            match self
                .client
                .action(
                    "Agents",
                    agent_id,
                    "Complete",
                    json!({ "result": result }),
                )
                .await
            {
                Ok(_) => {
                    if attempt > 0 {
                        info!(
                            agent_id = %agent_id,
                            attempts = attempt + 1,
                            "Agent.Complete succeeded after retries"
                        );
                    }
                    return;
                }
                Err(e) => {
                    let err_str = e.to_string();
                    let is_guard_failure = err_str.contains("guard")
                        || err_str.contains("cross_entity")
                        || err_str.contains("precondition");
                    if !is_guard_failure || attempt == MAX_RETRIES {
                        warn!(
                            agent_id = %agent_id,
                            attempt = attempt + 1,
                            "Agent.Complete failed: {e}"
                        );
                        return;
                    }
                    info!(
                        agent_id = %agent_id,
                        attempt = attempt + 1,
                        "Agent.Complete blocked (children not finished), retrying in {RETRY_INTERVAL_SECS}s"
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(RETRY_INTERVAL_SECS)).await;
                }
            }
        }
    }

    // ── Internal helpers ────────────────────────────────────────────────

    /// Run the LLM tool-call loop.
    ///
    /// Sends messages to the LLM with available tools. When the LLM calls
    /// tools, executes them through Cedar authorization and returns results.
    /// Continues until the LLM produces a text response (end_turn) or the
    /// turn budget is exhausted.
    async fn run_loop(
        &self,
        agent_id: &str,
        system_prompt: &str,
        messages: &mut Vec<Message>,
        max_turns: usize,
        autonomous: bool,
    ) -> Result<String> {
        let tool_defs = self.tools.list_tools();
        let tool_schemas: Vec<serde_json::Value> = tool_defs
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.input_schema,
                })
            })
            .collect();
        for turn in 0..max_turns {
            self.emit(AgentEvent::LlmCallStart);
            let on_delta = self.on_delta.clone();
            let response = self
                .provider
                .send_streaming(
                    system_prompt,
                    messages,
                    &tool_schemas,
                    Box::new(move |text| on_delta(text)),
                )
                .await?;

            info!(
                stop_reason = %response.stop_reason,
                content_blocks = response.content.len(),
                "LLM response received"
            );
            for (i, block) in response.content.iter().enumerate() {
                match block {
                    ContentBlock::Text { text } => {
                        info!(block = i, len = text.len(), "  text block");
                    }
                    ContentBlock::ToolUse { id, name, .. } => {
                        info!(block = i, tool = %name, id = %id, "  tool_use block");
                    }
                    ContentBlock::ToolResult { tool_use_id, .. } => {
                        info!(block = i, id = %tool_use_id, "  tool_result block");
                    }
                }
            }

            // Handle empty LLM response — inject continuation prompt instead of exiting.
            if response.content.is_empty() {
                warn!("LLM returned empty content array — injecting continuation");
                self.emit(AgentEvent::LlmCallEnd {
                    full_text: String::new(),
                });
                messages.push(Message {
                    role: "user".to_string(),
                    content: vec![ContentBlock::Text {
                        text: "Continue. Use execute_code to proceed with the task.".to_string(),
                    }],
                });
                continue;
            }

            messages.push(Message {
                role: "assistant".to_string(),
                content: response.content.clone(),
            });

            let tool_uses: Vec<_> = response
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolUse { id, name, input } => {
                        Some((id.clone(), name.clone(), input.clone()))
                    }
                    _ => None,
                })
                .collect();

            if tool_uses.is_empty() {
                let result_text = response
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");

                self.emit(AgentEvent::LlmCallEnd {
                    full_text: result_text.clone(),
                });

                if autonomous {
                    // In autonomous mode, text-only is not an exit — keep working.
                    info!("Autonomous mode: text-only response, injecting continuation");
                    messages.push(Message {
                        role: "user".to_string(),
                        content: vec![ContentBlock::Text {
                            text: "You returned text without calling execute_code. \
                                   Use execute_code now to continue working on the goal."
                                .to_string(),
                        }],
                    });
                    continue;
                }

                self.checkpoint(
                    agent_id,
                    &serde_json::to_string(messages).unwrap_or_default(),
                )
                .await;

                return Ok(result_text);
            }

            self.emit(AgentEvent::LlmCallEnd {
                full_text: String::new(),
            });

            // Execute tool calls.
            let mut tool_results = Vec::new();
            for (tool_use_id, tool_name, tool_input) in &tool_uses {
                self.emit(AgentEvent::ToolStart {
                    name: tool_name.clone(),
                });
                info!(tool = %tool_name, "Executing tool");

                let start_time = std::time::Instant::now(); // determinism-ok: CLI timing
                let result = self.execute_tool(agent_id, tool_name, tool_input).await?;
                let tool_duration = start_time.elapsed().as_millis() as u64;

                let (success, content, is_error) = match result {
                    ToolResult::Success(output) => (true, output, None),
                    ToolResult::Error(err) => (false, format!("Error: {err}"), Some(true)),
                };
                self.emit(AgentEvent::ToolEnd {
                    name: tool_name.clone(),
                    success,
                    duration_ms: tool_duration,
                });
                tool_results.push(ContentBlock::ToolResult {
                    tool_use_id: tool_use_id.clone(),
                    content,
                    is_error,
                });
            }

            messages.push(Message {
                role: "user".to_string(),
                content: tool_results,
            });

            // Checkpoint after each turn.
            self.checkpoint(
                agent_id,
                &serde_json::to_string(messages).unwrap_or_default(),
            )
            .await;

            if turn == max_turns - 1 {
                return Ok("Max turns reached".to_string());
            }
        }

        Ok("Max turns reached".to_string())
    }

    /// Execute a single tool call through Cedar authorization + tool registry.
    async fn execute_tool(
        &self,
        agent_id: &str,
        tool_name: &str,
        tool_input: &serde_json::Value,
    ) -> Result<ToolResult> {
        let cedar = self.tools.to_cedar(tool_name, tool_input);

        // Create ToolCall entity.
        let tc_id = uuid::Uuid::now_v7().to_string();
        self.client
            .create(
                "ToolCalls",
                json!({
                    "id": tc_id,
                    "agent_id": agent_id,
                    "tool_name": tool_name,
                    "tool_input": serde_json::to_string(tool_input).unwrap_or_default(),
                    "resource_type": &cedar.resource_type,
                    "resource_id": &cedar.resource_id,
                }),
            )
            .await?;

        // Cedar authorization.
        let authz = self
            .client
            .authorize(
                agent_id,
                &cedar.action,
                &cedar.resource_type,
                &cedar.resource_id,
            )
            .await?;

        if !authz.allowed {
            let decision_id = authz.decision_id.unwrap_or_default();
            self.client
                .action(
                    "ToolCalls",
                    &tc_id,
                    "Deny",
                    json!({ "decision_id": decision_id }),
                )
                .await?;
            return Ok(ToolResult::Error(format!(
                "Tool '{tool_name}' denied by Cedar policy. Decision: {decision_id}"
            )));
        }

        // Governance events are emitted by the sandbox dispatch layer via GovernanceContext.
        self.client
            .action("ToolCalls", &tc_id, "Authorize", json!({}))
            .await?;

        // Execute.
        self.client
            .action("ToolCalls", &tc_id, "Execute", json!({}))
            .await?;

        let start = std::time::Instant::now(); // determinism-ok: executor code, not simulation-visible
        let result = self.tools.execute(tool_name, tool_input.clone()).await;
        let duration_ms = start.elapsed().as_millis() as u64;

        match result {
            Ok(ToolResult::Success(output)) => {
                let truncated = if output.len() > 4096 {
                    let boundary = output
                        .char_indices()
                        .take_while(|(i, _)| *i < 4096)
                        .last()
                        .map(|(i, c)| i + c.len_utf8())
                        .unwrap_or(0);
                    format!("{}...(truncated)", &output[..boundary])
                } else {
                    output.clone()
                };
                self.client
                    .action(
                        "ToolCalls",
                        &tc_id,
                        "Complete",
                        json!({ "result": truncated, "duration_ms": duration_ms.to_string() }),
                    )
                    .await?;
                Ok(ToolResult::Success(output))
            }
            Ok(ToolResult::Error(err)) => {
                self.client
                    .action("ToolCalls", &tc_id, "Fail", json!({ "error": &err }))
                    .await?;
                Ok(ToolResult::Error(err))
            }
            Err(e) => {
                self.client
                    .action(
                        "ToolCalls",
                        &tc_id,
                        "Fail",
                        json!({ "error": e.to_string() }),
                    )
                    .await?;
                Ok(ToolResult::Error(e.to_string()))
            }
        }
    }

    /// Checkpoint the agent's conversation state.
    async fn checkpoint(&self, agent_id: &str, conversation: &str) {
        if let Err(e) = self
            .client
            .action(
                "Agents",
                agent_id,
                "Checkpoint",
                json!({ "conversation": conversation }),
            )
            .await
        {
            warn!("Checkpoint failed: {e}");
        }
    }
}

/// Resolve a property from an OData entity response.
///
/// Checks the top-level object first, then falls back to the `fields`
/// sub-object — matching the resolution strategy used by the server's
/// `query_eval::resolve_property`.
fn entity_field<'a>(entity: &'a serde_json::Value, key: &str) -> Option<&'a serde_json::Value> {
    entity
        .get(key)
        .or_else(|| entity.get("fields").and_then(|f| f.get(key)))
}
