//! The Crucible Phase 4 turn loop.
//!
//! Given a `session_id` and optionally a new user message, this
//! module drives exactly one turn of a chat: read the session +
//! agent, reconstruct history from the SessionEvent feed, call the
//! model, and write the reply back as new events plus the matching
//! Session bound actions.
//!
//! The loop is **stateless**. Every invocation re-reads the full
//! SessionEvent history from scratch, so a crashed or interrupted
//! turn can be safely re-run — the next invocation will observe
//! whichever events already landed and advance from there. Event
//! ids are derived from the session id + sequence number, so replay
//! either succeeds trivially (idempotent) or 409s on the duplicate
//! id (the correct resume signal).
//!
//! The 12 steps below match the plan in
//! `reference-apps/crucible/PHASE_4_PLAN.md` §"The turn loop".

use crate::chat::anthropic::{
    ChatMessage, ContentBlock, MessagesRequest, MessagesResponse, Model, ToolDefinition,
};
use crate::chat::temper_client::{
    AgentToolRow, CallableAgentRow, SessionAction, SessionEventRow, SessionRow,
    SessionThreadRow, TemperClient, ThreadAction,
};
use crate::chat::tools;
use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

/// The default `max_tokens` Phase 4 asks Anthropic for. Intentionally
/// hardcoded — see ADR-0046 Sub-Decision 4 for why we did not add a
/// `MaxTokens` column to `ManagedAgent`.
const DEFAULT_MAX_TOKENS: u32 = 1024;

/// Cap on how many events we will load per turn. The append-only
/// feed is in practice small for chat sessions, but we want an
/// explicit bound so a pathological session cannot hang the CLI.
const EVENT_HISTORY_LIMIT: u32 = 500;

/// Maximum iterations of the tool loop before forcing a stop. This
/// prevents infinite tool chains from running away.
const MAX_TOOL_ITERATIONS: u32 = 25;

/// Default tool server URL when `CRUCIBLE_TOOL_SERVER_URL` is set.
const DEFAULT_TOOL_SERVER_URL: &str = "http://127.0.0.1:3100";

/// Tool execution routing: Local runs tools in-process; Remote
/// routes through the Python tool server (for Modal environments).
#[derive(Debug, Clone)]
enum ToolRouter {
    Local,
    Remote {
        tool_server_url: String,
        environment_id: String,
    },
}

#[derive(Debug, Clone)]
pub struct RespondRequest<'a> {
    pub session_id: &'a str,
    /// If `Some`, the responder appends this text as a new
    /// `user.message` event before calling the model. If `None`,
    /// the loop reacts to whatever user events already sit at the
    /// tail of the feed (the `respond` CLI subcommand path).
    pub new_user_message: Option<&'a str>,
}

#[derive(Debug, Clone)]
pub struct RespondOutcome {
    pub assistant_text: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    /// The sequence number of the `agent.message` event this turn
    /// emitted. Exposed so the caller (and tests) can correlate.
    pub agent_message_sequence: i64,
}

