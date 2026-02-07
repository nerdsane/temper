//! Agent orchestrator: interprets user requests via Claude, executes OData actions.

use serde_json::Value;

use temper_observe::clickhouse::{ClickHouseStore, SpanRecord};

use super::claude::{self, Message};
use super::client::TemperClient;

const SYSTEM_PROMPT: &str = r#"You are a customer service agent for an e-commerce platform powered by the Temper OData API.

Available operations (respond with EXACTLY one JSON object per turn):

1. Create an order:
   {"tool": "create_order"}

2. Get order state:
   {"tool": "get_order", "order_id": "<id>"}

3. Add item to order:
   {"tool": "add_item", "order_id": "<id>", "product_id": "<product>"}

4. Submit order:
   {"tool": "submit_order", "order_id": "<id>"}

5. Cancel order:
   {"tool": "cancel_order", "order_id": "<id>", "reason": "<reason>"}

6. Confirm order:
   {"tool": "confirm_order", "order_id": "<id>"}

7. Respond to user (no API call needed):
   {"tool": "respond", "message": "<your response to the user>"}

RULES:
- Always get the order state first to check its current status before acting
- Orders start in Draft. You must AddItem before SubmitOrder
- Cancel only works from Draft, Submitted, or Confirmed states
- If an action fails, explain why and suggest alternatives
- When you're done, use the "respond" tool with your final message

Respond with ONLY a JSON object. No markdown, no explanation outside the JSON."#;

/// A customer-facing agent that operates the e-commerce API using Claude.
pub struct CustomerAgent {
    client: TemperClient,
    api_key: String,
    model: String,
    conversation: Vec<Message>,
    /// The last order ID the agent interacted with.
    pub last_order_id: Option<String>,
    /// Trajectory trace ID for this conversation.
    trace_id: String,
    /// Current turn number.
    turn_number: u32,
    /// Optional ClickHouse store for trajectory capture.
    clickhouse: Option<ClickHouseStore>,
}

impl CustomerAgent {
    /// Create a new agent targeting the given server.
    pub fn new(server_url: &str, api_key: &str) -> Self {
        Self {
            client: TemperClient::new(server_url, "customer-agent-1"),
            api_key: api_key.to_string(),
            model: "claude-sonnet-4-5-20250929".to_string(),
            conversation: Vec::new(),
            last_order_id: None,
            trace_id: uuid::Uuid::now_v7().to_string(),
            turn_number: 0,
            clickhouse: None,
        }
    }

    /// Enable trajectory capture to ClickHouse.
    pub fn set_clickhouse(&mut self, url: &str) {
        self.clickhouse = Some(ClickHouseStore::new(url));
    }

