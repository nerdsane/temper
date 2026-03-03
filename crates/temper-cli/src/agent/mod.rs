//! `temper agent` — entity-native agent built on Temper entities.
//!
//! The agent loop creates and transitions Temper entities (Agent, ToolCall)
//! while executing tools locally. All durable state lives on the server.
//! The CLI is stateless — crash and restart with `--agent-id` to resume.

pub mod llm;
pub mod tools;

use std::time::Instant; // determinism-ok: CLI code, not simulation-visible

use anyhow::{Context, Result};
use serde_json::json;

use self::llm::{AnthropicClient, ContentBlock, Message};
use self::tools::{ToolResult, tool_definitions, tool_to_cedar};

/// HTTP client wrapper for Temper server OData + API calls.
struct TemperClient {
    client: reqwest::Client,
    base_url: String,
    tenant: String,
}

/// Authorization response from POST /api/authorize.
struct AuthzResponse {
    allowed: bool,
    decision_id: Option<String>,
    #[allow(dead_code)]
    reason: Option<String>,
}

impl TemperClient {
    fn new(port: u16, tenant: &str) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: format!("http://127.0.0.1:{port}"),
            tenant: tenant.to_string(),
        }
    }

    /// Create an Agent entity and transition it to Working.
    async fn create_agent(&self, id: &str, role: &str, goal: &str, model: &str) -> Result<()> {
        // Create entity.
        let url = format!("{}/tdata/Agents", self.base_url);
        let body = json!({ "id": id });
        self.client
            .post(&url)
            .header("x-tenant-id", &self.tenant)
            .json(&body)
            .send()
            .await
            .context("Failed to create Agent entity")?;

        // Assign.
        self.agent_action(
            id,
            "Assign",
            json!({ "role": role, "goal": goal, "model": model }),
        )
        .await?;

        // Start.
        self.agent_action(id, "Start", json!({})).await?;

        Ok(())
    }

    /// Get an Agent entity's current state.
    async fn get_agent(&self, id: &str) -> Result<serde_json::Value> {
        let url = format!("{}/tdata/Agents('{id}')", self.base_url);
        let resp = self
            .client
            .get(&url)
            .header("x-tenant-id", &self.tenant)
            .send()
            .await
            .context("Failed to get Agent entity")?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Failed to get Agent ({status}): {body}");
        }

        resp.json().await.context("Failed to parse Agent response")
    }

    /// Invoke an action on the Agent entity.
    async fn agent_action(
        &self,
        id: &str,
        action: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let url = format!("{}/tdata/Agents('{id}')/Temper.{action}", self.base_url);
        let resp = self
            .client
            .post(&url)
            .header("x-tenant-id", &self.tenant)
            .json(&params)
            .send()
            .await
            .with_context(|| format!("Failed to invoke Agent.{action}"))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Agent.{action} failed ({status}): {body}");
        }

        resp.json()
            .await
            .with_context(|| format!("Failed to parse Agent.{action} response"))
    }

    /// Create a ToolCall entity in Pending state.
    async fn create_tool_call(
        &self,
        agent_id: &str,
        tool_name: &str,
        tool_input: &serde_json::Value,
        resource_type: &str,
        resource_id: &str,
    ) -> Result<String> {
        let tc_id = uuid::Uuid::now_v7().to_string();
        let url = format!("{}/tdata/ToolCalls", self.base_url);
        let body = json!({
            "id": tc_id,
            "agent_id": agent_id,
            "tool_name": tool_name,
            "tool_input": serde_json::to_string(tool_input).unwrap_or_default(),
            "resource_type": resource_type,
            "resource_id": resource_id,
        });
        let resp = self
            .client
            .post(&url)
            .header("x-tenant-id", &self.tenant)
            .json(&body)
            .send()
            .await
            .context("Failed to create ToolCall entity")?;

        let status = resp.status();
        if !status.is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Create ToolCall failed ({status}): {body_text}");
        }

        Ok(tc_id)
    }

    /// Invoke an action on a ToolCall entity.
    async fn tool_call_action(
        &self,
        id: &str,
        action: &str,
        params: serde_json::Value,
    ) -> Result<()> {
        let url = format!("{}/tdata/ToolCalls('{id}')/Temper.{action}", self.base_url);
        let resp = self
            .client
            .post(&url)
            .header("x-tenant-id", &self.tenant)
            .json(&params)
            .send()
            .await
            .with_context(|| format!("Failed to invoke ToolCall.{action}"))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("ToolCall.{action} failed ({status}): {body}");
        }

        Ok(())
    }

    /// Check Cedar authorization via POST /api/authorize.
    async fn authorize(
        &self,
        agent_id: &str,
        action: &str,
        resource_type: &str,
        resource_id: &str,
    ) -> Result<AuthzResponse> {
        let url = format!("{}/api/authorize", self.base_url);
        let body = json!({
            "agent_id": agent_id,
            "action": action,
            "resource_type": resource_type,
            "resource_id": resource_id,
        });

        let resp = self
            .client
            .post(&url)
            .header("x-tenant-id", &self.tenant)
            .header("x-temper-principal-id", agent_id)
            .header("x-temper-principal-kind", "Agent")
            .json(&body)
            .send()
            .await
            .context("Failed to call /api/authorize")?;

        let resp_json: serde_json::Value = resp
            .json()
            .await
            .context("Failed to parse authorize response")?;

        Ok(AuthzResponse {
            allowed: resp_json
                .get("allowed")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            decision_id: resp_json
                .get("decision_id")
                .and_then(|v| v.as_str())
                .map(String::from),
            reason: resp_json
                .get("reason")
                .and_then(|v| v.as_str())
                .map(String::from),
        })
    }

    /// Poll for a pending decision to be resolved.
    ///
    /// Budget: 300 polls at 2-second intervals = 10 minutes max wait.
    async fn poll_decision(&self, decision_id: &str) -> Result<bool> {
        let url = format!(
            "{}/api/tenants/{}/decisions?status=pending",
            self.base_url, self.tenant
        );

        let max_polls: u32 = 300; // 10 minutes at 2-second intervals
        for _attempt in 0..max_polls {
            let resp = self
                .client
                .get(&url)
                .header("Accept", "application/json")
                .send()
                .await
                .context("Failed to poll decisions")?;

            let body: serde_json::Value = resp.json().await.context("Failed to parse decisions")?;

            let decisions = body
                .get("decisions")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();

            // Check if our decision is still pending.
            let still_pending = decisions
                .iter()
                .any(|d| d.get("id").and_then(|v| v.as_str()) == Some(decision_id));

            if !still_pending {
                // Decision was resolved — check if approved.
                let all_url = format!("{}/api/tenants/{}/decisions", self.base_url, self.tenant);
                let all_resp = self
                    .client
                    .get(&all_url)
                    .header("Accept", "application/json")
                    .send()
                    .await
                    .context("Failed to fetch all decisions")?;
                let all_body: serde_json::Value = all_resp
                    .json()
                    .await
                    .context("Failed to parse all decisions")?;
                let all_decisions = all_body
                    .get("decisions")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                let our_decision = all_decisions
                    .iter()
                    .find(|d| d.get("id").and_then(|v| v.as_str()) == Some(decision_id));
                if let Some(d) = our_decision {
                    let status = d.get("status").and_then(|v| v.as_str()).unwrap_or("");
                    return Ok(status == "approved" || status == "Approved");
                }
                // Decision not found at all — treat as denied.
                return Ok(false);
            }

            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }

        anyhow::bail!("Timed out waiting for decision {decision_id} (10 minutes)")
    }
}