/// Run one turn of the chat loop against the supplied Temper instance
/// and model provider. Returns the assistant's text plus the token
/// counts observed from the model.
pub async fn respond<M: Model>(
    temper: &TemperClient,
    model: &M,
    req: RespondRequest<'_>,
) -> Result<RespondOutcome> {
    // ------------------------------------------------------------------
    // 1. Fetch parents.
    // ------------------------------------------------------------------
    let session = temper
        .get_session(req.session_id)
        .await
        .with_context(|| format!("loading Session('{}')", req.session_id))?;
    if session.status == "Archived" || session.status == "Terminated" {
        return Err(anyhow!(
            "session {} is in terminal status {}; cannot respond",
            req.session_id,
            session.status
        ));
    }
    let agent = temper
        .get_managed_agent(&session.agent_id)
        .await
        .with_context(|| format!("loading ManagedAgent('{}')", session.agent_id))?;

    // ------------------------------------------------------------------
    // 1b. Determine tool routing from environment ConfigType.
    // ------------------------------------------------------------------
    let env_result = temper.get_environment(&session.environment_id).await;
    match &env_result {
        Ok(env) => eprintln!("[respond] Environment {} ConfigType={}", env.id, env.config_type),
        Err(e) => eprintln!("[respond] Failed to get environment {}: {}", session.environment_id, e),
    }
    let tool_router = match env_result {
        Ok(env) if env.config_type == "Modal" => {
            let url = std::env::var("CRUCIBLE_TOOL_SERVER_URL")
                .unwrap_or_else(|_| DEFAULT_TOOL_SERVER_URL.to_string());
            eprintln!(
                "[respond] Modal environment detected ({}), routing tools via {}",
                env.id, url
            );
            ToolRouter::Remote {
                tool_server_url: url,
                environment_id: env.id,
            }
        }
        _ => ToolRouter::Local,
    };

    // ------------------------------------------------------------------
    // 2. Fetch history and compute the next sequence.
    // ------------------------------------------------------------------
    let mut history = temper
        .list_session_events(req.session_id, EVENT_HISTORY_LIMIT)
        .await
        .with_context(|| format!("listing events for session {}", req.session_id))?;
    history.sort_by_key(|e| e.sequence);
    let mut next_sequence = history.last().map(|e| e.sequence + 1).unwrap_or(0);
    let now = now_rfc3339();

    // ------------------------------------------------------------------
    // 3. Append the new user message, if the caller supplied one.
    // ------------------------------------------------------------------
    if let Some(text) = req.new_user_message {
        let row = SessionEventRow {
            id: event_id(req.session_id, next_sequence),
            session_id: req.session_id.to_string(),
            sequence: next_sequence,
            kind: "user.message".to_string(),
            created_at: now.clone(),
            processed_at: Some(now.clone()),
            content: Some(user_message_content(text)),
            ..blank_event()
        };
        temper
            .create_session_event(&row)
            .await
            .context("POSTing user.message event")?;
        history.push(row);
        next_sequence += 1;
    }

    // ------------------------------------------------------------------
    // 4. Drive lifecycle to Running if needed.
    // ------------------------------------------------------------------
    drive_to_running(temper, &session).await?;

    // ------------------------------------------------------------------
    // 4b. Load tools from AgentTool children + callable agents.
    // ------------------------------------------------------------------
    let agent_tools = temper
        .list_agent_tools(&session.agent_id)
        .await
        .unwrap_or_default();
    let mut tool_defs = build_tools_from_agent(&agent_tools);

    // If this agent has callable agents, inject the delegate_to_agent tool.
    let callable_agents = temper
        .list_callable_agents(&session.agent_id)
        .await
        .unwrap_or_default();
    if !callable_agents.is_empty() {
        tool_defs.push(build_delegate_tool_definition(&callable_agents));
    }

    let tools_for_request = if tool_defs.is_empty() {
        None
    } else {
        Some(tool_defs)
    };

    // ------------------------------------------------------------------
    // 5–9. Iterative tool loop.
    //
    // Each iteration: emit model_request_start → call model →
    // if tool_use: emit events + execute tools + continue,
    // if text: emit agent.message → break.
    // ------------------------------------------------------------------
    let mut total_input_tokens: i64 = 0;
    let mut total_output_tokens: i64 = 0;
    let mut assistant_text = String::new();
    let mut agent_message_sequence: i64 = -1;
    // The in-memory messages array, built once and extended in-place
    // as the loop produces assistant + tool_result turns.
    let mut messages = events_to_messages(&history)?;

    for iteration in 0..MAX_TOOL_ITERATIONS {
        // 5. span.model_request_start
        let model_request_start_id = event_id(req.session_id, next_sequence);
        let start_row = SessionEventRow {
            id: model_request_start_id.clone(),
            session_id: req.session_id.to_string(),
            sequence: next_sequence,
            kind: "span.model_request_start".to_string(),
            created_at: now.clone(),
            processed_at: Some(now.clone()),
            content: Some(
                serde_json::json!({ "model": agent.model_id, "started_at": now }).to_string(),
            ),
            model_speed: agent.model_speed.clone(),
            ..blank_event()
        };
        temper
            .create_session_event(&start_row)
            .await
            .context("POSTing span.model_request_start event")?;
        next_sequence += 1;

        // 6. Build and send the request.
        let req_anthropic = MessagesRequest {
            model: agent.model_id.clone(),
            system: agent.system.clone(),
            messages: messages.clone(),
            max_tokens: DEFAULT_MAX_TOKENS,
            tools: tools_for_request.clone(),
        };

        // 7. Call the model.
        let response = model
            .complete(req_anthropic)
            .await
            .context("calling the model provider")?;
        total_input_tokens += response.usage.input_tokens;
        total_output_tokens += response.usage.output_tokens;

        if response.has_tool_use() {
            // ── Tool-use path ──────────────────────────────────────

            // 8a. Emit agent.message with ALL content blocks (text + tool_use).
            let agent_row = SessionEventRow {
                id: event_id(req.session_id, next_sequence),
                session_id: req.session_id.to_string(),
                sequence: next_sequence,
                kind: "agent.message".to_string(),
                created_at: now.clone(),
                processed_at: Some(now.clone()),
                content: Some(content_blocks_to_blob(&response.content)),
                ..blank_event()
            };
            temper
                .create_session_event(&agent_row)
                .await
                .context("POSTing agent.message (tool_use) event")?;
            next_sequence += 1;

            // 8b. Emit one agent.tool_use observability event per tool call.
            for block in &response.content {
                if let ContentBlock::ToolUse { id, name, input } = block {
                    let tu_row = SessionEventRow {
                        id: event_id(req.session_id, next_sequence),
                        session_id: req.session_id.to_string(),
                        sequence: next_sequence,
                        kind: "agent.tool_use".to_string(),
                        created_at: now.clone(),
                        processed_at: Some(now.clone()),
                        content: Some(
                            serde_json::json!({
                                "tool_use_id": id,
                                "name": name,
                                "input": input
                            })
                            .to_string(),
                        ),
                        tool_name: Some(name.clone()),
                        tool_use_id: Some(id.clone()),
                        ..blank_event()
                    };
                    temper
                        .create_session_event(&tu_row)
                        .await
                        .context("POSTing agent.tool_use event")?;
                    next_sequence += 1;
                }
            }

            // Emit span.model_request_end for this iteration.
            emit_model_request_end(
                temper,
                req.session_id,
                &mut next_sequence,
                &now,
                &model_request_start_id,
                &response,
                &agent.model_speed,
            )
            .await?;

            // Add the assistant turn (with tool_use blocks) to messages.
            messages.push(ChatMessage {
                role: "assistant".to_string(),
                content: response.content.clone(),
            });

            // 8c. Execute each tool and collect results.
            let mut tool_result_blocks: Vec<ContentBlock> = Vec::new();
            for block in &response.content {
                if let ContentBlock::ToolUse { id, name, input } = block {
                    let result = if name == "delegate_to_agent" {
                        // Multi-agent delegation path.
                        execute_delegation(
                            temper,
                            model,
                            req.session_id,
                            &callable_agents,
                            input,
                            &mut next_sequence,
                            &now,
                            &tool_router,
                        )
                        .await
                    } else {
                        route_tool_call(&tool_router, temper, name, input).await
                    };

                    // Emit agent.tool_result event.
                    let tr_row = SessionEventRow {
                        id: event_id(req.session_id, next_sequence),
                        session_id: req.session_id.to_string(),
                        sequence: next_sequence,
                        kind: "agent.tool_result".to_string(),
                        created_at: now.clone(),
                        processed_at: Some(now.clone()),
                        content: Some(
                            serde_json::json!({
                                "blocks": [{
                                    "type": "tool_result",
                                    "tool_use_id": id,
                                    "content": result.output,
                                    "is_error": result.is_error,
                                }]
                            })
                            .to_string(),
                        ),
                        tool_use_id: Some(id.clone()),
                        ..blank_event()
                    };
                    temper
                        .create_session_event(&tr_row)
                        .await
                        .context("POSTing agent.tool_result event")?;
                    next_sequence += 1;

                    tool_result_blocks.push(ContentBlock::ToolResult {
                        tool_use_id: id.clone(),
                        content: result.output,
                        is_error: Some(result.is_error),
                    });
                }
            }

            // Add tool results as a user turn for the next iteration.
            messages.push(ChatMessage {
                role: "user".to_string(),
                content: tool_result_blocks,
            });

            // Continue the loop for the next model call.
            continue;
        }

        // ── Text-only path (final iteration) ───────────────────
        assistant_text = response.text();
        if assistant_text.is_empty() && iteration == 0 {
            return Err(anyhow!("model returned an empty response"));
        }

        // 8. Emit agent.message.
        agent_message_sequence = next_sequence;
        let agent_row = SessionEventRow {
            id: event_id(req.session_id, next_sequence),
            session_id: req.session_id.to_string(),
            sequence: next_sequence,
            kind: "agent.message".to_string(),
            created_at: now.clone(),
            processed_at: Some(now.clone()),
            content: Some(agent_message_content(&assistant_text)),
            ..blank_event()
        };
        temper
            .create_session_event(&agent_row)
            .await
            .context("POSTing agent.message event")?;
        next_sequence += 1;

        // 9. Emit span.model_request_end.
        emit_model_request_end(
            temper,
            req.session_id,
            &mut next_sequence,
            &now,
            &model_request_start_id,
            &response,
            &agent.model_speed,
        )
        .await?;

        break;
    }

    // ------------------------------------------------------------------
    // 10. Emit session.status_idle with StopReason=end_turn.
    // ------------------------------------------------------------------
    let idle_row = SessionEventRow {
        id: event_id(req.session_id, next_sequence),
        session_id: req.session_id.to_string(),
        sequence: next_sequence,
        kind: "session.status_idle".to_string(),
        created_at: now.clone(),
        processed_at: Some(now.clone()),
        content: Some("{}".to_string()),
        stop_reason: Some("end_turn".to_string()),
        ..blank_event()
    };
    temper
        .create_session_event(&idle_row)
        .await
        .context("POSTing session.status_idle event")?;

    // ------------------------------------------------------------------
    // 11. Transition Running → Idle.
    // ------------------------------------------------------------------
    temper
        .invoke_session_action(req.session_id, SessionAction::IdleSession)
        .await
        .context("invoking IdleSession")?;

    // ------------------------------------------------------------------
    // 12. Return the outcome.
    // ------------------------------------------------------------------
    Ok(RespondOutcome {
        assistant_text,
        input_tokens: total_input_tokens,
        output_tokens: total_output_tokens,
        agent_message_sequence,
    })
}

