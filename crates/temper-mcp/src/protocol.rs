//! JSON-RPC protocol handlers and tool schema.

use anyhow::anyhow;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::spec_loader::generate_loaded_summary;
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

pub(super) async fn dispatch_json_line(ctx: &RuntimeContext, line: &str) -> Option<Value> {
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

pub(super) async fn dispatch_json_value(ctx: &RuntimeContext, raw: Value) -> Option<Value> {
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
        "tools/list" => Ok(json!({ "tools": tool_definitions(ctx) })),
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
                "search" => ctx.run_search(code),
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

fn tool_definitions(ctx: &RuntimeContext) -> Vec<Value> {
    let summary = generate_loaded_summary(&ctx.spec);

    let search_desc = format!(
        "Explore loaded Temper specs (read-only, no server required). \
In self-contained mode with no --apps, results will be empty until you submit specs \
via the execute tool. Use after submit_specs to verify spec structure.\n\n\
Code receives `spec` with async methods (use `await`):\n\n\
\x20 await spec.tenants() -> list of tenant names\n\
\x20 await spec.entities(tenant) -> list of entity type names\n\
\x20 await spec.describe(tenant, entity_type) -> states, actions, guards, vars\n\
\x20 await spec.actions(tenant, entity_type) -> all actions with params, from/to, guards\n\
\x20 await spec.actions_from(tenant, entity_type, state) -> actions available from a state\n\
\x20 await spec.raw(tenant, entity_type) -> full spec JSON{summary}"
    );

    let execute_desc = format!(
        "Run Python against the Temper operating layer. Code receives `temper` with async methods.\n\n\
WORKFLOW — build and operate governed apps:\n\
\x20 1. await temper.start_server()  — launch the runtime\n\
\x20 2. await temper.submit_specs(tenant, {{\"entity.ioa.toml\": \"...\", \"model.csdl.xml\": \"...\"}})\n\
\x20 3. await temper.create/action/list/get — operate through verified specs\n\n\
INTEGRATION PATTERN — specs declare [[integration]] sections for external APIs.\n\
Effects trigger integrations; callbacks re-enter the state machine. You do not\n\
call external APIs directly — you declare them in the spec and the runtime\n\
handles execution within governance boundaries.\n\n\
METHOD REFERENCE:\n\
\x20 Lifecycle:    start_server\n\
\x20 Entity ops:   list, get, create, action, patch\n\
\x20 Developer:    show_spec, submit_specs, get_policies, upload_wasm, compile_wasm\n\
\x20 Governance:   get_decisions, poll_decision\n\n\
BUILT-IN MODULE: The server ships with a pre-registered `http_fetch` WASM module.\n\
Declare `module = \"http_fetch\"` in [[integration]] sections with `url` and `method`\n\
config keys to call any HTTP API without uploading a custom module.\n\n\
COMPILE_WASM: Use `await temper.compile_wasm(tenant, module_name, rust_source)` to compile\n\
Rust source into a WASM module. Source should use `temper_wasm_sdk::prelude::*` and the\n\
`temper_module!` macro. Returns {{\"status\":\"compiled\",\"module\":name,\"hash\":...,\"size\":...}}\n\
on success, or compiler errors on failure for self-correction.\n\n\
Call `await temper.start_server()` before entity/developer/governance methods \
(unless MCP was started with --port).\n\
CEDAR GOVERNANCE: actions may be denied by Cedar policy. Denied actions create\n\
decisions for human approval in the Observe UI or via `temper decide` CLI.\n\
Use `await temper.poll_decision(tenant, decision_id)` to wait for the human decision.\n\
You cannot approve or set policies — only humans can do that.{summary}"
    );

    vec![
        json!({
            "name": "search",
            "description": search_desc,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "code": {
                        "type": "string",
                        "description": "Python snippet. Use `return ...` as the final statement."
                    }
                },
                "required": ["code"],
                "additionalProperties": false
            }
        }),
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
    ]
}
