//! Core agent execution loop.
//!
//! [`AgentRunner`] orchestrates the full agent lifecycle:
//! 1. Create Agent entity, Assign, Start
//! 2. Create Plan, Activate
//! 3. LLM decomposes goal into tasks (Plan.AddTask)
//! 4. For each task: Claim, StartWork, tool-call loop, SubmitForReview, Approve
//! 5. Plan.Complete, Agent.Complete

use std::io::Write;

use anyhow::Result;
use serde_json::json;
use temper_sdk::TemperClient;
use tracing::{info, warn};

use crate::providers::{ContentBlock, LlmProvider, Message};
use crate::tools::{ToolRegistry, ToolResult};

/// Core agent execution runner.
///
/// Wraps a [`TemperClient`], an [`LlmProvider`], and a [`ToolRegistry`]
/// to run the full agent lifecycle against a Temper server.
pub struct AgentRunner {
    client: TemperClient,
    provider: Box<dyn LlmProvider>,
    tools: Box<dyn ToolRegistry>,
}

impl AgentRunner {
    /// Create a new agent runner.
    pub fn new(
        client: TemperClient,
        provider: Box<dyn LlmProvider>,
        tools: Box<dyn ToolRegistry>,
    ) -> Self {
        Self {
            client,
            provider,
            tools,
        }
    }

    /// Run a new agent with the given goal and role.
    ///
    /// Creates the Agent entity, plans tasks via LLM, executes each task,
    /// and returns the agent ID on completion.
    pub async fn run(&self, goal: &str, role: &str) -> Result<String> {
        let model = "claude-sonnet-4-6";
        let agent_id = uuid::Uuid::now_v7().to_string();

        info!(agent_id = %agent_id, "Creating agent");
        self.create_agent(&agent_id, role, goal, model).await?;

        self.execute_agent_loop(&agent_id, goal, role, model)
            .await?;

        Ok(agent_id)
    }

    /// Run a new agent with a specific model.
    ///
    /// Like [`run`](Self::run) but allows specifying the LLM model name.
    pub async fn run_with_model(&self, goal: &str, role: &str, model: &str) -> Result<String> {
        let agent_id = uuid::Uuid::now_v7().to_string();

        info!(agent_id = %agent_id, "Creating agent");
        self.create_agent(&agent_id, role, goal, model).await?;

        self.execute_agent_loop(&agent_id, goal, role, model)
            .await?;

        Ok(agent_id)
    }

    /// Resume an existing agent by ID.
    ///
    /// Reads the agent's current state and continues execution from where
    /// it left off.
    pub async fn resume(&self, agent_id: &str) -> Result<()> {
        info!(agent_id = %agent_id, "Resuming agent");

        let agent = self.client.get("Agents", agent_id).await?;
        let status = agent.get("Status").and_then(|v| v.as_str()).unwrap_or("");
        let role = agent
            .get("role")
            .and_then(|v| v.as_str())
            .unwrap_or("assistant");
        let goal = agent.get("goal").and_then(|v| v.as_str()).unwrap_or("");
        let model = agent
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("claude-sonnet-4-6");

        info!(status = %status, "Agent status");

        if status == "Completed" || status == "Failed" {
            info!("Agent already in terminal state: {status}");
            return Ok(());
        }

        self.execute_agent_loop(agent_id, goal, role, model).await
    }

    /// Complete an agent (transition to terminal state).
    pub async fn complete_agent(&self, agent_id: &str) -> Result<()> {
        self.client
            .action(
                "Agents",
                agent_id,
                "Complete",
                json!({ "result": "interactive session ended" }),
            )
            .await?;
        Ok(())
    }

    // ── Internal helpers ────────────────────────────────────────────────

    /// Create an Agent entity and transition it to Working.
    async fn create_agent(&self, id: &str, role: &str, goal: &str, model: &str) -> Result<()> {
        self.client.create("Agents", json!({ "id": id })).await?;
        self.client
            .action(
                "Agents",
                id,
                "Assign",
                json!({ "role": role, "goal": goal, "model": model }),
            )
            .await?;
        self.client.action("Agents", id, "Start", json!({})).await?;
        Ok(())
    }