/// Push the Session into `Running` if it isn't already. From
/// `Rescheduling` we call `StartSession`; from `Idle` we call
/// `ResumeSession`. From `Running` we do nothing. Any other status
/// (Terminated/Archived) is a hard error — the caller already
/// checked for those at step 1 so this should never fire, but we
/// keep the branch explicit.
async fn drive_to_running(temper: &TemperClient, session: &SessionRow) -> Result<()> {
    match session.status.as_str() {
        "Running" => Ok(()),
        "Rescheduling" => temper
            .invoke_session_action(&session.id, SessionAction::StartSession)
            .await
            .context("invoking StartSession"),
        "Idle" => temper
            .invoke_session_action(&session.id, SessionAction::ResumeSession)
            .await
            .context("invoking ResumeSession"),
        other => Err(anyhow!(
            "cannot drive session {} to Running from status {}",
            session.id,
            other
        )),
    }
}

/// Emit a `span.model_request_end` event. Factored out because both
/// the tool-use and text-only paths need it.
async fn emit_model_request_end(
    temper: &TemperClient,
    session_id: &str,
    next_sequence: &mut i64,
    now: &str,
    model_request_start_id: &str,
    response: &MessagesResponse,
    model_speed: &Option<String>,
) -> Result<()> {
    let end_row = SessionEventRow {
        id: event_id(session_id, *next_sequence),
        session_id: session_id.to_string(),
        sequence: *next_sequence,
        kind: "span.model_request_end".to_string(),
        created_at: now.to_string(),
        processed_at: Some(now.to_string()),
        content: Some(
            serde_json::json!({
                "stop_reason": response.stop_reason.clone().unwrap_or_default()
            })
            .to_string(),
        ),
        model_request_start_id: Some(model_request_start_id.to_string()),
        is_error: Some(false),
        model_input_tokens: Some(response.usage.input_tokens),
        model_output_tokens: Some(response.usage.output_tokens),
        model_cache_creation_input_tokens: Some(response.usage.cache_creation_input_tokens),
        model_cache_read_input_tokens: Some(response.usage.cache_read_input_tokens),
        model_speed: model_speed.clone(),
        ..blank_event()
    };
    temper
        .create_session_event(&end_row)
        .await
        .context("POSTing span.model_request_end event")?;
    *next_sequence += 1;
    Ok(())
}

/// Route a tool call through the appropriate backend based on the
/// environment's `ConfigType`. Local environments run tools in-process;
/// Modal environments route through the Python tool server.
async fn route_tool_call(
    router: &ToolRouter,
    _temper: &TemperClient,
    name: &str,
    args: &serde_json::Value,
) -> tools::ToolResult {
    match router {
        ToolRouter::Local => execute_tool_call_local(name, args).await,
        ToolRouter::Remote {
            tool_server_url,
            environment_id,
        } => tools::execute_tool_remote(tool_server_url, name, args, environment_id).await,
    }
}

/// Execute a tool call locally. Wraps the synchronous
/// `tools::execute_tool` in `spawn_blocking` so bash commands
/// don't block the async runtime.
async fn execute_tool_call_local(name: &str, args: &serde_json::Value) -> tools::ToolResult {
    let name = name.to_string();
    let args = args.clone();
    tokio::task::spawn_blocking(move || tools::execute_tool(&name, &args))
        .await
        .unwrap_or_else(|e| tools::ToolResult {
            output: format!("tool execution panicked: {e}"),
            is_error: true,
        })
}

