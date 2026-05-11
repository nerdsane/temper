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
        let process = ActorHandle::new(namespace.clone(), "Process");
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
                // Builtin process primitives.
                "builtin" => dispatch_builtin(ctx, &process, &tool_name, &input).await,
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

async fn dispatch_builtin(
    ctx: &ActorContext,
    process: &ActorHandle,
    tool_name: &str,
    input: &Value,
) -> String {
    match tool_name {
        "spawn_child" => {
            let prompt = input["prompt"].as_str().unwrap_or("").to_string();
            let tool_names = input["tool_names"]
                .as_array()
                .cloned()
                .unwrap_or_else(|| vec![json!("bash")]);
            let parent_id = session_id_from_namespace(&ctx.self_handle().namespace).to_string();
            let child_process_id = format!("{parent_id}-child-{}", uuid::Uuid::new_v4());
            // Clear stale child_result on parent before spawning a new child.
            if let Some(bytes) = ctx
                .load_actor_state(&ctx.self_handle().namespace, "Process")
                .await
                .ok()
                .flatten()
                && let Ok(mut state) = serde_json::from_slice::<
                    temper_actor_runtime::spec_actor::SpecActorState,
                >(&bytes)
            {
                if let Some(obj) = state.fields.as_object_mut() {
                    obj.remove("child_result");
                }
                if let Ok(new_bytes) = serde_json::to_vec(&state) {
                    let _ = ctx
                        .upsert_actor_state(&ctx.self_handle().namespace, "Process", new_bytes)
                        .await;
                }
            }
            ctx.tell(
                process,
                SpecMessage::with_params(
                    "SpawnChild",
                    json!({
                        "child_definition_id": "bits-agent",
                        "child_user_prompt": prompt,
                        "child_process_id": child_process_id,
                        "tool_names": tool_names,
                    }),
                ),
            )
            .await;
            serde_json::json!({
                "status": "spawned",
                "child_process_id": child_process_id,
            })
            .to_string()
        }
        "wait_child" => {
            let ns = ctx.self_handle().namespace.clone();
            let tenant = ns.split('/').next().unwrap_or("default").to_string();

            // If the caller provided a child_id, check child state directly first.
            if let Some(child_id) = input["child_id"].as_str().filter(|s| !s.is_empty()) {
                let child_ns = format!("{tenant}/{child_id}");
                if let Some(child_state) = ctx
                    .load_actor_state(&child_ns, "Process")
                    .await
                    .ok()
                    .flatten()
                    .and_then(|b| {
                        serde_json::from_slice::<temper_actor_runtime::spec_actor::SpecActorState>(
                            &b,
                        )
                        .ok()
                    })
                    && matches!(
                        child_state.status.as_str(),
                        "Ready" | "Failed" | "Terminated"
                    )
                {
                    return serde_json::json!({
                        "child_process_id": child_id,
                        "status": child_state.status,
                        "result": child_state.fields,
                    })
                    .to_string();
                }
            }

            // If parent already has child_result, return immediately.
            let state = ctx
                .load_actor_state(&ns, "Process")
                .await
                .ok()
                .flatten()
                .and_then(|b| {
                    serde_json::from_slice::<temper_actor_runtime::spec_actor::SpecActorState>(&b)
                        .ok()
                })
                .unwrap_or_default();
            if !state.fields["child_result"].is_null() {
                return serde_json::to_string(&state.fields["child_result"]).unwrap_or_default();
            }

            // Otherwise block parent and wait for child_result to arrive.
            ctx.tell(process, SpecMessage::new("BlockOnChild")).await;
            let timeout = std::time::Duration::from_secs(
                std::env::var("WAIT_CHILD_TIMEOUT_SECS")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(300),
            );
            let start = std::time::Instant::now();
            while start.elapsed() < timeout {
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                let state = ctx
                    .load_actor_state(&ns, "Process")
                    .await
                    .ok()
                    .flatten()
                    .and_then(|b| {
                        serde_json::from_slice::<temper_actor_runtime::spec_actor::SpecActorState>(
                            &b,
                        )
                        .ok()
                    })
                    .unwrap_or_default();
                if !state.fields["child_result"].is_null() {
                    return serde_json::to_string(&state.fields["child_result"])
                        .unwrap_or_default();
                }
            }
            "timeout waiting for child process".to_string()
        }
        "sleep" => {
            let seconds = input["seconds"].as_u64().unwrap_or(1);
            ctx.tell(
                process,
                SpecMessage::with_params("SleepProcess", json!({ "sleep_seconds": seconds })),
            )
            .await;
            format!("sleep scheduled for {seconds}s")
        }
        "get_process_status" => {
            let Some(process_id) = input["process_id"].as_str() else {
                return "error: process_id is required".to_string();
            };
            get_process_state(ctx, process_id, false).await
        }
        "get_process_output" => {
            let Some(process_id) = input["process_id"].as_str() else {
                return "error: process_id is required".to_string();
            };
            get_process_state(ctx, process_id, true).await
        }
        _ => format!("unknown builtin tool '{tool_name}'"),
    }
}

async fn get_process_state(ctx: &ActorContext, process_id: &str, output_only: bool) -> String {
    let ns = ctx.self_handle().namespace.clone();
    let tenant = ns.split('/').next().unwrap_or("default");
    let target_ns = format!("{tenant}/{process_id}");
    let Some(state) = ctx
        .load_actor_state(&target_ns, "Process")
        .await
        .ok()
        .flatten()
        .and_then(|b| {
            serde_json::from_slice::<temper_actor_runtime::spec_actor::SpecActorState>(&b).ok()
        })
    else {
        return format!("error: process '{process_id}' not found");
    };

    if output_only {
        let output = if !state.fields["response"].is_null() {
            state.fields["response"].clone()
        } else if !state.fields["error"].is_null() {
            state.fields["error"].clone()
        } else if !state.fields["child_result"].is_null() {
            state.fields["child_result"].clone()
        } else {
            serde_json::json!(null)
        };
        return serde_json::json!({
            "process_id": process_id,
            "status": state.status,
            "output": output,
        })
        .to_string();
    }

    serde_json::json!({
        "process_id": process_id,
        "status": state.status,
        "counters": state.counters,
        "fields": state.fields,
    })
    .to_string()
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
    .map_err(|e| ActorError::HandlerFailed(format!("serialize bus call payload: {e}")))?;

    let timeout_secs: u64 = std::env::var("CLIENT_TOOL_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(180);

    let reply = match ctx
        .ask(
            bus,
            CallMsg {
                session_id: session_id.to_string(),
                client_id: client_id.to_string(),
                payload,
            },
            Duration::from_secs(timeout_secs),
        )
        .await
    {
        Ok(reply) => reply,
        Err(e) => {
            // Tool timeouts are tool failures, not actor failures. Return an
            // error result so actor activation commits and we don't retry the
            // same local side effect forever.
            return Ok(format!("error: tool call timed out or failed: {e}"));
        }
    };

    let call_reply = reply
        .decode::<CallReply>()
        .map_err(|e| ActorError::HandlerFailed(format!("decode CallReply: {e}")))?;

    let response: Value = serde_json::from_slice(&call_reply.payload).unwrap_or_default();
    Ok(response["output"]
        .as_str()
        .unwrap_or("(no output)")
        .to_string())
}