    /// Execute the full agent loop: plan, execute tasks, complete.
    async fn execute_agent_loop(
        &self,
        agent_id: &str,
        goal: &str,
        role: &str,
        _model: &str,
    ) -> Result<()> {
        // Build tool schemas for the LLM.
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

        // ── Phase 1: Planning ─────────────────────────────────────────
        info!("Phase 1: Planning");
        let plan_id = self.create_plan(goal).await?;
        info!(plan_id = %plan_id, "Plan created");

        // Checkpoint plan_id on the agent.
        self.checkpoint(agent_id, &format!("{{\"plan_id\":\"{plan_id}\"}}"))
            .await;

        let tasks = self.decompose_goal(goal, role, &tool_schemas).await?;
        info!(count = tasks.len(), "Tasks planned");

        // Add tasks to the plan.
        for task in &tasks {
            let title = task
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("Untitled");
            let description = task
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if let Err(e) = self
                .client
                .action(
                    "Plans",
                    &plan_id,
                    "AddTask",
                    json!({ "title": title, "description": description }),
                )
                .await
            {
                warn!("AddTask failed: {e}");
            }
        }

        // Brief pause for spawn dispatch.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        // Query spawned tasks.
        let filter = format!("plan_id eq '{plan_id}'");
        let spawned_tasks = self
            .client
            .list_filtered("Tasks", &filter)
            .await
            .unwrap_or_default();
        info!(count = spawned_tasks.len(), "Spawned tasks");

        // ── Phase 2: Execution ────────────────────────────────────────
        info!("Phase 2: Execution");
        let max_turns_per_task = 30;

        for (idx, task_entity) in spawned_tasks.iter().enumerate() {
            let task_id = task_entity.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let task_title = task_entity
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("Untitled");
            let task_desc = task_entity
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("");

            if task_id.is_empty() {
                continue;
            }

            info!(
                task = idx + 1,
                total = spawned_tasks.len(),
                title = task_title,
                "Executing task"
            );

            // Claim the task.
            if let Err(e) = self
                .client
                .action("Tasks", task_id, "Claim", json!({ "agent_id": agent_id }))
                .await
            {
                warn!("Task.Claim failed: {e}");
                continue;
            }

            // Start work.
            if let Err(e) = self
                .client
                .action("Tasks", task_id, "StartWork", json!({}))
                .await
            {
                warn!("Task.StartWork failed: {e}");
                continue;
            }

            // Build task-scoped system prompt.
            let task_prompt = format!(
                "You are a Temper agent with role '{role}'. \
                 You are working on task: {task_title}\n\
                 Task description: {task_desc}\n\n\
                 Overall goal: {goal}\n\n\
                 You have tools to interact with the filesystem, execute shell commands, \
                 and manage Temper entities. \
                 Focus on completing this specific task. When done, provide a summary of what you accomplished."
            );

            let mut task_messages = vec![Message {
                role: "user".to_string(),
                content: vec![ContentBlock::Text {
                    text: format!("Work on this task: {task_title}\n\nDescription: {task_desc}"),
                }],
            }];

            let deliverable = self
                .run_task_loop(
                    agent_id,
                    &task_prompt,
                    &tool_schemas,
                    &mut task_messages,
                    max_turns_per_task,
                )
                .await?;

            // Submit for review.
            let truncated_deliverable: String = deliverable.chars().take(2000).collect();
            if let Err(e) = self
                .client
                .action(
                    "Tasks",
                    task_id,
                    "SubmitForReview",
                    json!({ "deliverable": truncated_deliverable }),
                )
                .await
            {
                warn!("Task.SubmitForReview failed: {e}");
            }

            // Auto-approve.
            if let Err(e) = self
                .client
                .action("Tasks", task_id, "Approve", json!({}))
                .await
            {
                warn!("Task.Approve failed: {e}");
            }

            info!(title = task_title, "Task completed");
        }

        // ── Completion ────────────────────────────────────────────────
        let summary = format!("Completed {} tasks for goal: {goal}", spawned_tasks.len());
        if let Err(e) = self
            .client
            .action("Plans", &plan_id, "Complete", json!({ "summary": summary }))
            .await
        {
            warn!("Plan.Complete failed: {e}");
        }

        let result_text = format!(
            "Plan {plan_id} completed with {} tasks.",
            spawned_tasks.len()
        );
        if let Err(e) = self
            .client
            .action(
                "Agents",
                agent_id,
                "Complete",
                json!({ "result": result_text }),
            )
            .await
        {
            warn!("Agent.Complete failed: {e}");
        }

        info!(plan_id = %plan_id, "Agent completed");
        Ok(())
    }

    /// Create a Plan entity and activate it.
    async fn create_plan(&self, description: &str) -> Result<String> {
        let plan_id = uuid::Uuid::now_v7().to_string();
        self.client
            .create("Plans", json!({ "id": plan_id }))
            .await?;
        self.client
            .action(
                "Plans",
                &plan_id,
                "Activate",
                json!({ "description": description }),
            )
            .await?;
        Ok(plan_id)
    }