/// Build the tool definitions list from the agent's `AgentTool` children.
///
/// - `Kind=agent_toolset` → expands to the 6 built-in tool definitions
/// - `Kind=custom` → single tool from Name/Description/InputSchema
/// - No AgentTool children → empty vec (pure chat, no tools)
pub fn build_tools_from_agent(agent_tools: &[AgentToolRow]) -> Vec<ToolDefinition> {
    let mut defs = Vec::new();
    for at in agent_tools {
        match at.kind.as_str() {
            "agent_toolset" => {
                defs.extend(tools::tool_definitions());
            }
            "custom" => {
                let name = at.name.as_deref().unwrap_or("");
                let description = at.description.as_deref().unwrap_or("");
                let input_schema_str = at.input_schema.as_deref().unwrap_or("{}");
                let input_schema: serde_json::Value =
                    serde_json::from_str(input_schema_str).unwrap_or(serde_json::json!({
                        "type": "object",
                        "properties": {}
                    }));
                if !name.is_empty() {
                    defs.push(ToolDefinition {
                        name: name.to_string(),
                        description: description.to_string(),
                        input_schema,
                    });
                }
            }
            _ => continue,
        }
    }
    defs
}

/// Build the `delegate_to_agent` synthetic tool definition. The
/// description lists available agents so the LLM knows what it can
/// delegate to.
fn build_delegate_tool_definition(callable_agents: &[CallableAgentRow]) -> ToolDefinition {
    let agents_desc: Vec<String> = callable_agents
        .iter()
        .map(|ca| ca.callee_agent_id.clone())
        .collect();
    ToolDefinition {
        name: "delegate_to_agent".to_string(),
        description: format!(
            "Delegate a task to a sub-agent. Available agents: {}. \
             The sub-agent runs in its own thread with its own model and system prompt. \
             Returns the sub-agent's response text.",
            agents_desc.join(", ")
        ),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "agent_id": {
                    "type": "string",
                    "description": "ID of the agent to delegate to"
                },
                "message": {
                    "type": "string",
                    "description": "The task/message to send to the sub-agent"
                },
                "thread_id": {
                    "type": "string",
                    "description": "Existing thread ID for follow-up (omit for new thread)"
                }
            },
            "required": ["agent_id", "message"]
        }),
    }
}

