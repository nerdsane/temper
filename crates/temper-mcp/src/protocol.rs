//! JSON-RPC protocol handlers and tool schema.

use anyhow::anyhow;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{MCP_PROTOCOL_VERSION, MCP_SERVER_NAME, RuntimeContext};

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

pub(super) async fn dispatch_json_line(ctx: &mut RuntimeContext, line: &str) -> Option<Value> {
    let raw: Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(error) => {
            return Some(json_rpc_error(
                None,
                -32700,
                format!("parse error: {error}"),
            ));
        }
    };

    dispatch_json_value(ctx, raw).await
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
        "initialize" => Ok(json!({
            "protocolVersion": MCP_PROTOCOL_VERSION,
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
        decision surfaces to the human developer for approval."
        })),
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

            let tool_result = match params.name.as_str() {
                "execute" => ctx.run_execute(code).await,
                other => Err(anyhow!(format!("unknown tool '{other}'"))),
            };

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

fn json_rpc_error(id: Option<Value>, code: i64, message: String) -> Value {
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
\x20 await temper.upload_wasm(tenant, module_name, wasm_path) -> upload WASM module\n\
\x20 await temper.compile_wasm(tenant, module_name, rust_source) -> compile + upload WASM\n\
\n\
OS APP CATALOG:\n\
\x20 await temper.list_apps() -> available pre-built apps (name, description, entity_types)\n\
\x20 await temper.install_app(app_name) -> install an OS app into the current tenant\n\
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
INTEGRATION: specs declare [[integration]] sections for external APIs.\n\
Use module = \"http_fetch\" with url and method config keys for HTTP integrations.\n\
\n\
COMPILE_WASM: Use compile_wasm(tenant, module_name, rust_source) to compile Rust into WASM.\n\
Source should use `temper_wasm_sdk::prelude::*` and the `temper_module!` macro.\n\
\n\
CEDAR GOVERNANCE: actions may be denied by Cedar policy. Denied actions create\n\
decisions for human approval in the Observe UI or via `temper decide` CLI.\n\
Use poll_decision(tenant, decision_id) to wait for the human decision.\n\
You cannot approve or set policies — only humans can do that.";

    vec![json!({
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
    })]
}
