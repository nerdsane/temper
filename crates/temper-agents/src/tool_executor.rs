//! ToolExecutor integration actor — routes tool calls based on source.
//!
//! Routing (from ToolRegistry LookupTool reply):
//!   source = "client"  → Bus actor (Zenoh/Nexus → client-side execution)
//!   source = "mcp"     → ToolDriver (existing Rust MCP driver)
//!   source = "lassie"  → ToolDriver (existing Rust lassie callback driver)
//!   source = "builtin" → ToolDriver (in-process builtin)
//!
//! ToolRegistry is the single routing table — no hard-coded source logic here.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use temper_actor_runtime::bus::{BUS_ACTOR_TYPE, CallMsg, CallReply};
use temper_actor_runtime::spec_actor::SpecMessage;
use temper_actor_runtime::{Actor, ActorContext, ActorError, ActorHandle, Message};

use crate::common::{decode_params, message_action, session_id_from_namespace};

// ─── ToolDriver trait ─────────────────────────────────────────────────────────

/// Result of a single tool call.
#[derive(Debug, Clone)]
pub struct ToolResult {
    pub success: bool,
    pub output: String,
}

/// Pluggable tool driver for server-side tool dispatch.
/// Mirrors `domains/odp/apps/apis/temper/src/drivers::IntegrationDriver`.
/// The temper app injects its existing DriverRegistry at registration time.
#[async_trait]
pub trait ToolDriver: Send + Sync {
    /// Whether this driver handles the given source type.
    fn can_handle(&self, source: &str) -> bool;

    /// Execute the tool call.
    async fn dispatch(
        &self,
        tool_name: &str,
        tool_call_id: &str,
        input: &Value,
        driver_info: &Value,
    ) -> ToolResult;
}

// ─── Actor ────────────────────────────────────────────────────────────────────

pub struct ToolExecutorActor {
    driver: Option<Arc<dyn ToolDriver>>,
}

impl ToolExecutorActor {
    pub fn new() -> Self {
        Self { driver: None }
    }

    pub fn with_driver(driver: Arc<dyn ToolDriver>) -> Self {
        Self {
            driver: Some(driver),
        }
    }
}

impl Default for ToolExecutorActor {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Actor for ToolExecutorActor {
    fn actor_type(&self) -> &str {
        "ToolExecutorIntegration"
    }

    async fn handle(
        &self,
        ctx: &ActorContext,
        _state: &mut Vec<u8>,
        message: &Message,
    ) -> Result<(), ActorError> {
        if message_action(message) != "execute_tool_batch" {
            return Ok(());
        }

        let from = match &message.from {
            Some(f) => f.clone(),
            None => return Ok(()),
        };

        let params = decode_params(message);
        let tool_calls = match params["tool_calls"].as_array() {
            Some(calls) => calls.clone(),
            None => {
                ctx.tell(&from, SpecMessage::new("ToolCallBatchComplete"))
                    .await;
                return Ok(());
            }
        };

        let namespace = ctx.self_handle().namespace.clone();
        let session_id = session_id_from_namespace(&namespace).to_string();
        let registry = ActorHandle::new(namespace.clone(), "ToolRegistry");
        let bus = ActorHandle::new(namespace.clone(), BUS_ACTOR_TYPE);
        let mut results = Vec::new();

        for call in &tool_calls {
            let call_id = call["id"].as_str().unwrap_or("unknown").to_string();
            let tool_name = call["name"].as_str().unwrap_or("").to_string();
            let input = call["input"].clone();

            // ToolRegistry is the single routing table.
            let lookup_resp = ctx
                .ask(
                    &registry,
                    SpecMessage::with_params("LookupTool", json!({ "tool_name": tool_name })),
                    Duration::from_secs(5),
                )
                .await
                .map_err(|e| ActorError::HandlerFailed(format!("LookupTool: {e}")))?;

            let tool_info = lookup_resp
                .decode::<SpecMessage>()
                .ok()
                .and_then(|m| serde_json::from_slice::<Value>(&m.params).ok())
                .unwrap_or(json!({}));

            if let Some(err) = tool_info["error"].as_str() {
                tracing::warn!("ToolExecutor: {err}");
                results.push(json!({
                    "tool_use_id": call_id,
                    "type": "tool_result",
                    "content": format!("Tool not available: {err}")
                }));
                continue;
            }

            let source = tool_info["source"].as_str().unwrap_or("client");

            let output = match source {
                // Client-side tool → Bus actor.
                "client" => {
                    let client_id = tool_info["client_id"].as_str().unwrap_or("").to_string();
                    tracing::info!("ToolExecutor: {tool_name} → Bus (client={client_id})");
                    dispatch_via_bus(
                        ctx,
                        &bus,
                        &session_id,
                        &client_id,
                        &call_id,
                        &tool_name,
                        &input,
                    )
                    .await?
                }
                // Server-side tool → driver.
                _ => {
                    if let Some(driver) = &self.driver {
                        if driver.can_handle(source) {
                            tracing::info!("ToolExecutor: {tool_name} → driver (source={source})");
                            let result = driver
                                .dispatch(&tool_name, &call_id, &input, &tool_info)
                                .await;
                            if result.success {
                                result.output
                            } else {
                                format!("error: {}", result.output)
                            }
                        } else {
                            format!("no driver for source '{source}'")
                        }
                    } else {
                        format!(
                            "no driver registered for server-side tool '{tool_name}' (source={source})"
                        )
                    }
                }
            };

            results.push(json!({
                "tool_use_id": call_id,
                "type": "tool_result",
                "content": output
            }));
        }

        ctx.tell(
            &from,
            SpecMessage::with_params("ToolCallBatchComplete", json!({ "tool_results": results })),
        )
        .await;

        Ok(())
    }
}

async fn dispatch_via_bus(
    ctx: &ActorContext,
    bus: &ActorHandle,
    session_id: &str,
    client_id: &str,
    call_id: &str,
    tool_name: &str,
    input: &Value,
) -> Result<String, ActorError> {
    let payload = serde_json::to_vec(&json!({
        "call_id": call_id,
        "tool": tool_name,
        "args": input,
    }))
    .unwrap();

    let reply = ctx
        .ask(
            bus,
            CallMsg {
                session_id: session_id.to_string(),
                client_id: client_id.to_string(),
                payload,
            },
            Duration::from_secs(30),
        )
        .await
        .map_err(|e| ActorError::HandlerFailed(format!("Bus.Call: {e}")))?;

    let call_reply = reply
        .decode::<CallReply>()
        .map_err(|e| ActorError::HandlerFailed(format!("decode CallReply: {e}")))?;

    let response: Value = serde_json::from_slice(&call_reply.payload).unwrap_or_default();
    Ok(response["output"]
        .as_str()
        .unwrap_or("(no output)")
        .to_string())
}