/// Handle a `delegate_to_agent` tool call by creating or resuming a
/// thread and running a sub-agent turn on it.
async fn execute_delegation<M: Model>(
    temper: &TemperClient,
    model: &M,
    session_id: &str,
    callable_agents: &[CallableAgentRow],
    input: &serde_json::Value,
    next_sequence: &mut i64,
    now: &str,
    tool_router: &ToolRouter,
) -> tools::ToolResult {
    let agent_id = input
        .get("agent_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let message = input
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let thread_id_opt = input.get("thread_id").and_then(|v| v.as_str());

    // Validate agent_id is in callable_agents.
    if !callable_agents
        .iter()
        .any(|ca| ca.callee_agent_id == agent_id)
    {
        return tools::ToolResult {
            output: format!(
                "Error: agent_id '{}' is not in the callable agents list",
                agent_id
            ),
            is_error: true,
        };
    }

    // Create or resume thread.
    let thread_id = match thread_id_opt {
        Some(tid) if !tid.is_empty() => {
            // Resume existing thread.
            match temper.get_session_thread(tid).await {
                Ok(thread) if thread.status == "Idle" => {
                    if let Err(e) = temper
                        .invoke_thread_action(tid, ThreadAction::ResumeThread)
                        .await
                    {
                        return tools::ToolResult {
                            output: format!("Error resuming thread: {e}"),
                            is_error: true,
                        };
                    }
                }
                Ok(_) => {} // Already Running, fine
                Err(e) => {
                    return tools::ToolResult {
                        output: format!("Error fetching thread: {e}"),
                        is_error: true,
                    };
                }
            }
            tid.to_string()
        }
        _ => {
            // Create new thread.
            let tid = format!("thr-{}-{}", agent_id, *next_sequence);
            let thread_row = SessionThreadRow {
                id: tid.clone(),
                session_id: session_id.to_string(),
                agent_id: agent_id.to_string(),
                parent_thread_id: None,
                status: "Running".to_string(),
                created_at: now.to_string(),
                updated_at: now.to_string(),
            };
            if let Err(e) = temper.create_session_thread(&thread_row).await {
                return tools::ToolResult {
                    output: format!("Error creating thread: {e}"),
                    is_error: true,
                };
            }

            // Emit session.thread_created on the primary thread.
            let create_ev = SessionEventRow {
                id: event_id(session_id, *next_sequence),
                session_id: session_id.to_string(),
                sequence: *next_sequence,
                kind: "session.thread_created".to_string(),
                created_at: now.to_string(),
                processed_at: Some(now.to_string()),
                content: Some(
                    serde_json::json!({
                        "agent_id": agent_id,
                        "thread_id": tid,
                    })
                    .to_string(),
                ),
                session_thread_id: Some(tid.clone()),
                ..blank_event()
            };
            if let Err(e) = temper.create_session_event(&create_ev).await {
                return tools::ToolResult {
                    output: format!("Error emitting thread_created event: {e}"),
                    is_error: true,
                };
            }
            *next_sequence += 1;
            tid
        }
    };

    // Emit agent.thread_message_sent on primary.
    let sent_ev = SessionEventRow {
        id: event_id(session_id, *next_sequence),
        session_id: session_id.to_string(),
        sequence: *next_sequence,
        kind: "agent.thread_message_sent".to_string(),
        created_at: now.to_string(),
        processed_at: Some(now.to_string()),
        content: Some(
            serde_json::json!({
                "to_thread_id": thread_id,
                "content": message,
            })
            .to_string(),
        ),
        session_thread_id: Some(thread_id.clone()),
        ..blank_event()
    };
    if let Err(e) = temper.create_session_event(&sent_ev).await {
        return tools::ToolResult {
            output: format!("Error emitting thread_message_sent: {e}"),
            is_error: true,
        };
    }
    *next_sequence += 1;

    // Run sub-agent turn.
    match respond_thread(temper, model, session_id, &thread_id, agent_id, message, next_sequence, tool_router)
        .await
    {
        Ok(result_text) => {
            // Emit agent.thread_message_received on primary.
            let recv_ev = SessionEventRow {
                id: event_id(session_id, *next_sequence),
                session_id: session_id.to_string(),
                sequence: *next_sequence,
                kind: "agent.thread_message_received".to_string(),
                created_at: now.to_string(),
                processed_at: Some(now.to_string()),
                content: Some(
                    serde_json::json!({
                        "from_thread_id": thread_id,
                        "content": result_text,
                    })
                    .to_string(),
                ),
                session_thread_id: Some(thread_id.clone()),
                ..blank_event()
            };
            let _ = temper.create_session_event(&recv_ev).await;
            *next_sequence += 1;

            // Idle the thread.
            let _ = temper
                .invoke_thread_action(&thread_id, ThreadAction::IdleThread)
                .await;

            tools::ToolResult {
                output: result_text,
                is_error: false,
            }
        }
        Err(e) => tools::ToolResult {
            output: format!("Sub-agent error: {e}"),
            is_error: true,
        },
    }
}

/// Run one turn of a sub-agent within a delegation thread. This is a
/// focused variant of `respond()` that:
/// - Reads the sub-agent's own config (model, system prompt, tools)
/// - Scopes events to the thread via `SessionThreadId`
/// - Sub-agents cannot delegate (no callable agents injected)
/// - Returns just the response text
async fn respond_thread<M: Model>(
    temper: &TemperClient,
    model: &M,
    session_id: &str,
    thread_id: &str,
    agent_id: &str,
    message: &str,
    next_sequence: &mut i64,
    tool_router: &ToolRouter,
) -> Result<String> {
    let now = now_rfc3339();

    // Load the sub-agent's config.
    let sub_agent = temper
        .get_managed_agent(agent_id)
        .await
        .with_context(|| format!("loading sub-agent ManagedAgent('{agent_id}')"))?;

    // Load sub-agent's tools (no callable agents — one level only).
    let sub_agent_tools = temper
        .list_agent_tools(agent_id)
        .await
        .unwrap_or_default();
    let tool_defs = build_tools_from_agent(&sub_agent_tools);
    let tools_for_request = if tool_defs.is_empty() {
        None
    } else {
        Some(tool_defs)
    };

    // Read thread-scoped history.
    let thread_history = temper
        .list_thread_events(session_id, thread_id, EVENT_HISTORY_LIMIT)
        .await
        .unwrap_or_default();

    // Build messages from thread history.
    let mut messages = events_to_messages(&thread_history)?;

    // Add the delegated message as a user message.
    let user_ev = SessionEventRow {
        id: event_id(session_id, *next_sequence),
        session_id: session_id.to_string(),
        sequence: *next_sequence,
        kind: "user.message".to_string(),
        created_at: now.clone(),
        processed_at: Some(now.clone()),
        content: Some(user_message_content(message)),
        session_thread_id: Some(thread_id.to_string()),
        ..blank_event()
    };
    temper
        .create_session_event(&user_ev)
        .await
        .context("POSTing thread user.message")?;
    messages.push(ChatMessage {
        role: "user".to_string(),
        content: vec![ContentBlock::text(message)],
    });
    *next_sequence += 1;

    // Sub-agent tool loop (same max iterations).
    let mut assistant_text = String::new();

    for _iteration in 0..MAX_TOOL_ITERATIONS {
        // span.model_request_start
        let start_id = event_id(session_id, *next_sequence);
        let start_ev = SessionEventRow {
            id: start_id.clone(),
            session_id: session_id.to_string(),
            sequence: *next_sequence,
            kind: "span.model_request_start".to_string(),
            created_at: now.clone(),
            processed_at: Some(now.clone()),
            content: Some(
                serde_json::json!({ "model": sub_agent.model_id, "started_at": now }).to_string(),
            ),
            model_speed: sub_agent.model_speed.clone(),
            session_thread_id: Some(thread_id.to_string()),
            ..blank_event()
        };
        temper.create_session_event(&start_ev).await?;
        *next_sequence += 1;

        // Call the model.
        let req = MessagesRequest {
            model: sub_agent.model_id.clone(),
            system: sub_agent.system.clone(),
            messages: messages.clone(),
            max_tokens: DEFAULT_MAX_TOKENS,
            tools: tools_for_request.clone(),
        };
        let response = model.complete(req).await.context("sub-agent model call")?;

        if response.has_tool_use() {
            // Emit agent.message with tool_use blocks.
            let agent_ev = SessionEventRow {
                id: event_id(session_id, *next_sequence),
                session_id: session_id.to_string(),
                sequence: *next_sequence,
                kind: "agent.message".to_string(),
                created_at: now.clone(),
                processed_at: Some(now.clone()),
                content: Some(content_blocks_to_blob(&response.content)),
                session_thread_id: Some(thread_id.to_string()),
                ..blank_event()
            };
            temper.create_session_event(&agent_ev).await?;
            *next_sequence += 1;

            // span.model_request_end
            let end_ev = SessionEventRow {
                id: event_id(session_id, *next_sequence),
                session_id: session_id.to_string(),
                sequence: *next_sequence,
                kind: "span.model_request_end".to_string(),
                created_at: now.clone(),
                processed_at: Some(now.clone()),
                content: Some(
                    serde_json::json!({
                        "stop_reason": response.stop_reason.clone().unwrap_or_default()
                    })
                    .to_string(),
                ),
                model_request_start_id: Some(start_id.clone()),
                is_error: Some(false),
                model_input_tokens: Some(response.usage.input_tokens),
                model_output_tokens: Some(response.usage.output_tokens),
                model_cache_creation_input_tokens: Some(
                    response.usage.cache_creation_input_tokens,
                ),
                model_cache_read_input_tokens: Some(response.usage.cache_read_input_tokens),
                model_speed: sub_agent.model_speed.clone(),
                session_thread_id: Some(thread_id.to_string()),
                ..blank_event()
            };
            temper.create_session_event(&end_ev).await?;
            *next_sequence += 1;

            messages.push(ChatMessage {
                role: "assistant".to_string(),
                content: response.content.clone(),
            });

            // Execute tools and collect results.
            let mut tool_results: Vec<ContentBlock> = Vec::new();
            for block in &response.content {
                if let ContentBlock::ToolUse { id, name, input } = block {
                    // Sub-agents cannot delegate — route via tool router.
                    let result = route_tool_call(tool_router, temper, name, input).await;

                    let tr_ev = SessionEventRow {
                        id: event_id(session_id, *next_sequence),
                        session_id: session_id.to_string(),
                        sequence: *next_sequence,
                        kind: "agent.tool_result".to_string(),
                        created_at: now.clone(),
                        processed_at: Some(now.clone()),
                        content: Some(
                            serde_json::json!({
                                "blocks": [{
                                    "type": "tool_result",
                                    "tool_use_id": id,
                                    "content": result.output,
                                    "is_error": result.is_error,
                                }]
                            })
                            .to_string(),
                        ),
                        tool_use_id: Some(id.clone()),
                        session_thread_id: Some(thread_id.to_string()),
                        ..blank_event()
                    };
                    temper.create_session_event(&tr_ev).await?;
                    *next_sequence += 1;

                    tool_results.push(ContentBlock::ToolResult {
                        tool_use_id: id.clone(),
                        content: result.output,
                        is_error: Some(result.is_error),
                    });
                }
            }
            messages.push(ChatMessage {
                role: "user".to_string(),
                content: tool_results,
            });
            continue;
        }

        // Text-only response — final iteration.
        assistant_text = response.text();

        // Emit agent.message.
        let agent_ev = SessionEventRow {
            id: event_id(session_id, *next_sequence),
            session_id: session_id.to_string(),
            sequence: *next_sequence,
            kind: "agent.message".to_string(),
            created_at: now.clone(),
            processed_at: Some(now.clone()),
            content: Some(agent_message_content(&assistant_text)),
            session_thread_id: Some(thread_id.to_string()),
            ..blank_event()
        };
        temper.create_session_event(&agent_ev).await?;
        *next_sequence += 1;

        // span.model_request_end
        let end_ev = SessionEventRow {
            id: event_id(session_id, *next_sequence),
            session_id: session_id.to_string(),
            sequence: *next_sequence,
            kind: "span.model_request_end".to_string(),
            created_at: now.clone(),
            processed_at: Some(now.clone()),
            content: Some(
                serde_json::json!({
                    "stop_reason": response.stop_reason.clone().unwrap_or_default()
                })
                .to_string(),
            ),
            model_request_start_id: Some(start_id),
            is_error: Some(false),
            model_input_tokens: Some(response.usage.input_tokens),
            model_output_tokens: Some(response.usage.output_tokens),
            model_cache_creation_input_tokens: Some(response.usage.cache_creation_input_tokens),
            model_cache_read_input_tokens: Some(response.usage.cache_read_input_tokens),
            model_speed: sub_agent.model_speed.clone(),
            session_thread_id: Some(thread_id.to_string()),
            ..blank_event()
        };
        temper.create_session_event(&end_ev).await?;
        *next_sequence += 1;

        break;
    }

    Ok(assistant_text)
}

/// Walk a chronologically sorted SessionEvent history and reduce it
/// to the `messages` array an Anthropic Messages-API request needs.
///
/// Rules:
/// - `user.message` → one user turn containing text blocks
/// - `agent.message` → one assistant turn containing all blocks
///   (text, tool_use — supporting the iterative tool loop)
/// - `agent.tool_result` → accumulated into a user turn with
///   tool_result blocks (grouped between assistant messages)
/// - every other kind is skipped (state pulses, observability spans,
///   individual tool_use observability events)
/// - malformed `Content` JSON surfaces as an `anyhow` error
///
/// This is `pub` so the unit tests at the bottom of the file can
/// exercise it directly.
pub fn events_to_messages(history: &[SessionEventRow]) -> Result<Vec<ChatMessage>> {
    let mut out = Vec::new();
    let mut pending_tool_results: Vec<ContentBlock> = Vec::new();

    for ev in history {
        match ev.kind.as_str() {
            "user.message" => {
                flush_tool_results(&mut out, &mut pending_tool_results);
                let content = ev.content.as_deref().unwrap_or("");
                let text = extract_text_from_content_blob(content, &ev.kind, ev.sequence)?;
                out.push(ChatMessage {
                    role: "user".to_string(),
                    content: vec![ContentBlock::text(text)],
                });
            }
            "agent.message" => {
                flush_tool_results(&mut out, &mut pending_tool_results);
                let content = ev.content.as_deref().unwrap_or("");
                let blocks = extract_content_blocks(content, &ev.kind, ev.sequence)?;
                if !blocks.is_empty() {
                    out.push(ChatMessage {
                        role: "assistant".to_string(),
                        content: blocks,
                    });
                }
            }
            "agent.tool_result" => {
                let content = ev.content.as_deref().unwrap_or("");
                let blocks = extract_content_blocks(content, &ev.kind, ev.sequence)?;
                for block in blocks {
                    if matches!(block, ContentBlock::ToolResult { .. }) {
                        pending_tool_results.push(block);
                    }
                }
            }
            _ => continue,
        }
    }

    flush_tool_results(&mut out, &mut pending_tool_results);
    Ok(out)
}

/// Push accumulated tool_result blocks as a single user message.
fn flush_tool_results(out: &mut Vec<ChatMessage>, pending: &mut Vec<ContentBlock>) {
    if !pending.is_empty() {
        out.push(ChatMessage {
            role: "user".to_string(),
            content: pending.drain(..).collect(),
        });
    }
}

/// Parse a Content blob `{"blocks":[...]}` into typed `ContentBlock`s.
/// Handles text, tool_use, and tool_result block types. Unknown block
/// types are silently skipped.
fn extract_content_blocks(blob: &str, kind: &str, sequence: i64) -> Result<Vec<ContentBlock>> {
    #[derive(Deserialize)]
    struct Envelope {
        blocks: Vec<serde_json::Value>,
    }

    if blob.trim().is_empty() {
        return Err(anyhow!(
            "{kind} event at sequence {sequence} has empty Content"
        ));
    }
    let env: Envelope = serde_json::from_str(blob).with_context(|| {
        format!("decoding Content blob for {kind} event at sequence {sequence}: {blob}")
    })?;

    let mut out = Vec::new();
    for block in env.blocks {
        let ty = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match ty {
            "text" => {
                let text = block
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                out.push(ContentBlock::Text { text });
            }
            "tool_use" => {
                let id = block
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = block
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let input = block
                    .get("input")
                    .cloned()
                    .unwrap_or(serde_json::json!({}));
                out.push(ContentBlock::ToolUse { id, name, input });
            }
            "tool_result" => {
                let tool_use_id = block
                    .get("tool_use_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let content = block
                    .get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let is_error = block.get("is_error").and_then(|v| v.as_bool());
                out.push(ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                });
            }
            _ => continue,
        }
    }
    Ok(out)
}

/// Parse `{"blocks":[{"type":"text","text":"..."}]}` and return a
/// single concatenated text string. Used for `user.message` events
/// which are always pure text.
fn extract_text_from_content_blob(blob: &str, kind: &str, sequence: i64) -> Result<String> {
    let blocks = extract_content_blocks(blob, kind, sequence)?;
    let mut out = String::new();
    for b in &blocks {
        if let Some(t) = b.as_text() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(t);
        }
    }
    if out.is_empty() {
        return Err(anyhow!(
            "{kind} event at sequence {sequence} has no text blocks in Content: {blob}",
        ));
    }
    Ok(out)
}

