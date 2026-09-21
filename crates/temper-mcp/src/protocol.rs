//! JSON-RPC protocol handlers and tool schema.

use anyhow::anyhow;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::runtime::ClientInfo;
use super::{
    MCP_LATEST_PROTOCOL_VERSION, MCP_PROTOCOL_VERSION, MCP_SERVER_NAME, RuntimeContext,
    SUPPORTED_PROTOCOL_VERSIONS,
};

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    #[allow(dead_code)]
    jsonrpc: Option<String>,
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Serialize)]
struct JsonRpcError {
    code: i64,
    message: String,
}

#[derive(Debug, Deserialize)]
struct ToolCallParams {
    name: String,
    #[serde(default)]
    arguments: Value,
}

pub(super) async fn dispatch_json_value(ctx: &mut RuntimeContext, raw: Value) -> Option<Value> {
    let request: JsonRpcRequest = match serde_json::from_value(raw) {
        Ok(value) => value,
        Err(error) => {
            return Some(json_rpc_error(
                None,
                -32600,
                format!("invalid request: {error}"),
            ));
        }
    };

    let id = request.id.clone();

    let result = match request.method.as_str() {
        "initialize" => {
            // Extract clientInfo from the initialize params (MCP spec).
            if let Some(client_info) = request.params.get("clientInfo") {
                let name = client_info
                    .get("name")
                    .and_then(Value::as_str)
                    .map(String::from);
                let version = client_info
                    .get("version")
                    .and_then(Value::as_str)
                    .map(String::from);
                if let Err(e) = ctx.apply_client_info(ClientInfo { name, version }).await {
                    tracing::error!("Failed to apply client info: {e}");
                }
            }

            let requested_version = request
                .params
                .get("protocolVersion")
                .and_then(Value::as_str);
            let protocol_version = negotiate_protocol_version(requested_version);
            ctx.client_supports_elicitation = protocol_version == MCP_LATEST_PROTOCOL_VERSION
                && request
                    .params
                    .pointer("/capabilities/elicitation")
                    .is_some_and(Value::is_object);
            ctx.init_trajectory();
            Ok(json!({
                "protocolVersion": protocol_version,
                "capabilities": {
                    "tools": {
                        "listChanged": false
                    }
                },
                "serverInfo": {
                    "name": MCP_SERVER_NAME,
                    "version": env!("CARGO_PKG_VERSION")
                },
                "instructions": "Temper is an operating layer for governed applications, not a general-purpose API. \
            When you need a capability (weather, task management, etc.), generate an IOA spec \
            that declares [[integration]] sections for external APIs, then submit it via \
            the execute tool. Use execute to start the server, submit specs, create entities, \
            and invoke actions — all governed by Cedar policies. If an action is denied, the \
            decision surfaces to the human developer for approval. When the tool result \
            carries an `approval` annotation, a human already resolved the decision inline: \
            if granted, re-invoke the original action; if denied, do not retry it."
            }))
        }
        "tools/list" => Ok(json!({ "tools": tool_definitions() })),
        "tools/call" => {
            let params: ToolCallParams = match serde_json::from_value(request.params) {
                Ok(value) => value,
                Err(error) => {
                    return Some(json_rpc_error(
                        id,
                        -32602,
                        format!("invalid tools/call params: {error}"),
                    ));
                }
            };

            if matches!(
                params.name.as_str(),
                "request_policy_replacement" | "request_policy_amendment"
            ) {
                let id = id?;
                let result = if params.name == "request_policy_amendment" {
                    crate::policy_replacement::amendment::request_policy_amendment(
                        ctx,
                        &params.arguments,
                    )
                    .await
                } else {
                    crate::policy_replacement::request_policy_replacement(ctx, &params.arguments)
                        .await
                };
                ctx.record_execute_turn(&params.name, &result);
                let (text, is_error) = match result {
                    Ok(text) => (text, false),
                    Err(error) => (error.to_string(), true),
                };
                return Some(json!({"jsonrpc":"2.0", "id":id, "result": {
                    "content":[{"type":"text", "text":text}], "isError":is_error
                }}));
            }

            if params.name == "setup_connection" {
                let id = id?; // Notifications cannot initiate human administration.
                let result = crate::setup::setup_connection(ctx, &params.arguments).await;
                ctx.record_execute_turn("setup_connection", &result);
                let (text, is_error) = match result {
                    Ok(text) => (text, false),
                    Err(error) => (error.to_string(), true),
                };
                return Some(json!({"jsonrpc":"2.0", "id":id, "result": {
                    "content":[{"type":"text", "text":text}], "isError":is_error
                }}));
            }

            let code = match params.arguments.get("code").and_then(Value::as_str) {
                Some(code) => code,
                None => {
                    return Some(json_rpc_error(
                        id,
                        -32602,
                        "tools/call missing required `arguments.code` string".to_string(),
                    ));
                }
            };

            let (tool_result, denials) = match params.name.as_str() {
                "execute" => {
                    if is_flush_trajectory_request(code) {
                        let flushed = ctx.flush_trajectory().await.map(|trajectory_id| {
                            json!({
                                "trajectory_id": trajectory_id,
                                "status": "flushed",
                            })
                            .to_string()
                        });
                        (flushed, Vec::new())
                    } else {
                        ctx.run_execute(code).await
                    }
                }
                other => (Err(anyhow!(format!("unknown tool '{other}'"))), Vec::new()),
            };

            // If Cedar denied a call and the client supports elicitation,
            // offer the decision to the human before returning (ADR-0173).
            let tool_result =
                crate::elicit::apply_denial_elicitation(ctx, tool_result, denials).await;
            // Include the human outcome in the same recorded tool turn.
            ctx.record_execute_turn(code, &tool_result);

            Ok(match tool_result {
                Ok(text) => json!({
                    "content": [{"type": "text", "text": text}],
                    "isError": false
                }),
                Err(error) => json!({
                    "content": [{"type": "text", "text": error.to_string()}],
                    "isError": true
                }),
            })
        }
        "ping" => Ok(json!({})),
        "initialized" | "notifications/initialized" => {
            // Notification-style methods intentionally produce no response.
            return None;
        }
        method => Err(anyhow!(format!("method not found: {method}"))),
    };

    // Notifications (no id) do not require a response.
    let response_id = id?;

    Some(match result {
        Ok(payload) => json!({
            "jsonrpc": "2.0",
            "id": response_id,
            "result": payload,
        }),
        Err(error) => {
            let (code, message) = if error.to_string().starts_with("method not found") {
                (-32601, error.to_string())
            } else {
                (-32602, error.to_string())
            };
            json_rpc_error(Some(response_id), code, message)
        }
    })
}