/// Execute a single tool call through the entity lifecycle.
async fn execute_tool_call(
    client: &TemperClient,
    agent_id: &str,
    tool_name: &str,
    tool_input: &serde_json::Value,
) -> Result<ToolResult> {
    let cedar = tool_to_cedar(tool_name, tool_input);

    // 1. Create ToolCall entity (Pending).
    let tc_id = client
        .create_tool_call(
            agent_id,
            tool_name,
            tool_input,
            &cedar.resource_type,
            &cedar.resource_id,
        )
        .await?;

    // 2. Authorize via Cedar.
    let authz = client
        .authorize(
            agent_id,
            &cedar.action,
            &cedar.resource_type,
            &cedar.resource_id,
        )
        .await?;

    if !authz.allowed {
        let decision_id = authz.decision_id.unwrap_or_default();
        client
            .tool_call_action(&tc_id, "Deny", json!({ "decision_id": decision_id }))
            .await?;
        return Ok(ToolResult::Denied {
            decision_id,
            tool_call_id: tc_id,
        });
    }

    // Authorized.
    client
        .tool_call_action(&tc_id, "Authorize", json!({}))
        .await?;

    // 3. Execute locally.
    client
        .tool_call_action(&tc_id, "Execute", json!({}))
        .await?;

    let start = Instant::now(); // determinism-ok: CLI code
    let result = tools::execute_tool(tool_name, tool_input).await;
    let duration_ms = start.elapsed().as_millis() as u64;

    // 4. Record result.
    match &result {
        Ok(output) => {
            // Truncate result for entity storage (keep first ~4KB, UTF-8 safe).
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
            client
                .tool_call_action(
                    &tc_id,
                    "Complete",
                    json!({ "result": truncated, "duration_ms": duration_ms.to_string() }),
                )
                .await?;
            Ok(ToolResult::Success(output.clone()))
        }
        Err(e) => {
            client
                .tool_call_action(&tc_id, "Fail", json!({ "error": e.to_string() }))
                .await?;
            Ok(ToolResult::Error(e.to_string()))
        }
    }
}