/// Serialize a list of ContentBlocks into the `{"blocks":[...]}` blob
/// format, preserving tool_use and tool_result blocks alongside text.
fn content_blocks_to_blob(blocks: &[ContentBlock]) -> String {
    let block_values: Vec<serde_json::Value> = blocks
        .iter()
        .map(|b| serde_json::to_value(b).unwrap_or(serde_json::json!(null)))
        .collect();
    serde_json::json!({ "blocks": block_values }).to_string()
}

// ----------------------------------------------------------------------
// Helpers
// ----------------------------------------------------------------------

/// Build a deterministic event id. Idempotent across replays of the
/// same turn — if the caller re-invokes mid-failure, the second POST
/// will 409 on the duplicate primary key, which is the correct
/// resume signal.
fn event_id(session_id: &str, sequence: i64) -> String {
    format!("ev-auto-{session_id}-{sequence}")
}

/// Serialize a plain text string into the `{"blocks":[{"type":"text","text":"..."}]}`
/// shape the existing `SESSION_EVENTS_CURL_WALKTHROUGH.md` uses.
fn user_message_content(text: &str) -> String {
    serde_json::json!({
        "blocks": [{ "type": "text", "text": text }]
    })
    .to_string()
}

fn agent_message_content(text: &str) -> String {
    user_message_content(text)
}