/// Echo a supported requested protocol revision; otherwise answer with the
/// latest revision this server implements (MCP lifecycle spec).
fn negotiate_protocol_version(requested: Option<&str>) -> &'static str {
    match requested {
        Some(version) => SUPPORTED_PROTOCOL_VERSIONS
            .iter()
            .find(|supported| **supported == version)
            .copied()
            .unwrap_or(MCP_LATEST_PROTOCOL_VERSION),
        None => MCP_PROTOCOL_VERSION,
    }
}

pub(crate) fn json_rpc_error(id: Option<Value>, code: i64, message: String) -> Value {
    let error = JsonRpcError { code, message };
    json!({
        "jsonrpc": "2.0",
        "id": id.unwrap_or(Value::Null),
        "error": error,
    })
}

fn tool_definitions() -> Vec<Value> {
    let execute_desc = "\
Run Python against the Temper operating layer. Code receives `temper` with async methods.\n\
\n\
Requires a running Temper server (--port for local, --url for remote).\n\
\n\
DISCOVERY:\n\
\x20 await temper.specs(tenant) -> loaded specs with states, actions, verification status\n\
\x20 await temper.spec_detail(tenant, entity_type) -> full spec: actions, guards, invariants, state vars\n\
\n\
ENTITY OPERATIONS:\n\
\x20 await temper.list(tenant, entity_type, filter?) -> list entities\n\
\x20 await temper.get(tenant, entity_type, entity_id) -> get entity\n\
\x20 await temper.create(tenant, entity_type, fields) -> create entity\n\
\x20 await temper.action(tenant, entity_type, entity_id, action_name, body) -> invoke action\n\
\x20 await temper.patch(tenant, entity_type, entity_id, fields) -> update fields\n\
\n\
DEVELOPER:\n\
\x20 await temper.submit_specs(tenant, {\"entity.ioa.toml\": \"...\", \"model.csdl.xml\": \"...\"}) -> submit specs\n\
\x20 await temper.get_policies(tenant) -> Cedar policies\n\
\x20 await temper.get_policy_entries(tenant) -> stored policy IDs, hashes and Cedar text (authorized read)\n\
\x20 await temper.upload_wasm(tenant, module_name, wasm_path) -> upload WASM module\n\
\x20 await temper.compile_wasm(tenant, module_name, rust_source) -> compile + upload WASM\n\
\n\
APP CATALOG:\n\
\x20 await temper.list_apps(tenant) -> available pre-built apps (name, description, entity_types)\n\
\x20 await temper.get_app(tenant, app_name) -> full app guide markdown (when to use, actions, examples)\n\
\x20 await temper.install_app(tenant, app_ref) -> install a pinned owner/app@hash through the governed Genesis installer\n\
\n\
GOVERNANCE:\n\
\x20 await temper.get_decisions(tenant, status?) -> list decisions\n\
\x20 await temper.get_decision_status(tenant, decision_id) -> check single decision\n\
\x20 await temper.poll_decision(tenant, decision_id) -> wait for human decision (120s timeout)\n\
\n\
OBSERVABILITY:\n\
\x20 await temper.get_trajectories(tenant, entity_type?, failed_only?, limit?) -> trajectory spans\n\
\x20 await temper.get_insights(tenant) -> evolution insights\n\
\x20 await temper.get_evolution_records(tenant, record_type?) -> O-P-A-D-I records\n\
\x20 await temper.check_sentinel(tenant) -> trigger evolution engine\n\
\n\
TRIGGERS (ADR-0046): actions declare [[action.triggers]] inline with kind = \"entity\" | \"wasm\" | \"webhook\".\n\
For HTTP, use kind = \"wasm\" with module = \"http_fetch\", url, and method config keys.\n\
The webhook kind is parse-only today; use wasm + http_fetch instead.\n\
\n\
COMPILE_WASM: Use compile_wasm(tenant, module_name, rust_source) to compile Rust into WASM.\n\
Source should use `temper_wasm_sdk::prelude::*` and the `temper_module!` macro.\n\
\n\
CEDAR GOVERNANCE: actions may be denied by Cedar policy. Denied actions create\n\
decisions for human approval in the Observe UI or via `temper decide` CLI.\n\
Use poll_decision(tenant, decision_id) to wait for the human decision.\n\
OTS FLUSH: `await temper.flush_trajectory()` uploads a mid-session OTS snapshot\n\
without ending the session.\n\
You cannot approve or set policies — only humans can do that.";

    vec![
        json!({
            "name": "execute",
            "description": execute_desc,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "code": {
                        "type": "string",
                        "description": "Python snippet. Use async calls like `await temper.list(...)` and `return ...`."
                    }
                },
                "required": ["code"],
                "additionalProperties": false
            }
        }),
        json!({
            "name":"request_policy_replacement",
            "description":"Propose replacement of one enabled durable Cedar policy entry. Requires its current SHA-256, exact new text and native human consent. The configured human approver remains separate from execute; ordinary decision approval cannot authorize this operation. Never retries a stale proposal.",
            "inputSchema":{"type":"object","properties":{
                "policy_id":{"type":"string","description":"Durable entry ID from policies/list, not an evaluator policy number"},
                "expected_hash":{"type":"string","description":"Current lowercase SHA-256"},
                "cedar_text":{"type":"string","description":"Exact complete proposed Cedar text"}
            },"required":["policy_id","expected_hash","cedar_text"],"additionalProperties":false}
        }),
        json!({
            "name":"request_policy_amendment",
            "description":"Propose exact text edits to one enabled Cedar entry, including large bundles. Human reviews every complete old/new edit and whole-document hashes. Other bytes remain unchanged. Requires separate native human approval; stale, missing, ambiguous or overlapping edits are rejected.",
            "inputSchema":{"type":"object","properties":{
                "policy_id":{"type":"string"},"expected_hash":{"type":"string"},
                "edits":{"type":"array","minItems":1,"maxItems":16,"items":{"type":"object","properties":{"old":{"type":"string","minLength":1},"new":{"type":"string"}},"required":["old","new"],"additionalProperties":false}}
            },"required":["policy_id","expected_hash","edits"],"additionalProperties":false}
        }),
        json!({
            "name": "setup_connection",
            "description": "Ask the human to set up distinct requester and approver identities for the configured Temper service. Accepts no arguments. Only a native human setup response authorizes administration. Does not approve pending decisions. Requires TEMPER_MCP_IDENTITY_FILE to name a file in a private directory.",
            "inputSchema": {"type":"object", "properties":{}, "additionalProperties":false}
        }),
    ]
}

fn is_flush_trajectory_request(code: &str) -> bool {
    let compact = code.split_whitespace().collect::<String>();
    compact.contains("temper.flush_trajectory()")
}