    /// Process a user request. Returns the agent's response.
    pub async fn handle(&mut self, user_input: &str) -> String {
        self.turn_number += 1;
        let turn = self.turn_number;
        let start = chrono::Utc::now();

        self.conversation.push(Message {
            role: "user".to_string(),
            content: user_input.to_string(),
        });

        // Agent loop: call Claude, execute tool, feed result back, repeat
        for turn in 0..10 {
            let response = match claude::call_claude(
                &self.api_key,
                SYSTEM_PROMPT,
                &self.conversation,
                &self.model,
            )
            .await
            {
                Ok(r) => r,
                Err(e) => return format!("Agent error: {e}"),
            };

            tracing::info!(turn, response = %response, "claude response");

            // Parse the tool call — strip markdown code fences if present
            let json_str = extract_json(&response);
            let tool_call: Value = match serde_json::from_str(&json_str) {
                Ok(v) => v,
                Err(_) => {
                    // Claude didn't return valid JSON — treat as direct response
                    self.conversation.push(Message {
                        role: "assistant".to_string(),
                        content: response.clone(),
                    });
                    return response;
                }
            };

            let tool = tool_call.get("tool").and_then(|t| t.as_str()).unwrap_or("");

            match tool {
                "respond" => {
                    let msg = tool_call.get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("Done.")
                        .to_string();
                    self.conversation.push(Message {
                        role: "assistant".to_string(),
                        content: response,
                    });
                    let elapsed = (chrono::Utc::now() - start).num_nanoseconds().unwrap_or(0) as u64;
                    self.record_span("trajectory.complete", "ok", elapsed, user_input).await;
                    return msg;
                }

                "create_order" => {
                    let result = self.client.create_entity("Orders").await;
                    let (result_text, order_id) = match &result {
                        Ok(v) => {
                            let id = v.get("entity_id")
                                .and_then(|i| i.as_str())
                                .unwrap_or("unknown")
                                .to_string();
                            (format!("Order created: id={}, status={}", id, v.get("status").and_then(|s| s.as_str()).unwrap_or("?")), Some(id))
                        }
                        Err(e) => (format!("Error: {e}"), None),
                    };
                    if let Some(id) = order_id {
                        self.last_order_id = Some(id.clone());
                    }
                    let status = if result.is_ok() { "ok" } else { "error" };
                    self.record_span("odata.POST.CreateOrder", status, 0, user_input).await;
                    self.feed_tool_result(&response, &result_text);
                }

                "get_order" => {
                    let id = self.resolve_order_id(&tool_call);
                    let result = self.client.get_entity("Orders", &id).await;
                    let text = match result {
                        Ok(v) => format!("Order {}: status={}, items={}, events={}",
                            id,
                            v.get("status").and_then(|s| s.as_str()).unwrap_or("?"),
                            v.get("item_count").and_then(|i| i.as_u64()).unwrap_or(0),
                            v.get("events").and_then(|e| e.as_array()).map_or(0, |a| a.len()),
                        ),
                        Err(e) => format!("Error: {e}"),
                    };
                    self.feed_tool_result(&response, &text);
                }

                "add_item" => {
                    let id = self.resolve_order_id(&tool_call);
                    let product = tool_call.get("product_id")
                        .and_then(|p| p.as_str())
                        .unwrap_or("default-product");
                    let result = self.client.invoke_action(
                        "Orders", &id, "AddItem",
                        &serde_json::json!({"ProductId": product}),
                    ).await;
                    let text = match result {
                        Ok(v) => format!("Item added. items={}", v.get("item_count").and_then(|i| i.as_u64()).unwrap_or(0)),
                        Err(e) => format!("Error: {e}"),
                    };
                    self.feed_tool_result(&response, &text);
                }

                "submit_order" => {
                    let id = self.resolve_order_id(&tool_call);
                    let result = self.client.invoke_action(
                        "Orders", &id, "SubmitOrder",
                        &serde_json::json!({}),
                    ).await;
                    let text = match result {
                        Ok(v) => format!("Order submitted. status={}", v.get("status").and_then(|s| s.as_str()).unwrap_or("?")),
                        Err(e) => format!("Error: {e}"),
                    };
                    self.feed_tool_result(&response, &text);
                }

                "cancel_order" => {
                    let id = self.resolve_order_id(&tool_call);
                    let reason = tool_call.get("reason")
                        .and_then(|r| r.as_str())
                        .unwrap_or("customer request");
                    let result = self.client.invoke_action(
                        "Orders", &id, "CancelOrder",
                        &serde_json::json!({"Reason": reason}),
                    ).await;
                    let text = match result {
                        Ok(v) => format!("Order cancelled. status={}", v.get("status").and_then(|s| s.as_str()).unwrap_or("?")),
                        Err(e) => format!("Error: {e}"),
                    };
                    self.feed_tool_result(&response, &text);
                }

                "confirm_order" => {
                    let id = self.resolve_order_id(&tool_call);
                    let result = self.client.invoke_action(
                        "Orders", &id, "ConfirmOrder",
                        &serde_json::json!({}),
                    ).await;
                    let text = match result {
                        Ok(v) => format!("Order confirmed. status={}", v.get("status").and_then(|s| s.as_str()).unwrap_or("?")),
                        Err(e) => format!("Error: {e}"),
                    };
                    self.feed_tool_result(&response, &text);
                }

                _ => {
                    self.conversation.push(Message {
                        role: "assistant".to_string(),
                        content: response,
                    });
                    return format!("Unknown tool: {tool}");
                }
            }
        }

        "Agent reached max turns without responding.".to_string()
    }

    fn resolve_order_id(&self, tool_call: &Value) -> String {
        tool_call.get("order_id")
            .and_then(|i| i.as_str())
            .map(|s| s.to_string())
            .or_else(|| self.last_order_id.clone())
            .unwrap_or_else(|| "unknown".to_string())
    }

    fn feed_tool_result(&mut self, assistant_msg: &str, result: &str) {
        self.conversation.push(Message {
            role: "assistant".to_string(),
            content: assistant_msg.to_string(),
        });
        self.conversation.push(Message {
            role: "user".to_string(),
            content: format!("[Tool result]: {result}"),
        });
    }

    /// Record a trajectory span in ClickHouse (if configured).
    async fn record_span(&self, operation: &str, status: &str, duration_ns: u64, user_intent: &str) {
        let Some(ref ch) = self.clickhouse else { return };

        // ClickHouse DateTime64 needs "YYYY-MM-DD HH:MM:SS" format (no T, no timezone)
        let now = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
        let span = SpanRecord {
            trace_id: self.trace_id.clone(),
            span_id: uuid::Uuid::now_v7().to_string(),
            parent_span_id: None,
            service: "temper-agent".into(),
            operation: operation.into(),
            status: status.into(),
            duration_ns,
            start_time: now.clone(),
            end_time: now,
            attributes: serde_json::json!({
                "turn": self.turn_number,
                "user_intent": user_intent,
                "order_id": self.last_order_id,
            }).to_string(),
        };

        if let Err(e) = ch.insert_span(&span).await {
            tracing::warn!(error = %e, "failed to record trajectory span");
        }
    }
}

/// Strip markdown code fences and extract the JSON object from Claude's response.
fn extract_json(text: &str) -> String {
    let trimmed = text.trim();

    // Strip ```json ... ``` fences
    if let Some(start) = trimmed.find('{') {
        if let Some(end) = trimmed.rfind('}') {
            return trimmed[start..=end].to_string();
        }
    }

    trimmed.to_string()
}