/// A SessionEventRow with every optional column cleared. Used as
/// `..blank_event()` so the five kinds the responder emits each only
/// need to set the columns their field invariants actually require.
fn blank_event() -> SessionEventRow {
    SessionEventRow {
        id: String::new(),
        session_id: String::new(),
        sequence: 0,
        kind: String::new(),
        created_at: String::new(),
        processed_at: None,
        content: None,
        stop_reason: None,
        stop_reason_event_ids: None,
        model_request_start_id: None,
        is_error: None,
        model_input_tokens: None,
        model_output_tokens: None,
        model_cache_creation_input_tokens: None,
        model_cache_read_input_tokens: None,
        model_speed: None,
        tool_name: None,
        tool_use_id: None,
        session_thread_id: None,
    }
}

/// Format a UTC timestamp as RFC-3339 with millisecond precision.
/// Kept as a tiny inline implementation to avoid pulling `chrono`
/// into the crate just for one format string.
pub fn now_rfc3339() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = d.as_secs() as i64;
    let millis = d.subsec_millis();
    // Convert epoch seconds to civil date via the standard trick.
    let (year, month, day, hour, minute, second) = epoch_to_civil(secs);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// Convert a unix timestamp (seconds since 1970-01-01 UTC) to a
/// civil date tuple `(year, month, day, hour, minute, second)`.
/// Howard Hinnant's algorithm — no external calendar crate needed.
pub fn epoch_to_civil(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400) as u32;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day / 60) % 60;
    let second = secs_of_day % 60;
    (year, m, d, hour, minute, second)
}

/// Serialize helper used by the CLI for logging. Keeps the module
/// self-contained — no need to re-export serde types from other
/// modules.
#[derive(Serialize)]
pub struct TurnLog<'a> {
    pub session_id: &'a str,
    pub agent_id: &'a str,
    pub assistant_text: &'a str,
    pub input_tokens: i64,
    pub output_tokens: i64,
}