/// Run the `temper agent` command.
pub async fn run(
    port: u16,
    tenant: &str,
    goal: &str,
    role: &str,
    model: &str,
    agent_id: Option<String>,
) -> Result<()> {
    let client = TemperClient::new(port, tenant);
    let llm = AnthropicClient::new(model)?;
    let tool_defs = tool_definitions();

    // Create or resume agent.
    let agent_id = match agent_id {
        Some(id) => {
            println!("Resuming agent: {id}");
            let agent = client.get_agent(&id).await?;
            let status = agent.get("Status").and_then(|v| v.as_str()).unwrap_or("");
            println!("  Status: {status}");
            id
        }
        None => {
            let id = uuid::Uuid::now_v7().to_string();
            println!("Creating agent: {id}");
            client.create_agent(&id, role, goal, model).await?;
            println!("  Role:  {role}");
            println!("  Goal:  {goal}");
            println!("  Model: {model}");
            id
        }
    };

    println!();

    // Build system prompt.
    let system_prompt = format!(
        "You are a Temper agent with role '{role}'. Your goal: {goal}\n\n\
         You have tools to interact with the filesystem and execute shell commands. \
         Use them to accomplish your goal. When done, provide a summary of what you accomplished."
    );

    // Initialize or resume conversation.
    let mut messages: Vec<Message> = Vec::new();

    // Check for resumed conversation.
    if let Ok(agent) = client.get_agent(&agent_id).await {
        if let Some(conv) = agent.get("conversation").and_then(|v| v.as_str()) {
            if !conv.is_empty() {
                if let Ok(restored) = serde_json::from_str::<Vec<Message>>(conv) {
                    println!("Restored conversation ({} messages)", restored.len());
                    messages = restored;
                }
            }
        }
    }

    // If no conversation history, start with the user goal.
    if messages.is_empty() {
        messages.push(Message {
            role: "user".to_string(),
            content: vec![ContentBlock::Text {
                text: goal.to_string(),
            }],
        });
    }

    // Agent loop.
    let max_turns = 50;
    for turn in 0..max_turns {
        // Call LLM.
        let response = llm.send(&system_prompt, &messages, &tool_defs).await?;

        // Print text content.
        for block in &response.content {
            if let ContentBlock::Text { text } = block {
                println!("{text}");
            }
        }

        // Add assistant response to conversation.
        messages.push(Message {
            role: "assistant".to_string(),
            content: response.content.clone(),
        });

        // Check for tool use.
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
            // No tool calls — agent is done.
            let result_text = response
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");

            // Checkpoint and complete.
            let conv_json = serde_json::to_string(&messages).unwrap_or_default();
            if let Err(e) = client
                .agent_action(
                    &agent_id,
                    "Checkpoint",
                    json!({ "conversation": conv_json }),
                )
                .await
            {
                eprintln!("Warning: checkpoint failed: {e}");
            }
            if let Err(e) = client
                .agent_action(
                    &agent_id,
                    "Complete",
                    json!({ "result": result_text.chars().take(2000).collect::<String>() }),
                )
                .await
            {
                eprintln!("Warning: complete transition failed: {e}");
            }

            println!("\nAgent completed.");
            return Ok(());
        }

        // Execute tool calls.
        let mut tool_results = Vec::new();
        for (tool_use_id, tool_name, tool_input) in &tool_uses {
            println!("\n  Tool: {tool_name}");

            let result = execute_tool_call(&client, &agent_id, tool_name, tool_input).await?;

            match result {
                ToolResult::Success(output) => {
                    let display = if output.len() > 200 {
                        let boundary = output
                            .char_indices()
                            .take_while(|(i, _)| *i < 200)
                            .last()
                            .map(|(i, c)| i + c.len_utf8())
                            .unwrap_or(0);
                        format!("{}...", &output[..boundary])
                    } else {
                        output.clone()
                    };
                    println!("  Result: {display}");
                    tool_results.push(ContentBlock::ToolResult {
                        tool_use_id: tool_use_id.clone(),
                        content: output,
                        is_error: None,
                    });
                }
                ToolResult::Error(err) => {
                    println!("  Error: {err}");
                    tool_results.push(ContentBlock::ToolResult {
                        tool_use_id: tool_use_id.clone(),
                        content: format!("Error: {err}"),
                        is_error: Some(true),
                    });
                }
                ToolResult::Denied {
                    decision_id,
                    tool_call_id,
                } => {
                    println!("  Blocked: Cedar denied. Decision {decision_id} pending approval.");
                    println!("  Use `temper decide` to approve or deny.");

                    // Transition agent to Blocked.
                    let reason =
                        format!("Tool '{tool_name}' denied. Decision {decision_id} pending.");
                    if let Err(e) = client
                        .agent_action(&agent_id, "Block", json!({ "reason": reason }))
                        .await
                    {
                        eprintln!("Warning: block transition failed: {e}");
                    }

                    // Poll for decision.
                    println!("  Waiting for decision...");
                    let approved = client.poll_decision(&decision_id).await?;

                    if approved {
                        println!("  Decision approved — retrying tool call.");
                        // Retry: ToolCall.Retry → Pending.
                        if let Err(e) = client
                            .tool_call_action(&tool_call_id, "Retry", json!({}))
                            .await
                        {
                            eprintln!("Warning: ToolCall retry transition failed: {e}");
                        }
                        // Unblock agent.
                        if let Err(e) = client.agent_action(&agent_id, "Unblock", json!({})).await {
                            eprintln!("Warning: unblock transition failed: {e}");
                        }

                        // Re-execute the tool call.
                        let retry_result =
                            execute_tool_call(&client, &agent_id, tool_name, tool_input).await?;
                        match retry_result {
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
                            ToolResult::Denied { .. } => {
                                // Still denied after retry — fail.
                                if let Err(e) = client
                                    .agent_action(
                                        &agent_id,
                                        "Fail",
                                        json!({ "reason": "Tool call denied after retry" }),
                                    )
                                    .await
                                {
                                    eprintln!("Warning: fail transition failed: {e}");
                                }
                                anyhow::bail!("Tool call denied after retry");
                            }
                        }
                    } else {
                        // Human denied — fail agent.
                        println!("  Decision denied by human.");
                        if let Err(e) = client
                            .agent_action(
                                &agent_id,
                                "Fail",
                                json!({ "reason": format!("Tool '{tool_name}' denied by human") }),
                            )
                            .await
                        {
                            eprintln!("Warning: fail transition failed: {e}");
                        }
                        anyhow::bail!("Agent failed: tool '{tool_name}' denied by human");
                    }
                }
            }
        }

        // Add tool results to conversation.
        messages.push(Message {
            role: "user".to_string(),
            content: tool_results,
        });

        // Checkpoint conversation after each turn.
        let conv_json = serde_json::to_string(&messages).unwrap_or_default();
        if let Err(e) = client
            .agent_action(
                &agent_id,
                "Checkpoint",
                json!({ "conversation": conv_json }),
            )
            .await
        {
            eprintln!("Warning: checkpoint failed: {e}");
        }

        if turn == max_turns - 1 {
            println!("\nMax turns ({max_turns}) reached.");
            if let Err(e) = client
                .agent_action(
                    &agent_id,
                    "Complete",
                    json!({ "result": "Max turns reached" }),
                )
                .await
            {
                eprintln!("Warning: complete transition failed: {e}");
            }
        }
    }

    Ok(())
}