    /// Ask the LLM to decompose a goal into tasks.
    async fn decompose_goal(
        &self,
        goal: &str,
        role: &str,
        _tool_schemas: &[serde_json::Value],
    ) -> Result<Vec<serde_json::Value>> {
        let planning_prompt = format!(
            "You are a Temper agent with role '{role}'. Your goal: {goal}\n\n\
             Decompose this goal into a list of discrete tasks. \
             Respond with ONLY a JSON array of objects, each with \"title\" and \"description\" fields. \
             Example: [{{\"title\": \"Set up project\", \"description\": \"Initialize the project structure\"}}]\n\n\
             Keep the list focused and actionable. Each task should be a meaningful unit of work."
        );

        let messages = vec![Message {
            role: "user".to_string(),
            content: vec![ContentBlock::Text {
                text: format!("Decompose this goal into tasks: {goal}"),
            }],
        }];

        let response = self
            .provider
            .send_streaming(
                &planning_prompt,
                &messages,
                &[],
                Box::new(|text| {
                    print!("{text}");
                    std::io::stdout().flush().ok();
                }),
            )
            .await?;

        let plan_text = response
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");

        let json_text = plan_text
            .trim()
            .trim_start_matches("```json")
            .trim_start_matches("```")
            .trim_end_matches("```")
            .trim();

        let tasks: Vec<serde_json::Value> = serde_json::from_str(json_text)
            .unwrap_or_else(|_| vec![json!({ "title": goal, "description": goal })]);

        Ok(tasks)
    }

    /// Run the LLM tool-call loop for a single task.
    async fn run_task_loop(
        &self,
        agent_id: &str,
        system_prompt: &str,
        tool_schemas: &[serde_json::Value],
        messages: &mut Vec<Message>,
        max_turns: usize,
    ) -> Result<String> {
        for turn in 0..max_turns {
            let response = self
                .provider
                .send_streaming(
                    system_prompt,
                    messages,
                    tool_schemas,
                    Box::new(|text| {
                        print!("{text}");
                        std::io::stdout().flush().ok();
                    }),
                )
                .await?;

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

            if tool_uses.is_empty() || response.stop_reason == "end_turn" {
                let result_text = response
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");

                self.checkpoint(
                    agent_id,
                    &serde_json::to_string(messages).unwrap_or_default(),
                )
                .await;

                return Ok(result_text);
            }

            // Execute tool calls.
            let mut tool_results = Vec::new();
            for (tool_use_id, tool_name, tool_input) in &tool_uses {
                info!(tool = %tool_name, "Executing tool");

                let result = self.execute_tool(agent_id, tool_name, tool_input).await?;

                match result {
                    ToolResult::Success(output) => {
                        tool_results.push(ContentBlock::ToolResult {
                            tool_use_id: tool_use_id.clone(),
                            content: output,
                            is_error: None,
                        });
                    }
                    ToolResult::Error(err) => {
                        tool_results.push(ContentBlock::ToolResult {
                            tool_use_id: tool_use_id.clone(),
                            content: format!("Error: {err}"),
                            is_error: Some(true),
                        });
                    }
                }
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

        // Authorized.
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

    /// Run a single conversational turn for interactive mode.
    ///
    /// Pushes the user message onto `messages`, runs the LLM tool-call loop
    /// until it returns text (end_turn), and returns the assistant's response.
    /// No plan decomposition — the user is the planner in interactive mode.
    pub async fn run_turn(
        &self,
        agent_id: &str,
        system_prompt: &str,
        user_message: &str,
        messages: &mut Vec<Message>,
    ) -> Result<String> {
        // Build tool schemas.
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

        // Push user message.
        messages.push(Message {
            role: "user".to_string(),
            content: vec![ContentBlock::Text {
                text: user_message.to_string(),
            }],
        });

        // Run tool-call loop (max 30 turns).
        let result = self
            .run_task_loop(agent_id, system_prompt, &tool_schemas, messages, 30)
            .await?;

        Ok(result)
    }

    /// Create a new agent entity for interactive mode and return its ID.
    pub async fn create_interactive_agent(&self, role: &str, model: &str) -> Result<String> {
        let agent_id = uuid::Uuid::now_v7().to_string();
        self.client
            .create("Agents", json!({ "id": &agent_id }))
            .await?;
        self.client
            .action(
                "Agents",
                &agent_id,
                "Assign",
                json!({ "role": role, "goal": "interactive session", "model": model }),
            )
            .await?;
        self.client
            .action("Agents", &agent_id, "Start", json!({}))
            .await?;
        Ok(agent_id)
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
