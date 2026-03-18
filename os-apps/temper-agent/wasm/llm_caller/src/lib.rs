//! LLM Caller — WASM module for calling the Anthropic Messages API.
//!
//! Reads conversation from TemperFS File entity (via $value endpoint) when
//! `conversation_file_id` is set, otherwise falls back to inline entity state.
//! Calls the LLM, appends the response, writes back to TemperFS, and returns
//! a dynamic callback action based on the LLM's response:
//! - `ProcessToolCalls` if the response contains tool_use blocks
//! - `RecordResult` if the response is an end_turn
//! - `Fail` if the turn budget is exceeded
//!
//! Build: `cargo build --target wasm32-unknown-unknown --release`

use temper_wasm_sdk::prelude::*;

/// Entry point — NOT using `temper_module!` because we need dynamic callback actions.
#[unsafe(no_mangle)]
pub extern "C" fn run(_ctx_ptr: i32, _ctx_len: i32) -> i32 {
    let result = (|| -> Result<(), String> {
        let ctx = Context::from_host()?;
        ctx.log("info", "llm_caller: starting");

        // Read entity state
        let fields = ctx.entity_state.get("fields").cloned().unwrap_or(json!({}));

        // Check turn budget
        let turn_count = fields
            .get("turn_count")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let max_turns = fields
            .get("max_turns")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(20);

        if turn_count >= max_turns {
            set_success_result(
                "Fail",
                &json!({ "error_message": format!("turn budget exhausted ({turn_count}/{max_turns})") }),
            );
            return Ok(());
        }

        // Read configuration
        let model = fields
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("claude-sonnet-4-20250514");
        let provider = fields
            .get("provider")
            .and_then(|v| v.as_str())
            .unwrap_or("anthropic");
        let tools_enabled = fields
            .get("tools_enabled")
            .and_then(|v| v.as_str())
            .unwrap_or("read,write,edit,bash");
        // `system_prompt` is the Anthropic API system parameter (agent persona/behavior).
        // `user_message` is the actual user task from the Provision action.
        let system_prompt = fields
            .get("system_prompt")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let user_message = fields
            .get("user_message")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let sandbox_url = fields
            .get("sandbox_url")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let workdir = fields
            .get("workdir")
            .and_then(|v| v.as_str())
            .unwrap_or("/workspace");

        // Get API key from integration config (resolved from {secret:anthropic_api_key})
        let api_key = ctx.config.get("api_key").cloned().unwrap_or_default();

        if api_key.is_empty() {
            return Err("missing api_key in integration config".to_string());
        }

        // TemperFS conversation storage
        let conversation_file_id = fields
            .get("conversation_file_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let temper_api_url = ctx
            .config
            .get("temper_api_url")
            .cloned()
            .unwrap_or_else(|| "http://127.0.0.1:3000".to_string());
        let tenant = &ctx.tenant;

        // Read conversation — from TemperFS if file_id set, else inline state.
        // First turn uses `user_message` (the actual user task from Provision).
        // `system_prompt` is always sent as the Anthropic system parameter, never as a message.
        if user_message.is_empty() {
            return Err("user_message is empty — nothing to send to the LLM".to_string());
        }
        let first_turn_content = user_message;
        let mut messages: Vec<Value> = if !conversation_file_id.is_empty() {
            read_conversation_from_temperfs(
                &ctx,
                &temper_api_url,
                tenant,
                conversation_file_id,
                first_turn_content,
            )?
        } else {
            let conversation_json = fields
                .get("conversation")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if conversation_json.is_empty() {
                vec![json!({ "role": "user", "content": first_turn_content })]
            } else {
                serde_json::from_str(conversation_json).unwrap_or_else(|_| {
                    vec![json!({ "role": "user", "content": first_turn_content })]
                })
            }
        };

        // Build tool definitions based on tools_enabled
        let tools = build_tool_definitions(tools_enabled, sandbox_url, workdir);

        // Call LLM API
        let response = match provider {
            "anthropic" => call_anthropic(&ctx, &api_key, model, system_prompt, &messages, &tools)?,
            other => return Err(format!("unsupported LLM provider: {other}")),
        };

        ctx.log(
            "info",
            &format!(
                "llm_caller: got response, stop_reason={}",
                response.stop_reason
            ),
        );

        // Append assistant response to conversation
        messages.push(json!({
            "role": "assistant",
            "content": response.content,
        }));

        // Write updated conversation to TemperFS (if file_id set) or pass inline
        let updated_conversation = serde_json::to_string(&messages).unwrap_or_default();

        if !conversation_file_id.is_empty() {
            write_conversation_to_temperfs(
                &ctx,
                &temper_api_url,
                tenant,
                conversation_file_id,
                &updated_conversation,
            )?;
        }

        // For TemperFS mode, don't pass conversation inline (it's in the File)
        let conv_param = if conversation_file_id.is_empty() {
            Some(updated_conversation.clone())
        } else {
            None
        };

        // Route based on stop_reason
        match response.stop_reason.as_str() {
            "tool_use" => {
                // Extract tool_use blocks
                let tool_calls: Vec<Value> = response
                    .content
                    .as_array()
                    .unwrap_or(&vec![])
                    .iter()
                    .filter(|block| block.get("type").and_then(|v| v.as_str()) == Some("tool_use"))
                    .cloned()
                    .collect();

                let tool_calls_json = serde_json::to_string(&tool_calls).unwrap_or_default();
                let mut params = json!({
                    "pending_tool_calls": tool_calls_json,
                    "input_tokens": response.input_tokens,
                    "output_tokens": response.output_tokens,
                });
                if let Some(ref conv) = conv_param {
                    params["conversation"] = json!(conv);
                }
                set_success_result("ProcessToolCalls", &params);
            }
            "end_turn" | "stop" => {
                // Extract text result
                let result_text = response
                    .content
                    .as_array()
                    .unwrap_or(&vec![])
                    .iter()
                    .filter_map(|block| {
                        if block.get("type").and_then(|v| v.as_str()) == Some("text") {
                            block.get("text").and_then(|v| v.as_str()).map(String::from)
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n");

                let mut params = json!({
                    "result": result_text,
                    "input_tokens": response.input_tokens,
                    "output_tokens": response.output_tokens,
                });
                if let Some(ref conv) = conv_param {
                    params["conversation"] = json!(conv);
                }
                set_success_result("RecordResult", &params);
            }
            other => {
                set_success_result(
                    "Fail",
                    &json!({ "error_message": format!("unexpected stop_reason: {other}") }),
                );
            }
        }

        Ok(())
    })();

    if let Err(e) = result {
        set_error_result(&e);
    }
    0
}

/// Parsed LLM response.
struct LlmResponse {
    content: Value,
    stop_reason: String,
    input_tokens: i64,
    output_tokens: i64,
}

/// Call Anthropic Messages API.
fn call_anthropic(
    ctx: &Context,
    api_key: &str,
    model: &str,
    system_prompt: &str,
    messages: &[Value],
    tools: &[Value],
) -> Result<LlmResponse, String> {
    // Detect OAuth token (sk-ant-oat-*) vs standard API key
    let is_oauth = api_key.contains("sk-ant-oat");

    // OAuth tokens enforce a fixed system prompt when tools are present.
    // Custom system instructions are prepended to the first user message instead.
    let (effective_system, effective_messages) = if is_oauth {
        let oauth_system = "You are Claude Code, Anthropic's official CLI for Claude.".to_string();
        let mut msgs = messages.to_vec();
        if !system_prompt.is_empty() {
            if let Some(first) = msgs.first_mut() {
                if let Some(content) = first.get("content").and_then(|v| v.as_str()) {
                    let combined = format!("[System instructions: {system_prompt}]\n\n{content}");
                    first["content"] = json!(combined);
                }
            }
        }
        (oauth_system, msgs)
    } else {
        (system_prompt.to_string(), messages.to_vec())
    };

    let mut body = json!({
        "model": model,
        "max_tokens": 4096,
        "messages": effective_messages,
    });

    if !effective_system.is_empty() {
        body["system"] = json!(effective_system);
    }

    if !tools.is_empty() {
        body["tools"] = json!(tools);
    }

    let body_str =
        serde_json::to_string(&body).map_err(|e| format!("JSON serialize error: {e}"))?;

    ctx.log(
        "info",
        &format!(
            "llm_caller: calling Anthropic API, model={model}, oauth={is_oauth}, messages={}",
            messages.len()
        ),
    );

    // Build auth headers — OAuth tokens use Bearer + beta header
    let headers = if is_oauth {
        vec![
            ("authorization".to_string(), format!("Bearer {api_key}")),
            ("anthropic-version".to_string(), "2023-06-01".to_string()),
            (
                "anthropic-beta".to_string(),
                "oauth-2025-04-20,computer-use-2025-01-24".to_string(),
            ),
            ("content-type".to_string(), "application/json".to_string()),
            ("user-agent".to_string(), "claude-cli/2.1.75".to_string()),
            ("x-app".to_string(), "cli".to_string()),
        ]
    } else {
        vec![
            ("x-api-key".to_string(), api_key.to_string()),
            ("anthropic-version".to_string(), "2023-06-01".to_string()),
            ("content-type".to_string(), "application/json".to_string()),
        ]
    };

    // Retry on transient API errors (500, 529, and 400 with vague "Error" message)
    let mut last_err = String::new();
    let mut resp = None;
    for attempt in 0..5 {
        if attempt > 0 {
            ctx.log(
                "warn",
                &format!(
                    "llm_caller: retrying (attempt {}/5), last error: {last_err}",
                    attempt + 1
                ),
            );
        }
        match ctx.http_call(
            "POST",
            "https://api.anthropic.com/v1/messages",
            &headers,
            &body_str,
        ) {
            Ok(r) if r.status == 200 => {
                resp = Some(r);
                break;
            }
            Ok(r) if r.status == 500 || r.status == 529 => {
                last_err = format!("HTTP {}: {}", r.status, &r.body[..r.body.len().min(200)]);
                continue;
            }
            Ok(r) if r.status == 400 && r.body.contains("\"message\":\"Error\"") => {
                // Transient 400 with vague error message — retry
                last_err = format!("HTTP 400 (transient): {}", &r.body[..r.body.len().min(200)]);
                continue;
            }
            Ok(r) => {
                return Err(format!(
                    "Anthropic API returned {}: {}",
                    r.status,
                    &r.body[..r.body.len().min(500)]
                ));
            }
            Err(e) => {
                last_err = e;
                continue;
            }
        }
    }
    let resp = resp.ok_or_else(|| format!("Anthropic API failed after 5 attempts: {last_err}"))?;

    let parsed: Value = serde_json::from_str(&resp.body)
        .map_err(|e| format!("failed to parse LLM response: {e}"))?;

    let stop_reason = parsed
        .get("stop_reason")
        .and_then(|v| v.as_str())
        .unwrap_or("end_turn")
        .to_string();

    let content = parsed.get("content").cloned().unwrap_or(json!([]));

    // Extract token usage from Anthropic response
    let usage = parsed.get("usage").cloned().unwrap_or(json!({}));
    let input_tokens = usage
        .get("input_tokens")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let output_tokens = usage
        .get("output_tokens")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);

    ctx.log(
        "info",
        &format!("llm_caller: usage: input={input_tokens}, output={output_tokens}"),
    );

    Ok(LlmResponse {
        content,
        stop_reason,
        input_tokens,
        output_tokens,
    })
}

/// Build tool definitions for the LLM based on enabled tools.
fn build_tool_definitions(tools_enabled: &str, sandbox_url: &str, workdir: &str) -> Vec<Value> {
    let enabled: Vec<&str> = tools_enabled.split(',').map(str::trim).collect();
    let mut tools = Vec::new();

    if enabled.contains(&"read") {
        tools.push(json!({
            "name": "read",
            "description": format!("Read a file from the sandbox at {sandbox_url}. Working directory: {workdir}"),
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path to read" }
                },
                "required": ["path"]
            }
        }));
    }

    if enabled.contains(&"write") {
        tools.push(json!({
            "name": "write",
            "description": format!("Write a file to the sandbox at {sandbox_url}. Working directory: {workdir}"),
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path to write" },
                    "content": { "type": "string", "description": "File content" }
                },
                "required": ["path", "content"]
            }
        }));
    }

    if enabled.contains(&"edit") {
        tools.push(json!({
            "name": "edit",
            "description": format!("Edit a file in the sandbox at {sandbox_url} using search-and-replace. Working directory: {workdir}"),
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path to edit" },
                    "old_string": { "type": "string", "description": "Text to find" },
                    "new_string": { "type": "string", "description": "Text to replace with" }
                },
                "required": ["path", "old_string", "new_string"]
            }
        }));
    }

    if enabled.contains(&"bash") {
        tools.push(json!({
            "name": "bash",
            "description": format!("Run a bash command in the sandbox at {sandbox_url}. Working directory: {workdir}"),
            "input_schema": {
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Bash command to execute" }
                },
                "required": ["command"]
            }
        }));
    }

    tools
}