// ======================================================================
// Unit tests — pure-function coverage of events_to_messages +
// extract_text_from_content_blob + epoch_to_civil.
// ======================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn blank_row(seq: i64, kind: &str, content: Option<&str>) -> SessionEventRow {
        SessionEventRow {
            id: format!("ev-{seq}"),
            session_id: "sess-test".to_string(),
            sequence: seq,
            kind: kind.to_string(),
            created_at: "2026-04-11T00:00:00Z".to_string(),
            content: content.map(|s| s.to_string()),
            ..blank_event()
        }
    }

    fn text_blob(s: &str) -> String {
        serde_json::json!({ "blocks": [{ "type": "text", "text": s }] }).to_string()
    }

    #[test]
    fn empty_history_produces_empty_message_list() {
        let msgs = events_to_messages(&[]).unwrap();
        assert!(msgs.is_empty());
    }

    #[test]
    fn single_user_message_maps_to_single_user_turn() {
        let blob = text_blob("hello");
        let history = vec![blank_row(0, "user.message", Some(&blob))];
        let msgs = events_to_messages(&history).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[0].content[0].as_text(), Some("hello"));
    }

    #[test]
    fn user_agent_user_produces_three_turns_in_order() {
        let history = vec![
            blank_row(0, "user.message", Some(&text_blob("q1"))),
            blank_row(1, "agent.message", Some(&text_blob("a1"))),
            blank_row(2, "user.message", Some(&text_blob("q2"))),
        ];
        let msgs = events_to_messages(&history).unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[0].content[0].as_text(), Some("q1"));
        assert_eq!(msgs[1].role, "assistant");
        assert_eq!(msgs[1].content[0].as_text(), Some("a1"));
        assert_eq!(msgs[2].role, "user");
        assert_eq!(msgs[2].content[0].as_text(), Some("q2"));
    }

    #[test]
    fn non_chat_kinds_are_skipped() {
        let history = vec![
            blank_row(0, "session.status_running", Some("{}")),
            blank_row(1, "user.message", Some(&text_blob("q1"))),
            blank_row(2, "span.model_request_start", Some("{}")),
            blank_row(3, "agent.thinking", Some("{}")),
            blank_row(4, "agent.message", Some(&text_blob("a1"))),
            blank_row(5, "span.model_request_end", Some("{}")),
            blank_row(6, "session.status_idle", Some("{}")),
        ];
        let msgs = events_to_messages(&history).unwrap();
        assert_eq!(msgs.len(), 2, "only the two chat turns should survive");
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[0].content[0].as_text(), Some("q1"));
        assert_eq!(msgs[1].role, "assistant");
        assert_eq!(msgs[1].content[0].as_text(), Some("a1"));
    }

    #[test]
    fn malformed_content_blob_surfaces_as_error() {
        let history = vec![blank_row(0, "user.message", Some("not json"))];
        let err = events_to_messages(&history).unwrap_err();
        let s = err.to_string();
        assert!(
            s.contains("user.message") || s.contains("Content"),
            "error should mention the kind or Content: {s}"
        );
    }

    #[test]
    fn empty_content_blob_is_rejected() {
        let history = vec![blank_row(0, "user.message", Some(""))];
        let err = events_to_messages(&history).unwrap_err();
        assert!(
            err.to_string().contains("empty Content"),
            "{err}"
        );
    }

    #[test]
    fn agent_message_with_tool_use_blocks_preserves_them() {
        let blob = serde_json::json!({
            "blocks": [
                { "type": "text", "text": "Let me check." },
                { "type": "tool_use", "id": "toolu_1", "name": "bash", "input": {"command": "ls"} }
            ]
        })
        .to_string();
        let history = vec![
            blank_row(0, "user.message", Some(&text_blob("run ls"))),
            blank_row(1, "agent.message", Some(&blob)),
        ];
        let msgs = events_to_messages(&history).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[1].role, "assistant");
        assert_eq!(msgs[1].content.len(), 2);
        assert_eq!(msgs[1].content[0].as_text(), Some("Let me check."));
        assert!(msgs[1].content[1].is_tool_use());
    }

    #[test]
    fn tool_result_events_are_grouped_into_user_turn() {
        let agent_blob = serde_json::json!({
            "blocks": [
                { "type": "tool_use", "id": "t1", "name": "bash", "input": {} },
                { "type": "tool_use", "id": "t2", "name": "read", "input": {} }
            ]
        })
        .to_string();
        let tr1 = serde_json::json!({
            "blocks": [{ "type": "tool_result", "tool_use_id": "t1", "content": "out1" }]
        })
        .to_string();
        let tr2 = serde_json::json!({
            "blocks": [{ "type": "tool_result", "tool_use_id": "t2", "content": "out2" }]
        })
        .to_string();
        let final_blob = text_blob("Done.");

        let history = vec![
            blank_row(0, "user.message", Some(&text_blob("do it"))),
            blank_row(1, "agent.message", Some(&agent_blob)),
            blank_row(2, "agent.tool_use", Some("{}")),  // observability, skipped
            blank_row(3, "agent.tool_use", Some("{}")),  // observability, skipped
            blank_row(4, "agent.tool_result", Some(&tr1)),
            blank_row(5, "agent.tool_result", Some(&tr2)),
            blank_row(6, "agent.message", Some(&final_blob)),
        ];
        let msgs = events_to_messages(&history).unwrap();
        // user.message, assistant(tool_use), user(tool_results), assistant(text)
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[1].role, "assistant"); // tool_use blocks
        assert_eq!(msgs[2].role, "user"); // grouped tool_results
        assert_eq!(msgs[2].content.len(), 2);
        assert!(matches!(&msgs[2].content[0], ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "t1"));
        assert!(matches!(&msgs[2].content[1], ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "t2"));
        assert_eq!(msgs[3].role, "assistant");
        assert_eq!(msgs[3].content[0].as_text(), Some("Done."));
    }

    #[test]
    fn build_tools_from_agent_handles_all_kinds() {
        use crate::chat::temper_client::AgentToolRow;
        let rows = vec![
            AgentToolRow {
                id: "t1".into(),
                agent_id: "agt".into(),
                kind: "agent_toolset".into(),
                name: None,
                description: None,
                input_schema: None,
            },
            AgentToolRow {
                id: "t2".into(),
                agent_id: "agt".into(),
                kind: "custom".into(),
                name: Some("my_tool".into()),
                description: Some("does stuff".into()),
                input_schema: Some(r#"{"type":"object","properties":{}}"#.into()),
            },
            AgentToolRow {
                id: "t3".into(),
                agent_id: "agt".into(),
                kind: "mcp_toolset".into(), // skipped
                name: None,
                description: None,
                input_schema: None,
            },
        ];
        let defs = build_tools_from_agent(&rows);
        // 6 built-in + 1 custom = 7
        assert_eq!(defs.len(), 7);
        assert_eq!(defs[6].name, "my_tool");
        assert_eq!(defs[6].description, "does stuff");
    }

    #[test]
    fn epoch_to_civil_matches_known_dates() {
        // 1970-01-01T00:00:00Z — unix epoch
        assert_eq!(epoch_to_civil(0), (1970, 1, 1, 0, 0, 0));
        // 1970-01-02T00:00:00Z — one day after epoch
        assert_eq!(epoch_to_civil(86_400), (1970, 1, 2, 0, 0, 0));
        // 2024-01-01T00:00:00Z — well-known reference timestamp
        assert_eq!(epoch_to_civil(1_704_067_200), (2024, 1, 1, 0, 0, 0));
        // 2024-02-29T00:00:00Z — leap-day edge case
        assert_eq!(epoch_to_civil(1_709_164_800), (2024, 2, 29, 0, 0, 0));
    }

    #[test]
    fn event_id_is_deterministic_per_session_and_sequence() {
        assert_eq!(event_id("sess-xyz", 0), "ev-auto-sess-xyz-0");
        assert_eq!(event_id("sess-xyz", 42), "ev-auto-sess-xyz-42");
    }

    #[test]
    fn user_message_content_wraps_text_in_blocks_envelope() {
        let out = user_message_content("hello world");
        // Parse it back — that is the only stable assertion about the
        // shape since serde_json does not guarantee key order.
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["blocks"][0]["type"], "text");
        assert_eq!(v["blocks"][0]["text"], "hello world");
    }
}
