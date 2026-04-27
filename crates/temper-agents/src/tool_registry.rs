//! ToolRegistry actor — single source of truth for tool routing.
//!
//! Stores per-tool routing info: source (client/mcp/lassie/builtin) + metadata.
//! Tools are registered via OData POST /tdata/ToolDefinition, which forwards
//! to this actor. LookupTool returns everything ToolExecutorActor needs.
//!
//! State shape:
//! {
//!   "tools": {
//!     "bash":        { "source": "client", "client_id": "abc-123" },
//!     "search_logs": { "source": "mcp",    "mcp_url": "https://..." },
//!     "memory":      { "source": "builtin" }
//!   }
//! }

use async_trait::async_trait;
use serde_json::{Value, json};
use temper_actor_runtime::spec_actor::SpecMessage;
use temper_actor_runtime::{Actor, ActorContext, ActorError, Message};

use crate::common::{decode_params, message_action};

pub struct ToolRegistryActor;

#[async_trait]
impl Actor for ToolRegistryActor {
    fn actor_type(&self) -> &str {
        "ToolRegistry"
    }

    fn initial_state(&self) -> Vec<u8> {
        serde_json::to_vec(&json!({ "tools": {} })).unwrap()
    }

    async fn handle(
        &self,
        ctx: &ActorContext,
        state: &mut Vec<u8>,
        message: &Message,
    ) -> Result<(), ActorError> {
        let mut s: Value = serde_json::from_slice(state).unwrap_or(json!({ "tools": {} }));
        let params = decode_params(message);

        match message_action(message).as_str() {
            // Client registers tools: { client_id, tool_names: ["bash", ...] }
            "RegisterTool" => {
                let client_id = params["client_id"].as_str().unwrap_or("").to_string();
                if let Some(names) = params["tool_names"].as_array() {
                    for tool in names {
                        if let Some(name) = tool.as_str() {
                            s["tools"][name] = json!({
                                "source": "client",
                                "client_id": client_id,
                            });
                        }
                    }
                }
                tracing::info!(
                    "ToolRegistry: registered client tools {:?} for client {client_id}",
                    params["tool_names"]
                );
                ctx.reply(
                    message,
                    SpecMessage::with_params("ToolRegistered", json!({ "client_id": client_id })),
                )
                .await;
            }

            // Server registers a tool (MCP, lassie, builtin):
            // { name, source: "mcp"|"lassie"|"builtin", json_schema?, ...driver_info }
            "RegisterServerTool" => {
                let name = params["name"].as_str().unwrap_or("").to_string();
                let source = params["source"].as_str().unwrap_or("builtin").to_string();
                if name.is_empty() {
                    return Ok(());
                }
                let mut entry = params.clone();
                entry["source"] = json!(source);
                entry.as_object_mut().map(|o| o.remove("name"));
                s["tools"][&name] = entry;
                tracing::info!("ToolRegistry: registered server tool {name} (source={source})");
            }

            // Returns all registered tools with their metadata (name, source, json_schema?).
            // Used by LLM actor to build the tool list for the LLM API.
            "GetAllTools" => {
                let tools: Vec<Value> = s["tools"]
                    .as_object()
                    .map(|m| {
                        m.iter()
                            .map(|(name, info)| {
                                let mut t = info.clone();
                                t["name"] = json!(name);
                                t
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                ctx.reply(
                    message,
                    SpecMessage::with_params("AllTools", json!({ "tools": tools })),
                )
                .await;
            }

            // { tool_name } → { source, client_id?, ...driver_info } or { error }
            "LookupTool" => {
                let tool_name = params["tool_name"].as_str().unwrap_or("");
                let reply = if s["tools"][tool_name].is_object() {
                    let mut info = s["tools"][tool_name].clone();
                    info["tool_name"] = json!(tool_name);
                    info
                } else {
                    json!({ "error": format!("tool '{tool_name}' not registered") })
                };
                ctx.reply(message, SpecMessage::with_params("ToolInfo", reply))
                    .await;
            }

            // { client_id } — remove all tools for a disconnected client
            "UnregisterClient" => {
                let client_id = params["client_id"].as_str().unwrap_or("");
                if let Some(obj) = s["tools"].as_object_mut() {
                    obj.retain(|_, v| v["client_id"].as_str() != Some(client_id));
                }
                tracing::info!("ToolRegistry: unregistered client {client_id}");
            }

            _ => {}
        }

        *state = serde_json::to_vec(&s).unwrap();
        Ok(())
    }
}