/// Read conversation messages from TemperFS File entity via $value endpoint.
fn read_conversation_from_temperfs(
    ctx: &Context,
    temper_api_url: &str,
    tenant: &str,
    file_id: &str,
    user_message: &str,
) -> Result<Vec<Value>, String> {
    let url = format!("{temper_api_url}/tdata/Files('{file_id}')/$value");
    let headers = vec![
        ("x-tenant-id".to_string(), tenant.to_string()),
        ("x-temper-principal-kind".to_string(), "system".to_string()),
        ("accept".to_string(), "application/json".to_string()),
    ];

    match ctx.http_call("GET", &url, &headers, "") {
        Ok(resp) if resp.status == 200 => {
            let parsed: Value = serde_json::from_str(&resp.body).unwrap_or(json!({"messages": []}));
            let messages = parsed
                .get("messages")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            if messages.is_empty() {
                // First turn — initialize with user message
                Ok(vec![json!({ "role": "user", "content": user_message })])
            } else {
                Ok(messages)
            }
        }
        Ok(resp) if resp.status == 404 => {
            // File has no content yet — first turn
            ctx.log(
                "info",
                "llm_caller: TemperFS file has no content, initializing",
            );
            Ok(vec![json!({ "role": "user", "content": user_message })])
        }
        Ok(resp) => {
            ctx.log(
                "warn",
                &format!(
                    "llm_caller: TemperFS read failed (HTTP {}), falling back to inline",
                    resp.status
                ),
            );
            Ok(vec![json!({ "role": "user", "content": user_message })])
        }
        Err(e) => {
            ctx.log(
                "warn",
                &format!("llm_caller: TemperFS read error: {e}, falling back to inline"),
            );
            Ok(vec![json!({ "role": "user", "content": user_message })])
        }
    }
}

/// Write conversation messages to TemperFS File entity via $value endpoint.
fn write_conversation_to_temperfs(
    ctx: &Context,
    temper_api_url: &str,
    tenant: &str,
    file_id: &str,
    conversation_json: &str,
) -> Result<(), String> {
    let url = format!("{temper_api_url}/tdata/Files('{file_id}')/$value");
    let headers = vec![
        ("content-type".to_string(), "application/json".to_string()),
        ("x-tenant-id".to_string(), tenant.to_string()),
        ("x-temper-principal-kind".to_string(), "system".to_string()),
    ];

    // Wrap messages array in the TemperFS conversation format
    let body = format!("{{\"messages\":{conversation_json}}}");

    let resp = ctx.http_call("PUT", &url, &headers, &body)?;
    if resp.status >= 200 && resp.status < 300 {
        ctx.log(
            "info",
            &format!(
                "llm_caller: wrote conversation to TemperFS ({} bytes)",
                body.len()
            ),
        );
        Ok(())
    } else {
        Err(format!(
            "TemperFS $value write failed (HTTP {}): {}",
            resp.status,
            &resp.body[..resp.body.len().min(200)]
        ))
    }
}
