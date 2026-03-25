//! Tool Runner — WASM module for executing tool calls in a sandbox.
//!
//! Reads pending_tool_calls from trigger params, executes each tool via
//! HTTP calls to the sandbox API, and returns tool results as callback params.
//!
//! Build: `cargo build --target wasm32-unknown-unknown --release`

use std::collections::BTreeMap;
use temper_wasm_sdk::prelude::*;

/// Entry point.
#[unsafe(no_mangle)]
pub extern "C" fn run(_ctx_ptr: i32, _ctx_len: i32) -> i32 {
    let result = (|| -> Result<(), String> {
        let ctx = Context::from_host()?;
        ctx.log("info", "tool_runner: starting");

        let fields = ctx.entity_state.get("fields").cloned().unwrap_or(json!({}));

        let sandbox_url = fields
            .get("sandbox_url")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let workdir = fields
            .get("workdir")
            .and_then(|v| v.as_str())
            .unwrap_or("/workspace");

        // Temper API URL: read from integration config, default to localhost
        let temper_api_url = ctx
            .config
            .get("temper_api_url")
            .cloned()
            .unwrap_or_else(|| "http://127.0.0.1:3000".to_string());
        let tenant = &ctx.tenant;
        let hook_policy = fields
            .get("hook_policy")
            .and_then(|v| v.as_str())
            .unwrap_or("none");
        let soul_id = fields
            .get("soul_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let _ = send_heartbeat(&ctx, &temper_api_url, tenant);

        // Read pending tool calls from trigger params
        let tool_calls_json = ctx
            .trigger_params
            .get("pending_tool_calls")
            .and_then(|v| v.as_str())
            .unwrap_or("[]");

        let tool_calls: Vec<Value> = serde_json::from_str(tool_calls_json)
            .map_err(|e| format!("failed to parse pending_tool_calls: {e}"))?;

        ctx.log(
            "info",
            &format!("tool_runner: executing {} tool calls", tool_calls.len()),
        );

        // Execute each tool call and collect results
        let mut tool_results: Vec<Value> = Vec::new();

        for call in &tool_calls {
            let tool_id = call.get("id").and_then(|v| v.as_str()).unwrap_or("unknown");
            let tool_name = call
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let input = call.get("input").cloned().unwrap_or(json!({}));

            ctx.log(
                "info",
                &format!("tool_runner: executing tool '{tool_name}' id={tool_id}"),
            );
            emit_progress_ignore(
                &ctx,
                json!({
                    "kind": "tool_execution_start",
                    "message": format!("executing tool {tool_name}"),
                    "tool_call_id": tool_id,
                    "tool_name": tool_name,
                }),
            );

            let result = if let Err(error) = validate_tool_input(tool_name, &input) {
                Err(error)
            } else if let Some(error) =
                evaluate_before_hooks(&ctx, &temper_api_url, tenant, soul_id, hook_policy, tool_name)?
            {
                Err(error)
            } else if is_entity_tool(tool_name) {
                execute_entity_tool(&ctx, &temper_api_url, tenant, &fields, tool_name, &input)
            } else if sandbox_url.is_empty() {
                Err(format!("sandbox_url is empty — cannot execute sandbox tool '{tool_name}'"))
            } else {
                execute_tool(&ctx, sandbox_url, workdir, tool_name, &input)
            };

            let (content, is_error) = match result {
                Ok(output) => (
                    apply_after_hooks(
                        &ctx,
                        &temper_api_url,
                        tenant,
                        soul_id,
                        hook_policy,
                        tool_name,
                        output,
                    )?,
                    false,
                ),
                Err(e) => (format!("Error: {e}"), true),
            };
            let _ = send_heartbeat(&ctx, &temper_api_url, tenant);
            emit_progress_ignore(
                &ctx,
                json!({
                    "kind": "tool_execution_complete",
                    "message": format!("completed tool {tool_name}"),
                    "tool_call_id": tool_id,
                    "tool_name": tool_name,
                    "is_error": is_error,
                }),
            );

            tool_results.push(json!({
                "type": "tool_result",
                "tool_use_id": tool_id,
                "content": content,
                "is_error": is_error,
            }));
        }

        // Session tree and conversation storage
        let conversation_file_id = fields
            .get("conversation_file_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let session_file_id = fields
            .get("session_file_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let session_leaf_id = fields
            .get("session_leaf_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let results_json = serde_json::to_string(&tool_results).unwrap_or_default();
        let mut params = json!({
            "pending_tool_calls": results_json,
        });

        if !session_file_id.is_empty() && !session_leaf_id.is_empty() {
            // Session tree mode: append tool results
            let session_jsonl = read_session_from_temperfs(&ctx, &temper_api_url, tenant, session_file_id)?;
            let mut tree = session_tree_lib::SessionTree::from_jsonl(&session_jsonl);
            let tool_results_value = json!(tool_results.clone());
            let tokens_est = results_json.len() / 4;
            let (new_leaf, _) = tree.append_tool_results(session_leaf_id, &tool_results_value, tokens_est);
            let updated_jsonl = tree.to_jsonl();
            write_session_to_temperfs(&ctx, &temper_api_url, tenant, session_file_id, &updated_jsonl)?;

            params["session_leaf_id"] = json!(new_leaf);
        } else if !conversation_file_id.is_empty() {
            // Legacy flat JSON mode
            let mut messages: Vec<Value> =
                read_conversation_from_temperfs(&ctx, &temper_api_url, tenant, conversation_file_id)?;

            // Append tool results as a user message (Anthropic API format)
            messages.push(json!({
                "role": "user",
                "content": tool_results,
            }));

            let updated_conversation = serde_json::to_string(&messages).unwrap_or_default();
            let body = format!("{{\"messages\":{updated_conversation}}}");
            let url = format!("{temper_api_url}/tdata/Files('{conversation_file_id}')/$value");
            let headers = vec![
                ("content-type".to_string(), "application/json".to_string()),
                ("x-tenant-id".to_string(), tenant.to_string()),
                ("x-temper-principal-kind".to_string(), "admin".to_string()),
            ];
            match ctx.http_call("PUT", &url, &headers, &body) {
                Ok(resp) if resp.status >= 200 && resp.status < 300 => {
                    ctx.log(
                        "info",
                        &format!(
                            "tool_runner: wrote conversation to TemperFS ({} bytes)",
                            body.len()
                        ),
                    );
                }
                Ok(resp) => {
                    return Err(format!(
                        "TemperFS conversation write failed (HTTP {}): {}",
                        resp.status,
                        &resp.body[..resp.body.len().min(200)]
                    ));
                }
                Err(e) => {
                    return Err(format!("TemperFS conversation write failed: {e}"));
                }
            }
            params["conversation"] = json!(updated_conversation);
        } else {
            // Inline conversation mode (no TemperFS)
            let mut messages: Vec<Value> = {
                let conversation_json = fields
                    .get("conversation")
                    .and_then(|v| v.as_str())
                    .unwrap_or("[]");
                serde_json::from_str(conversation_json).unwrap_or_default()
            };

            messages.push(json!({
                "role": "user",
                "content": tool_results,
            }));

            let updated_conversation = serde_json::to_string(&messages).unwrap_or_default();
            params["conversation"] = json!(updated_conversation);
        }

        // Fsync sandbox files to TemperFS (best-effort)
        let file_manifest_id = fields
            .get("file_manifest_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let workspace_id = fields
            .get("workspace_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let max_sync_file_bytes: u64 = ctx
            .config
            .get("max_sync_file_bytes")
            .and_then(|s| s.parse().ok())
            .unwrap_or(61440);
        let sync_exclude = ctx.config.get("sync_exclude").cloned().unwrap_or_default();

        if !file_manifest_id.is_empty() && !workspace_id.is_empty() && !sandbox_url.is_empty() {
            let e2b = is_e2b_sandbox(sandbox_url);
            match sync_files_to_temperfs(
                &ctx,
                sandbox_url,
                &temper_api_url,
                tenant,
                workspace_id,
                file_manifest_id,
                workdir,
                e2b,
                max_sync_file_bytes,
                &sync_exclude,
            ) {
                Ok(count) => ctx.log(
                    "info",
                    &format!("tool_runner: fsync complete ({count} files synced)"),
                ),
                Err(e) => ctx.log(
                    "warn",
                    &format!("tool_runner: fsync failed (non-fatal): {e}"),
                ),
            }
        }

        set_success_result("HandleToolResults", &params);

        Ok(())
    })();

    if let Err(e) = result {
        set_error_result(&e);
    }
    0
}

/// Detect whether the sandbox is E2B (envd daemon) based on the URL.
fn is_e2b_sandbox(sandbox_url: &str) -> bool {
    sandbox_url.contains("e2b.app") || sandbox_url.contains("e2b.dev")
}

/// Execute a single tool call against the sandbox API.
/// Supports both local sandbox API (/v1/fs/file, /v1/processes/run)
/// and E2B envd API (/files, Connect protocol for processes).
fn execute_tool(
    ctx: &Context,
    sandbox_url: &str,
    workdir: &str,
    tool_name: &str,
    input: &Value,
) -> Result<String, String> {
    let e2b = is_e2b_sandbox(sandbox_url);
    match tool_name {
        "read" => {
            let path = input
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or("read: missing 'path' parameter")?;

            let full_path = resolve_path(workdir, path);
            if e2b {
                read_file_e2b(ctx, sandbox_url, &full_path)
            } else {
                read_file_local(ctx, sandbox_url, &full_path)
            }
        }
        "write" => {
            let path = input
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or("write: missing 'path' parameter")?;
            let content = input
                .get("content")
                .and_then(|v| v.as_str())
                .ok_or("write: missing 'content' parameter")?;

            let full_path = resolve_path(workdir, path);
            if e2b {
                write_file_e2b(ctx, sandbox_url, &full_path, content)
            } else {
                write_file_local(ctx, sandbox_url, &full_path, content)
            }
        }
        "edit" => {
            let path = input
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or("edit: missing 'path' parameter")?;
            let old_string = input
                .get("old_string")
                .and_then(|v| v.as_str())
                .ok_or("edit: missing 'old_string' parameter")?;
            let new_string = input
                .get("new_string")
                .and_then(|v| v.as_str())
                .ok_or("edit: missing 'new_string' parameter")?;

            let full_path = resolve_path(workdir, path);
            // Read current file
            let current = if e2b {
                read_file_e2b(ctx, sandbox_url, &full_path)?
            } else {
                read_file_local(ctx, sandbox_url, &full_path)?
            };

            if !current.contains(old_string) {
                return Err(format!("edit: old_string not found in {full_path}"));
            }
            let updated = current.replacen(old_string, new_string, 1);

            // Write updated file
            if e2b {
                write_file_e2b(ctx, sandbox_url, &full_path, &updated)?;
            } else {
                write_file_local(ctx, sandbox_url, &full_path, &updated)?;
            }
            Ok(format!("File edited: {full_path}"))
        }
        "bash" => {
            let command = input
                .get("command")
                .and_then(|v| v.as_str())
                .ok_or("bash: missing 'command' parameter")?;

            if e2b {
                run_bash_e2b(ctx, sandbox_url, command, workdir)
            } else {
                run_bash_local(ctx, sandbox_url, command, workdir)
            }
        }
        "logfire_query" => query_logfire(ctx, input),
        unknown => Err(format!("unknown tool: {unknown}")),
    }
}

fn query_logfire(ctx: &Context, input: &Value) -> Result<String, String> {
    let sql = input
        .get("sql")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| build_logfire_sql(input).ok())
        .ok_or("logfire_query: provide either 'sql' or a supported 'query_kind'")?;

    let limit = input
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(50)
        .clamp(1, 200);
    let row_oriented = input
        .get("row_oriented")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let min_timestamp = input.get("min_timestamp").and_then(Value::as_str);
    let max_timestamp = input.get("max_timestamp").and_then(Value::as_str);
    let query_kind = input.get("query_kind").and_then(Value::as_str).unwrap_or("sql");

    let base_url = normalize_logfire_base_url(
        ctx.config
            .get("logfire_api_base")
            .map(String::as_str)
            .unwrap_or("https://logfire-us.pydantic.dev"),
    );
    let read_token = ctx
        .config
        .get("logfire_read_token")
        .cloned()
        .unwrap_or_default();
    if read_token.trim().is_empty() || read_token.contains("{secret:") {
        return Err(
            "logfire_query: missing Logfire read token; configure logfire_read_token secret"
                .to_string(),
        );
    }

    let mut url = format!(
        "{base_url}/v1/query?sql={}&limit={limit}&row_oriented={}",
        url_encode(&sql),
        if row_oriented { "true" } else { "false" }
    );
    if let Some(value) = min_timestamp.filter(|s| !s.trim().is_empty()) {
        url.push_str("&min_timestamp=");
        url.push_str(&url_encode(value));
    }
    if let Some(value) = max_timestamp.filter(|s| !s.trim().is_empty()) {
        url.push_str("&max_timestamp=");
        url.push_str(&url_encode(value));
    }

    ctx.log(
        "info",
        &format!(
            "tool_runner: querying Logfire, query_kind={query_kind}, limit={limit}, row_oriented={row_oriented}"
        ),
    );

    let headers = vec![
        ("authorization".to_string(), format!("Bearer {read_token}")),
        ("accept".to_string(), "application/json".to_string()),
    ];
    let resp = ctx.http_call("GET", &url, &headers, "")?;
    if resp.status < 200 || resp.status >= 300 {
        return Err(format!(
            "logfire_query failed (HTTP {}): {}",
            resp.status,
            truncate_tool_output(&resp.body, 1200)
        ));
    }

    let summarized = summarize_logfire_response(&resp.body, limit as usize);
    Ok(truncate_tool_output(&summarized, 6_000))
}

fn build_logfire_sql(input: &Value) -> Result<String, String> {
    let query_kind = input
        .get("query_kind")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("logfire_query: missing 'query_kind'")?;
    let query_kind = normalize_query_kind(query_kind);
    let limit = input
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(25)
        .clamp(1, 200);
    let service_name = input
        .get("service_name")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("temper-platform");
    let lookback_minutes = input
        .get("lookback_minutes")
        .and_then(Value::as_u64)
        .unwrap_or(240)
        .clamp(1, 10_080);
    let environment = input.get("environment").and_then(Value::as_str);
    let entity_type = input.get("entity_type").and_then(Value::as_str);
    let action = input.get("action").and_then(Value::as_str);
    let intent_text = input.get("intent_text").and_then(Value::as_str);
    let agent_id = input.get("agent_id").and_then(Value::as_str);

    let mut filters = vec![format!("service_name = {}", sql_string(service_name))];
    filters.push(format!(
        "start_timestamp >= now() - INTERVAL '{} minutes'",
        lookback_minutes
    ));
    if let Some(environment) = environment.filter(|value| !value.trim().is_empty()) {
        filters.push(format!(
            "deployment_environment = {}",
            sql_string(environment)
        ));
    }
    if let Some(entity_type) = entity_type.filter(|value| !value.trim().is_empty()) {
        let pattern = format!("%{entity_type}%");
        filters.push(format!(
            "(attributes->>'resource_type' = {value} OR attributes->>'entity_type' = {value} OR message ILIKE {pattern})",
            value = sql_string(entity_type),
            pattern = sql_string(&pattern),
        ));
    }
    if let Some(action) = action.filter(|value| !value.trim().is_empty()) {
        let pattern = format!("%{action}%");
        filters.push(format!(
            "(attributes->>'action' = {value} OR message ILIKE {pattern})",
            value = sql_string(action),
            pattern = sql_string(&pattern),
        ));
    }
    if let Some(intent_text) = intent_text.filter(|value| !value.trim().is_empty()) {
        let pattern = format!("%{intent_text}%");
        filters.push(format!("message ILIKE {}", sql_string(&pattern)));
    }
    if let Some(agent_id) = agent_id.filter(|value| !value.trim().is_empty()) {
        let pattern = format!("%{agent_id}%");
        filters.push(format!(
            "(attributes->>'agent_id' = {value} OR message ILIKE {pattern})",
            value = sql_string(agent_id),
            pattern = sql_string(&pattern),
        ));
    }

    let where_clause = filters.join("\n  AND ");
    let sql = match query_kind {
        "intent_failure_cluster" => format!(
            "SELECT\n  message,\n  coalesce(attributes->>'action', '') AS action,\n  coalesce(attributes->>'resource_type', attributes->>'entity_type', '') AS resource_type,\n  coalesce(attributes->>'decision', '') AS decision,\n  count(*) AS event_count,\n  max(start_timestamp) AS last_seen\nFROM records\nWHERE {where_clause}\n  AND (\n    message ILIKE '%unmet_intent%'\n    OR message ILIKE '%authz.%'\n    OR attributes->>'decision' = 'Deny'\n    OR message ILIKE '%failed%'\n  )\nGROUP BY message, action, resource_type, decision\nORDER BY event_count DESC, last_seen DESC\nLIMIT {limit}"
        ),
        "workflow_retries" => format!(
            "SELECT\n  start_timestamp,\n  message,\n  coalesce(attributes->>'action', '') AS action,\n  coalesce(attributes->>'resource_type', attributes->>'entity_type', '') AS resource_type,\n  coalesce(attributes->>'temper.from_status', '') AS from_status,\n  coalesce(attributes->>'temper.to_status', '') AS to_status,\n  coalesce(attributes->>'decision', '') AS decision\nFROM records\nWHERE {where_clause}\n  AND (\n    message ILIKE '%trajectory%'\n    OR message ILIKE '%dispatch%'\n    OR message ILIKE '%unmet_intent%'\n    OR attributes->>'action' IS NOT NULL\n  )\nORDER BY start_timestamp DESC\nLIMIT {limit}"
        ),
        "alternate_success_paths" => format!(
            "SELECT\n  start_timestamp,\n  message,\n  coalesce(attributes->>'action', '') AS action,\n  coalesce(attributes->>'resource_type', attributes->>'entity_type', '') AS resource_type,\n  coalesce(attributes->>'temper.from_status', '') AS from_status,\n  coalesce(attributes->>'temper.to_status', '') AS to_status,\n  coalesce(attributes->>'decision', '') AS decision\nFROM records\nWHERE {where_clause}\n  AND (\n    message ILIKE '%trajectory%'\n    OR message ILIKE '%unmet_intent%'\n    OR message ILIKE '%authz.%'\n    OR attributes->>'action' IS NOT NULL\n  )\nORDER BY start_timestamp DESC\nLIMIT {limit}"
        ),
        "intent_abandonment" => format!(
            "SELECT\n  coalesce(attributes->>'action', message) AS activity,\n  count(*) AS failed_event_count,\n  max(start_timestamp) AS last_seen\nFROM records\nWHERE {where_clause}\n  AND (\n    message ILIKE '%unmet_intent%'\n    OR message ILIKE '%authz.%'\n    OR attributes->>'decision' = 'Deny'\n    OR message ILIKE '%failed%'\n  )\nGROUP BY activity\nORDER BY failed_event_count DESC, last_seen DESC\nLIMIT {limit}"
        ),
        "recent_events" => format!(
            "SELECT start_timestamp, message, attributes\nFROM records\nWHERE {where_clause}\nORDER BY start_timestamp DESC\nLIMIT {limit}"
        ),
        other => return Err(format!("logfire_query: unsupported query_kind '{other}'")),
    };

    Ok(sql)
}

fn normalize_query_kind(query_kind: &str) -> &str {
    match query_kind {
        "workaround" => "alternate_success_paths",
        "governance_gap" => "intent_failure_cluster",
        other => other,
    }
}

fn sql_string(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn normalize_logfire_base_url(base: &str) -> String {
    let trimmed = base.trim().trim_end_matches('/');
    if trimmed.ends_with("/v1/query") {
        trimmed.trim_end_matches("/v1/query").to_string()
    } else {
        trimmed.to_string()
    }
}

fn truncate_tool_output(body: &str, max_chars: usize) -> String {
    if body.chars().count() <= max_chars {
        return body.to_string();
    }
    let truncated: String = body.chars().take(max_chars).collect();
    format!(
        "{truncated}\n\n[truncated {} chars; refine the query with a tighter filter or lower limit]",
        body.chars().count().saturating_sub(max_chars)
    )
}

fn summarize_logfire_response(body: &str, limit: usize) -> String {
    let Ok(parsed) = serde_json::from_str::<Value>(body) else {
        return body.to_string();
    };

    if let Some(rows) = parsed.get("rows").and_then(Value::as_array) {
        let compact_rows: Vec<Value> = rows
            .iter()
            .take(limit.min(8))
            .map(compact_logfire_row)
            .collect();
        return json!({
            "row_count": rows.len(),
            "rows": compact_rows,
            "truncated": rows.len() > compact_rows.len()
        })
        .to_string();
    }

    if let Some(columns) = parsed.get("columns").and_then(Value::as_array) {
        let rows = rows_from_columnar(columns, limit.min(8));
        let row_count = columnar_row_count(columns);
        return json!({
            "row_count": row_count,
            "rows": rows,
            "truncated": row_count > rows.len()
        })
        .to_string();
    }

    parsed.to_string()
}

fn columnar_row_count(columns: &[Value]) -> usize {
    columns
        .iter()
        .filter_map(|column| {
            column
                .get("values")
                .and_then(Value::as_array)
                .map(std::vec::Vec::len)
        })
        .max()
        .unwrap_or(0)
}

fn rows_from_columnar(columns: &[Value], row_limit: usize) -> Vec<Value> {
    let row_count = columnar_row_count(columns);
    let mut rows = Vec::new();
    for row_index in 0..row_count.min(row_limit) {
        let mut row = serde_json::Map::new();
        for column in columns {
            let Some(name) = column.get("name").and_then(Value::as_str) else {
                continue;
            };
            let Some(values) = column.get("values").and_then(Value::as_array) else {
                continue;
            };
            if let Some(value) = values.get(row_index)
                && !value.is_null()
            {
                row.insert(name.to_string(), value.clone());
            }
        }
        rows.push(compact_logfire_row(&Value::Object(row)));
    }
    rows
}

fn compact_logfire_row(row: &Value) -> Value {
    let Some(obj) = row.as_object() else {
        return row.clone();
    };

    let mut compact = serde_json::Map::new();
    for key in [
        "start_timestamp",
        "created_at",
        "last_seen",
        "message",
        "span_name",
        "activity",
        "action",
        "resource_type",
        "decision",
        "service_name",
        "deployment_environment",
        "event_count",
        "failed_event_count",
        "duration",
    ] {
        if let Some(value) = obj.get(key)
            && !value.is_null()
            && !value.as_str().is_some_and(str::is_empty)
        {
            compact.insert(key.to_string(), value.clone());
        }
    }

    if let Some(attributes) = obj.get("attributes").and_then(Value::as_object) {
        copy_attribute(attributes, &mut compact, "action", "action");
        copy_attribute(attributes, &mut compact, "resource_type", "resource_type");
        copy_attribute(attributes, &mut compact, "entity_type", "entity_type");
        copy_attribute(attributes, &mut compact, "decision", "decision");
        copy_attribute(attributes, &mut compact, "agent_id", "agent_id");
        copy_attribute(attributes, &mut compact, "tenant", "tenant");
        copy_attribute(attributes, &mut compact, "temper.from_status", "from_status");
        copy_attribute(attributes, &mut compact, "temper.to_status", "to_status");
    }

    Value::Object(compact)
}

fn copy_attribute(
    attributes: &serde_json::Map<String, Value>,
    compact: &mut serde_json::Map<String, Value>,
    source_key: &str,
    target_key: &str,
) {
    if compact.contains_key(target_key) {
        return;
    }
    let Some(value) = attributes.get(source_key) else {
        return;
    };
    if value.is_null() || value.as_str().is_some_and(str::is_empty) {
        return;
    }
    compact.insert(target_key.to_string(), value.clone());
}

// --- Local sandbox API (our custom HTTP server) ---

/// Read file via local sandbox API.
fn read_file_local(ctx: &Context, sandbox_url: &str, full_path: &str) -> Result<String, String> {
    let url = format!("{sandbox_url}/v1/fs/file?path={}", url_encode(full_path));
    let resp = ctx.http_get(&url)?;
    if resp.status == 200 {
        Ok(resp.body)
    } else {
        Err(format!("read failed (HTTP {}): {}", resp.status, resp.body))
    }
}

/// Write file via local sandbox API.
fn write_file_local(
    ctx: &Context,
    sandbox_url: &str,
    full_path: &str,
    content: &str,
) -> Result<String, String> {
    let url = format!("{sandbox_url}/v1/fs/file?path={}", url_encode(full_path));
    let headers = vec![("content-type".to_string(), "text/plain".to_string())];
    let resp = ctx.http_call("PUT", &url, &headers, content)?;
    if resp.status >= 200 && resp.status < 300 {
        Ok(format!("File written: {full_path}"))
    } else {
        Err(format!(
            "write failed (HTTP {}): {}",
            resp.status, resp.body
        ))
    }
}

/// Run bash command via local sandbox API.
fn run_bash_local(
    ctx: &Context,
    sandbox_url: &str,
    command: &str,
    workdir: &str,
) -> Result<String, String> {
    let url = format!("{sandbox_url}/v1/processes/run");
    let body = serde_json::to_string(&json!({
        "command": command,
        "workdir": workdir,
    }))
    .unwrap_or_default();

    let headers = vec![("content-type".to_string(), "application/json".to_string())];
    let resp = ctx.http_call("POST", &url, &headers, &body)?;

    if resp.status >= 200 && resp.status < 300 {
        if let Ok(parsed) = serde_json::from_str::<Value>(&resp.body) {
            let stdout = parsed.get("stdout").and_then(|v| v.as_str()).unwrap_or("");
            let stderr = parsed.get("stderr").and_then(|v| v.as_str()).unwrap_or("");
            let exit_code = parsed
                .get("exit_code")
                .and_then(|v| v.as_i64())
                .unwrap_or(-1);

            let mut output = String::new();
            if !stdout.is_empty() {
                output.push_str(stdout);
            }
            if !stderr.is_empty() {
                if !output.is_empty() {
                    output.push('\n');
                }
                output.push_str("STDERR: ");
                output.push_str(stderr);
            }
            if exit_code != 0 {
                output.push_str(&format!("\n(exit code: {exit_code})"));
            }
            Ok(output)
        } else {
            Ok(resp.body)
        }
    } else {
        Err(format!("bash failed (HTTP {}): {}", resp.status, resp.body))
    }
}

// --- E2B envd API (plain HTTP for files, port 49983) ---

/// Read file via E2B envd HTTP API: GET /files?path=...
fn read_file_e2b(ctx: &Context, sandbox_url: &str, full_path: &str) -> Result<String, String> {
    let url = format!("{sandbox_url}/files?path={}", url_encode(full_path));
    let resp = ctx.http_get(&url)?;
    if resp.status == 200 {
        Ok(resp.body)
    } else {
        Err(format!(
            "E2B read failed (HTTP {}): {}",
            resp.status, resp.body
        ))
    }
}

/// Write file via E2B envd HTTP API: POST /files?path=<full_path> with multipart file.
/// The E2B envd expects `path` as a query parameter (full file path) and the file
/// content as a multipart form-data upload with field name "file".
fn write_file_e2b(
    ctx: &Context,
    sandbox_url: &str,
    full_path: &str,
    content: &str,
) -> Result<String, String> {
    let url = format!("{sandbox_url}/files?path={}", url_encode(full_path));
    let boundary = "----TemperWasmBoundary7MA4YWxkTrZu0gW";
    let body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{full_path}\"\r\nContent-Type: application/octet-stream\r\n\r\n{content}\r\n--{boundary}--\r\n"
    );

    let headers = vec![(
        "content-type".to_string(),
        format!("multipart/form-data; boundary={boundary}"),
    )];
    let resp = ctx.http_call("POST", &url, &headers, &body)?;
    if resp.status >= 200 && resp.status < 300 {
        Ok(format!("File written: {full_path}"))
    } else {
        Err(format!(
            "E2B write failed (HTTP {}): {}",
            resp.status, resp.body
        ))
    }
}

/// Run bash command via E2B envd Connect protocol: POST /process.Process/Start.
///
/// Uses the `host_connect_call` host function which handles Connect binary
/// frame parsing. The envd daemon returns server-streamed process output
/// frames, each containing stdout/stderr/exitCode fields.
fn run_bash_e2b(
    ctx: &Context,
    sandbox_url: &str,
    command: &str,
    workdir: &str,
) -> Result<String, String> {
    let url = format!("{sandbox_url}/process.Process/Start");
    let body = serde_json::to_string(&json!({
        "command": command,
        "envs": {},
        "cwd": workdir,
    }))
    .unwrap_or_default();

    let headers: Vec<(String, String)> = Vec::new();
    let frames = ctx.connect_call(&url, &headers, &body)?;

    if frames.is_empty() {
        return Ok(String::new());
    }

    // Parse each frame — E2B process output has stdout, stderr, exitCode fields
    let mut stdout = String::new();
    let mut stderr = String::new();
    let mut exit_code: i64 = 0;

    for frame_str in &frames {
        if let Ok(frame) = serde_json::from_str::<Value>(frame_str) {
            if let Some(event) = frame.get("event") {
                // Connect-streamed events may nest the data
                if let Some(s) = event.get("stdout").and_then(|v| v.as_str()) {
                    stdout.push_str(s);
                }
                if let Some(s) = event.get("stderr").and_then(|v| v.as_str()) {
                    stderr.push_str(s);
                }
                if let Some(c) = event.get("exitCode").and_then(|v| v.as_i64()) {
                    exit_code = c;
                }
            } else {
                // Direct fields
                if let Some(s) = frame.get("stdout").and_then(|v| v.as_str()) {
                    stdout.push_str(s);
                }
                if let Some(s) = frame.get("stderr").and_then(|v| v.as_str()) {
                    stderr.push_str(s);
                }
                if let Some(c) = frame.get("exitCode").and_then(|v| v.as_i64()) {
                    exit_code = c;
                }
            }
        }
    }

    let mut output = String::new();
    if !stdout.is_empty() {
        output.push_str(&stdout);
    }
    if !stderr.is_empty() {
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str("STDERR: ");
        output.push_str(&stderr);
    }
    if exit_code != 0 {
        output.push_str(&format!("\n(exit code: {exit_code})"));
    }
    Ok(output)
}

/// Read conversation from TemperFS File entity.
fn read_conversation_from_temperfs(
    ctx: &Context,
    temper_api_url: &str,
    tenant: &str,
    file_id: &str,
) -> Result<Vec<Value>, String> {
    let url = format!("{temper_api_url}/tdata/Files('{file_id}')/$value");
    let headers = vec![
        ("x-tenant-id".to_string(), tenant.to_string()),
        ("x-temper-principal-kind".to_string(), "admin".to_string()),
        ("accept".to_string(), "application/json".to_string()),
    ];

    let resp = ctx
        .http_call("GET", &url, &headers, "")
        .map_err(|e| format!("TemperFS conversation read failed: {e}"))?;

    if resp.status != 200 {
        return Err(format!(
            "TemperFS conversation read failed (HTTP {}): {}",
            resp.status,
            &resp.body[..resp.body.len().min(200)]
        ));
    }

    let parsed: Value = serde_json::from_str(&resp.body)
        .map_err(|e| format!("TemperFS conversation parse failed: {e}"))?;

    Ok(parsed
        .get("messages")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default())
}

/// Resolve a path relative to the working directory.
fn resolve_path(workdir: &str, path: &str) -> String {
    if path.starts_with('/') {
        path.to_string()
    } else {
        format!("{}/{}", workdir.trim_end_matches('/'), path)
    }
}

/// Minimal URL encoding for path parameters.
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'/' => {
                out.push(b as char);
            }
            _ => {
                out.push_str(&format!("%{b:02X}"));
            }
        }
    }
    out
}

// --- Sandbox Fsync to TemperFS ---

/// File metadata from sandbox `find` + `stat`.
struct FileEntry {
    size_bytes: u64,
    mtime: u64,
}

/// Manifest entry stored in TemperFS.
struct ManifestEntry {
    file_id: String,
    size_bytes: u64,
    mtime: u64,
}

/// Enumerate all files in the sandbox workspace using `find` + `stat`.
/// Returns a map of path → FileEntry with size and mtime.
fn enumerate_sandbox_files(
    ctx: &Context,
    sandbox_url: &str,
    workdir: &str,
    exclude: &str,
    e2b: bool,
) -> Result<BTreeMap<String, FileEntry>, String> {
    // Build exclude flags from comma-separated patterns
    let mut exclude_flags = String::new();
    for pattern in exclude.split(',') {
        let pattern = pattern.trim();
        if !pattern.is_empty() {
            exclude_flags.push_str(&format!(" -not -path '*/{pattern}/*'"));
        }
    }

    // Use stat format appropriate for the OS
    let stat_fmt = if e2b {
        // Linux/GNU stat: %n=name %s=size %Y=mtime
        "-exec stat --format='%n %s %Y' {} +"
    } else {
        // macOS/BSD stat: %N=name %z=size %m=mtime
        "-exec stat -f '%N %z %m' {} +"
    };

    let command = format!("find {workdir} -type f -not -path '*/.*'{exclude_flags} {stat_fmt}");

    let output = if e2b {
        run_bash_e2b(ctx, sandbox_url, &command, workdir)?
    } else {
        run_bash_local(ctx, sandbox_url, &command, workdir)?
    };

    let mut files = BTreeMap::new();
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("STDERR:") {
            continue;
        }
        // Parse: "path size mtime" — split from the right since path may contain spaces
        let parts: Vec<&str> = line.rsplitn(3, ' ').collect();
        if parts.len() == 3 {
            let mtime: u64 = parts[0].parse().unwrap_or(0);
            let size_bytes: u64 = parts[1].parse().unwrap_or(0);
            let path = parts[2].to_string();
            files.insert(path, FileEntry { size_bytes, mtime });
        }
    }

    Ok(files)
}

/// Read the file manifest from TemperFS.
fn read_manifest(
    ctx: &Context,
    temper_api_url: &str,
    tenant: &str,
    manifest_file_id: &str,
) -> Result<BTreeMap<String, ManifestEntry>, String> {
    let url = format!("{temper_api_url}/tdata/Files('{manifest_file_id}')/$value");
    let headers = vec![
        ("x-tenant-id".to_string(), tenant.to_string()),
        ("x-temper-principal-kind".to_string(), "admin".to_string()),
        ("accept".to_string(), "application/json".to_string()),
    ];

    let resp = ctx.http_call("GET", &url, &headers, "")?;
    if resp.status != 200 {
        return Ok(BTreeMap::new());
    }

    let parsed: Value = serde_json::from_str(&resp.body).unwrap_or(json!({}));
    let files_obj = parsed.get("files").and_then(|v| v.as_object());

    let mut manifest = BTreeMap::new();
    if let Some(files) = files_obj {
        for (path, entry) in files {
            let file_id = entry
                .get("file_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let size_bytes = entry
                .get("size_bytes")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let mtime = entry.get("mtime").and_then(|v| v.as_u64()).unwrap_or(0);
            if !file_id.is_empty() {
                manifest.insert(
                    path.clone(),
                    ManifestEntry {
                        file_id,
                        size_bytes,
                        mtime,
                    },
                );
            }
        }
    }

    Ok(manifest)
}

/// Simple hash function for deterministic File entity IDs.
/// Returns first 16 hex chars of a djb2 hash.
fn simple_hash(input: &str) -> String {
    let mut hash: u64 = 5381;
    for b in input.bytes() {
        hash = hash.wrapping_mul(33).wrapping_add(b as u64);
    }
    format!("{hash:016x}")
}

/// Sync sandbox files to TemperFS. Returns the number of files synced.
fn sync_files_to_temperfs(
    ctx: &Context,
    sandbox_url: &str,
    temper_api_url: &str,
    tenant: &str,
    workspace_id: &str,
    manifest_file_id: &str,
    workdir: &str,
    e2b: bool,
    max_file_bytes: u64,
    exclude: &str,
) -> Result<usize, String> {
    // 1. Enumerate current sandbox files with stat metadata
    let current_files = enumerate_sandbox_files(ctx, sandbox_url, workdir, exclude, e2b)?;
    ctx.log(
        "info",
        &format!(
            "tool_runner: fsync enumerated {} files",
            current_files.len()
        ),
    );

    // 2. Read previous manifest from TemperFS
    let old_manifest = read_manifest(ctx, temper_api_url, tenant, manifest_file_id)?;

    let headers = vec![
        ("content-type".to_string(), "application/json".to_string()),
        ("x-tenant-id".to_string(), tenant.to_string()),
        ("x-temper-principal-kind".to_string(), "admin".to_string()),
    ];

    let file_url = format!("{temper_api_url}/tdata/Files");
    let mut new_manifest: BTreeMap<String, Value> = BTreeMap::new();
    let mut synced_count: usize = 0;

    // 3. Sync new/modified files
    for (path, entry) in &current_files {
        // Check if unchanged (size AND mtime match)
        if let Some(old_entry) = old_manifest.get(path) {
            if old_entry.size_bytes == entry.size_bytes && old_entry.mtime == entry.mtime {
                // Unchanged — carry forward manifest entry without reading file
                new_manifest.insert(
                    path.clone(),
                    json!({
                        "file_id": old_entry.file_id,
                        "size_bytes": old_entry.size_bytes,
                        "mtime": old_entry.mtime,
                    }),
                );
                continue;
            }
        }

        // File is new or modified — read from sandbox
        let content = if e2b {
            read_file_e2b(ctx, sandbox_url, path)
        } else {
            read_file_local(ctx, sandbox_url, path)
        };

        let content = match content {
            Ok(c) => c,
            Err(e) => {
                ctx.log(
                    "warn",
                    &format!("tool_runner: fsync skip {path}: read failed: {e}"),
                );
                continue;
            }
        };

        if content.len() as u64 > max_file_bytes {
            ctx.log(
                "warn",
                &format!(
                    "tool_runner: fsync skip {path}: {} bytes exceeds max {}",
                    content.len(),
                    max_file_bytes
                ),
            );
            continue;
        }

        // Deterministic File entity ID from workspace + path
        let file_entity_id = format!("wsf-{}", simple_hash(&format!("{workspace_id}:{path}")));

        // Create File entity (ignore 409 = already exists)
        let create_body = json!({
            "FileId": &file_entity_id,
            "workspace_id": workspace_id,
            "name": path.rsplit('/').next().unwrap_or("file"),
            "mime_type": "text/plain",
            "path": path,
        });
        let _ = ctx.http_call("POST", &file_url, &headers, &create_body.to_string());

        // Upload content (CAS dedup handles unchanged content)
        let value_url = format!("{temper_api_url}/tdata/Files('{file_entity_id}')/$value");
        match ctx.http_call("PUT", &value_url, &headers, &content) {
            Ok(resp) if resp.status >= 200 && resp.status < 300 => {
                synced_count += 1;
                new_manifest.insert(
                    path.clone(),
                    json!({
                        "file_id": file_entity_id,
                        "size_bytes": entry.size_bytes,
                        "mtime": entry.mtime,
                    }),
                );
            }
            Ok(resp) => {
                ctx.log(
                    "warn",
                    &format!(
                        "tool_runner: fsync upload failed for {path} (HTTP {})",
                        resp.status
                    ),
                );
            }
            Err(e) => {
                ctx.log(
                    "warn",
                    &format!("tool_runner: fsync upload failed for {path}: {e}"),
                );
            }
        }
    }

    // 4. Handle deletions — archive files that no longer exist in sandbox
    for (path, old_entry) in &old_manifest {
        if !current_files.contains_key(path) {
            let archive_url = format!(
                "{temper_api_url}/tdata/Files('{}')/TemperFS.File.Archive",
                old_entry.file_id
            );
            match ctx.http_call("POST", &archive_url, &headers, "{}") {
                Ok(_) => ctx.log(
                    "info",
                    &format!("tool_runner: fsync archived deleted file {path}"),
                ),
                Err(e) => ctx.log(
                    "warn",
                    &format!("tool_runner: fsync archive failed for {path}: {e}"),
                ),
            }
        }
    }

    // 5. Write updated manifest to TemperFS
    let manifest_body = json!({ "files": new_manifest }).to_string();
    let manifest_url = format!("{temper_api_url}/tdata/Files('{manifest_file_id}')/$value");
    ctx.http_call("PUT", &manifest_url, &headers, &manifest_body)
        .map_err(|e| format!("manifest write failed: {e}"))?;

    Ok(synced_count)
}

// --- Entity tool dispatch ---

fn emit_progress_ignore(ctx: &Context, payload: Value) {
    let _ = ctx.emit_progress(&payload);
}

fn send_heartbeat(ctx: &Context, temper_api_url: &str, tenant: &str) -> Result<(), String> {
    let url = format!(
        "{temper_api_url}/tdata/TemperAgents('{}')/Temper.Agent.TemperAgent.Heartbeat",
        ctx.entity_id
    );
    let body = json!({ "last_heartbeat_at": "alive" });
    let _ = ctx.http_call("POST", &url, &odata_headers(tenant), &body.to_string())?;
    Ok(())
}

fn validate_tool_input(tool_name: &str, input: &Value) -> Result<(), String> {
    let object = input
        .as_object()
        .ok_or_else(|| format!("{tool_name}: input must be an object"))?;
    let required: &[&str] = match tool_name {
        "read" => &["path"],
        "write" => &["path", "content"],
        "edit" => &["path", "old_string", "new_string"],
        "bash" => &["command"],
        "save_memory" => &["key", "content"],
        "recall_memory" => &["query"],
        "spawn_agent" => &["task"],
        "abort_agent" => &["agent_id"],
        "steer_agent" => &["agent_id", "message"],
        "read_entity" => &["file_id"],
        "run_coding_agent" => &["agent_type", "task"],
        _ => &[],
    };
    for key in required {
        let Some(value) = object.get(*key) else {
            return Err(format!("{tool_name}: missing '{key}'"));
        };
        if value.is_null() || value.as_str().is_some_and(str::is_empty) {
            return Err(format!("{tool_name}: '{key}' must not be empty"));
        }
    }
    Ok(())
}

fn evaluate_before_hooks(
    ctx: &Context,
    temper_api_url: &str,
    tenant: &str,
    soul_id: &str,
    hook_policy: &str,
    tool_name: &str,
) -> Result<Option<String>, String> {
    if hook_policy == "none" || soul_id.is_empty() {
        return Ok(None);
    }
    let hooks = load_matching_hooks(ctx, temper_api_url, tenant, soul_id, "before", tool_name)?;
    for hook in hooks {
        let action = entity_field_str(&hook, &["HookAction"]).unwrap_or("log");
        let name = entity_field_str(&hook, &["Name"]).unwrap_or("hook");
        match action {
            "block" => {
                return Ok(Some(format!(
                    "tool blocked by hook '{name}' for tool '{tool_name}'"
                )))
            }
            "log" => ctx.log("info", &format!("tool_runner: before hook '{name}' matched {tool_name}")),
            _ => {}
        }
    }
    Ok(None)
}

fn apply_after_hooks(
    ctx: &Context,
    temper_api_url: &str,
    tenant: &str,
    soul_id: &str,
    hook_policy: &str,
    tool_name: &str,
    mut output: String,
) -> Result<String, String> {
    if hook_policy != "full_hooks" || soul_id.is_empty() {
        return Ok(output);
    }
    let hooks = load_matching_hooks(ctx, temper_api_url, tenant, soul_id, "after", tool_name)?;
    for hook in hooks {
        let action = entity_field_str(&hook, &["HookAction"]).unwrap_or("log");
        let name = entity_field_str(&hook, &["Name"]).unwrap_or("hook");
        match action {
            "modify" => {
                output = format!("[modified by hook:{name}]\n{output}");
            }
            "log" => ctx.log("info", &format!("tool_runner: after hook '{name}' matched {tool_name}")),
            _ => {}
        }
    }
    Ok(output)
}

fn load_matching_hooks(
    ctx: &Context,
    temper_api_url: &str,
    tenant: &str,
    soul_id: &str,
    hook_type: &str,
    tool_name: &str,
) -> Result<Vec<Value>, String> {
    let url = format!("{temper_api_url}/tdata/ToolHooks");
    let resp = ctx.http_call("GET", &url, &odata_headers(tenant), "")?;
    if resp.status != 200 {
        return Ok(Vec::new());
    }
    let parsed: Value = serde_json::from_str(&resp.body).unwrap_or_else(|_| json!({ "value": [] }));
    let hooks = parsed
        .get("value")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|hook| {
            entity_field_str(hook, &["Status"]) == Some("Active")
                && entity_field_str(hook, &["SoulId"]).unwrap_or("") == soul_id
                && entity_field_str(hook, &["HookType"]).unwrap_or("") == hook_type
                && hook_matches(
                    entity_field_str(hook, &["ToolPattern"]).unwrap_or(".*"),
                    tool_name,
                )
        })
        .collect::<Vec<_>>();
    Ok(hooks)
}

fn hook_matches(pattern: &str, tool_name: &str) -> bool {
    let pattern = pattern.trim();
    if pattern.is_empty() || pattern == ".*" || pattern == "*" {
        return true;
    }
    if pattern.contains('|') {
        return pattern.split('|').any(|part| part.trim() == tool_name);
    }
    pattern == tool_name
}

fn is_entity_tool(name: &str) -> bool {
    matches!(
        name,
        "save_memory"
            | "recall_memory"
            | "spawn_agent"
            | "list_agents"
            | "abort_agent"
            | "steer_agent"
            | "read_entity"
            | "run_coding_agent"
    )
}

fn execute_entity_tool(
    ctx: &Context,
    temper_api_url: &str,
    tenant: &str,
    fields: &Value,
    tool_name: &str,
    input: &Value,
) -> Result<String, String> {
    match tool_name {
        "save_memory" => {
            let key = input.get("key").and_then(|v| v.as_str()).ok_or("save_memory: missing 'key'")?;
            let content = input.get("content").and_then(|v| v.as_str()).ok_or("save_memory: missing 'content'")?;
            let memory_type = input.get("memory_type").and_then(|v| v.as_str()).unwrap_or("reference");
            let soul_id = fields.get("soul_id").and_then(|v| v.as_str()).unwrap_or("");
            let agent_id = ctx.entity_state.get("entity_id").and_then(|v| v.as_str()).unwrap_or("");
            let body = json!({
                "Key": key, "Content": content, "MemoryType": memory_type,
                "SoulId": soul_id, "AuthorAgentId": agent_id,
            });
            let url = format!("{temper_api_url}/tdata/AgentMemorys");
            let resp = ctx.http_call("POST", &url, &odata_headers(tenant), &serde_json::to_string(&body).unwrap_or_default())?;
            if resp.status >= 200 && resp.status < 300 {
                let parsed: Value = serde_json::from_str(&resp.body).unwrap_or(json!({}));
                let entity_id = parsed
                    .get("entity_id")
                    .or_else(|| parsed.get("Id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if !entity_id.is_empty() {
                    let action_url = format!(
                        "{temper_api_url}/tdata/AgentMemorys('{entity_id}')/Temper.Agent.AgentMemory.Save"
                    );
                    let _ = ctx.http_call("POST", &action_url, &odata_headers(tenant), "{}");
                }
                Ok(format!("Memory saved: key={key}, type={memory_type}"))
            } else {
                Err(format!("save_memory failed (HTTP {}): {}", resp.status, &resp.body[..resp.body.len().min(200)]))
            }
        }
        "recall_memory" => {
            let query = input.get("query").and_then(|v| v.as_str()).ok_or("recall_memory: missing 'query'")?;
            let soul_id = fields.get("soul_id").and_then(|v| v.as_str()).unwrap_or("");
            let url = format!("{temper_api_url}/tdata/AgentMemorys");
            let resp = ctx.http_call("GET", &url, &odata_headers(tenant), "")?;
            if resp.status == 200 {
                let parsed: Value = serde_json::from_str(&resp.body).unwrap_or(json!({}));
                let memories = parsed
                    .get("value")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|mem| {
                        entity_field_str(mem, &["Status"]) == Some("Active")
                            && entity_field_str(mem, &["SoulId"]).unwrap_or("") == soul_id
                            && (entity_field_str(mem, &["Key"]).unwrap_or("").contains(query)
                                || entity_field_str(mem, &["Content"]).unwrap_or("").contains(query))
                    })
                    .collect::<Vec<_>>();
                if memories.is_empty() {
                    Ok("No memories found matching query.".to_string())
                } else {
                    let mut result = String::new();
                    for mem in &memories {
                        let k = entity_field_str(mem, &["Key"]).unwrap_or("?");
                        let c = entity_field_str(mem, &["Content"]).unwrap_or("");
                        let t = entity_field_str(mem, &["MemoryType"]).unwrap_or("?");
                        result.push_str(&format!("- [{t}] {k}: {c}\n"));
                    }
                    Ok(result)
                }
            } else {
                Err(format!("recall_memory failed (HTTP {})", resp.status))
            }
        }
        "spawn_agent" => {
            let task = input.get("task").and_then(|v| v.as_str()).ok_or("spawn_agent: missing 'task'")?;
            let requested_id = input.get("agent_id").and_then(|v| v.as_str()).unwrap_or("");
            let model = input.get("model").and_then(|v| v.as_str())
                .unwrap_or_else(|| fields.get("model").and_then(|v| v.as_str()).unwrap_or("claude-sonnet-4-20250514"));
            let provider = input.get("provider").and_then(|v| v.as_str())
                .unwrap_or_else(|| fields.get("provider").and_then(|v| v.as_str()).unwrap_or("anthropic"));
            let max_turns = input.get("max_turns").and_then(|v| v.as_i64()).unwrap_or(20);
            let tools = input.get("tools").and_then(|v| v.as_str())
                .unwrap_or_else(|| fields.get("tools_enabled").and_then(|v| v.as_str()).unwrap_or("read,write,edit,bash"));
            let soul_id = input.get("soul_id").and_then(|v| v.as_str())
                .unwrap_or_else(|| fields.get("soul_id").and_then(|v| v.as_str()).unwrap_or(""));
            let parent_id = ctx.entity_state.get("entity_id").and_then(|v| v.as_str()).unwrap_or("");
            let sandbox_url = fields.get("sandbox_url").and_then(|v| v.as_str()).unwrap_or("");
            let workdir = fields.get("workdir").and_then(|v| v.as_str()).unwrap_or("/workspace");
            let background = input.get("background").and_then(|v| v.as_bool()).unwrap_or(false);
            let current_depth = fields.get("agent_depth").and_then(|v| v.as_i64()).unwrap_or(0);
            if current_depth >= 5 {
                return Err("spawn_agent: agent_depth guard hit (max depth 5)".to_string());
            }

            // 1. Create child entity
            let url = format!("{temper_api_url}/tdata/TemperAgents");
            let create_body = if requested_id.is_empty() {
                "{}".to_string()
            } else {
                json!({ "TemperAgentId": requested_id }).to_string()
            };
            let resp = ctx.http_call("POST", &url, &odata_headers(tenant), &create_body)?;
            if resp.status < 200 || resp.status >= 300 {
                return Err(format!("spawn_agent: create failed (HTTP {})", resp.status));
            }
            let parsed: Value = serde_json::from_str(&resp.body).unwrap_or(json!({}));
            let child_id = parsed
                .get("entity_id")
                .or_else(|| parsed.get("Id"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if child_id.is_empty() {
                return Err("spawn_agent: created entity has no Id".to_string());
            }

            // 2. Configure
            let config_body = json!({
                "system_prompt": input.get("system_prompt").and_then(Value::as_str).unwrap_or(""),
                "model": model, "provider": provider, "max_turns": max_turns.to_string(), "tools_enabled": tools,
                "soul_id": soul_id, "user_message": task, "parent_agent_id": parent_id,
                "sandbox_url": sandbox_url, "workdir": workdir, "agent_depth": current_depth + 1,
            });
            let config_url = format!(
                "{temper_api_url}/tdata/TemperAgents('{child_id}')/Temper.Agent.TemperAgent.Configure"
            );
            let resp2 = ctx.http_call("POST", &config_url, &odata_headers(tenant), &serde_json::to_string(&config_body).unwrap_or_default())?;
            if resp2.status < 200 || resp2.status >= 300 {
                return Err(format!("spawn_agent: configure failed (HTTP {})", resp2.status));
            }

            // 3. Provision
            let prov_url = format!(
                "{temper_api_url}/tdata/TemperAgents('{child_id}')/Temper.Agent.TemperAgent.Provision"
            );
            let resp3 = ctx.http_call("POST", &prov_url, &odata_headers(tenant), "{}")?;
            if resp3.status < 200 || resp3.status >= 300 {
                return Err(format!("spawn_agent: provision failed (HTTP {})", resp3.status));
            }
            if background {
                return Ok(format!(
                    "Child agent {child_id} created and provisioned in background."
                ));
            }

            // 4. Wait for completion
            let wait_url = format!(
                "{temper_api_url}/observe/entities/TemperAgent/{child_id}/wait?statuses=Completed,Failed,Cancelled&timeout_ms=300000&poll_ms=250"
            );
            let wait_headers = vec![
                ("x-tenant-id".to_string(), tenant.to_string()),
                ("x-temper-principal-kind".to_string(), "admin".to_string()),
                ("accept".to_string(), "application/json".to_string()),
            ];
            let resp4 = ctx.http_call("GET", &wait_url, &wait_headers, "")?;
            if resp4.status == 200 {
                let result: Value = serde_json::from_str(&resp4.body).unwrap_or(json!({}));
                let status = result.get("status").and_then(|v| v.as_str()).unwrap_or("unknown");
                let agent_result = result
                    .get("fields")
                    .and_then(|v| v.get("result"))
                    .or_else(|| result.get("fields").and_then(|v| v.get("Result")))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                Ok(format!("Child agent {child_id} finished with status={status}. Result: {agent_result}"))
            } else {
                Ok(format!("Child agent {child_id} created and provisioned (poll for status)."))
            }
        }
        "list_agents" => {
            let parent_id = ctx.entity_state.get("entity_id").and_then(|v| v.as_str()).unwrap_or("");
            let agents = list_temper_agents(ctx, temper_api_url, tenant)?;
            let child_agents = agents
                .into_iter()
                .filter(|agent| {
                    entity_field_str(agent, &["ParentAgentId"]).unwrap_or("") == parent_id
                })
                .collect::<Vec<_>>();
            if child_agents.is_empty() {
                Ok("No child agents found.".to_string())
            } else {
                let mut result = String::new();
                for agent in &child_agents {
                    let id = agent_display_id(agent);
                    let status = entity_field_str(agent, &["Status"]).unwrap_or("?");
                    result.push_str(&format!("- {id}: {status}\n"));
                }
                Ok(result)
            }
        }
        "abort_agent" => {
            let agent_id = input.get("agent_id").and_then(|v| v.as_str()).ok_or("abort_agent: missing 'agent_id'")?;
            let resolved_agent_id = resolve_agent_reference(ctx, temper_api_url, tenant, agent_id)?
                .map(|agent| agent_entity_id(&agent).to_string())
                .unwrap_or_else(|| agent_id.to_string());
            let url = format!(
                "{temper_api_url}/tdata/TemperAgents('{resolved_agent_id}')/Temper.Agent.TemperAgent.Cancel"
            );
            let resp = ctx.http_call("POST", &url, &odata_headers(tenant), "{}")?;
            if resp.status >= 200 && resp.status < 300 {
                Ok(format!("Agent {resolved_agent_id} cancelled."))
            } else {
                Err(format!("cancel_agent failed (HTTP {})", resp.status))
            }
        }
        "steer_agent" => {
            let agent_id = input.get("agent_id").and_then(|v| v.as_str()).ok_or("steer_agent: missing 'agent_id'")?;
            let message = input.get("message").and_then(|v| v.as_str()).ok_or("steer_agent: missing 'message'")?;
            let Some(agent) = resolve_agent_reference(ctx, temper_api_url, tenant, agent_id)? else {
                return Err(format!("steer_agent: agent '{agent_id}' not found"));
            };
            let resolved_agent_id = agent_entity_id(&agent);
            let existing = entity_field_str(&agent, &["SteeringMessages"])
                .map(str::to_string)
                .unwrap_or_else(|| "[]".to_string());
            let mut queue: Vec<Value> = serde_json::from_str(&existing).unwrap_or_default();
            queue.push(json!({ "content": message }));
            let body = json!({
                "steering_messages": serde_json::to_string(&queue).unwrap_or_else(|_| "[]".to_string())
            });
            let url = format!(
                "{temper_api_url}/tdata/TemperAgents('{resolved_agent_id}')/Temper.Agent.TemperAgent.Steer"
            );
            let resp = ctx.http_call(
                "POST",
                &url,
                &odata_headers(tenant),
                &serde_json::to_string(&body).unwrap_or_default(),
            )?;
            if resp.status >= 200 && resp.status < 300 {
                Ok(format!(
                    "Steering message sent to agent {}.",
                    agent_display_id(&agent)
                ))
            } else {
                Err(format!("steer_agent failed (HTTP {})", resp.status))
            }
        }
        "read_entity" => {
            let file_id = input.get("file_id").and_then(|v| v.as_str()).ok_or("read_entity: missing 'file_id'")?;
            let url = format!("{temper_api_url}/tdata/Files('{file_id}')/$value");
            let headers = vec![
                ("x-tenant-id".to_string(), tenant.to_string()),
                ("x-temper-principal-kind".to_string(), "admin".to_string()),
            ];
            let resp = ctx.http_call("GET", &url, &headers, "")?;
            if resp.status == 200 { Ok(resp.body) }
            else { Err(format!("read_entity failed (HTTP {})", resp.status)) }
        }
        "run_coding_agent" => {
            let agent_type = input.get("agent_type").and_then(|v| v.as_str()).ok_or("run_coding_agent: missing 'agent_type'")?;
            let task = input.get("task").and_then(|v| v.as_str()).ok_or("run_coding_agent: missing 'task'")?;
            let agent_workdir = input.get("workdir").and_then(|v| v.as_str())
                .unwrap_or_else(|| fields.get("workdir").and_then(|v| v.as_str()).unwrap_or("/workspace"));
            let background = input.get("background").and_then(|v| v.as_bool()).unwrap_or(false);
            let sandbox_url = fields.get("sandbox_url").and_then(|v| v.as_str()).unwrap_or("");
            if sandbox_url.is_empty() {
                return Err("run_coding_agent: sandbox_url is empty".to_string());
            }
            let escaped_task = task.replace('\'', "'\\''");
            let command = match agent_type {
                "claude-code" => format!("cd {agent_workdir} && claude --permission-mode bypassPermissions --print '{escaped_task}'"),
                "codex" => format!("cd {agent_workdir} && codex exec '{escaped_task}'"),
                "pi" => format!("cd {agent_workdir} && pi -p '{escaped_task}'"),
                "opencode" => format!("cd {agent_workdir} && opencode run '{escaped_task}'"),
                _ => return Err(format!("unsupported coding agent type: {agent_type}")),
            };
            let final_cmd = if background {
                format!("nohup bash -c '{command}' > /tmp/coding-agent-{agent_type}.log 2>&1 & echo $!")
            } else {
                command
            };
            // Execute via sandbox bash API
            let url = format!("{sandbox_url}/v1/processes/run");
            let body = json!({ "command": final_cmd, "workdir": agent_workdir });
            let headers = vec![("content-type".to_string(), "application/json".to_string())];
            let resp = ctx.http_call("POST", &url, &headers, &serde_json::to_string(&body).unwrap_or_default())?;
            if resp.status >= 200 && resp.status < 300 {
                let parsed: Value = serde_json::from_str(&resp.body).unwrap_or(json!({}));
                let stdout = parsed.get("stdout").and_then(|v| v.as_str()).unwrap_or("");
                let stderr = parsed.get("stderr").and_then(|v| v.as_str()).unwrap_or("");
                let exit_code = parsed
                    .get("exit_code")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(-1);
                if exit_code != 0 && !stderr.is_empty() {
                    Ok(format!(
                        "Command: {final_cmd}\nExit code: {exit_code}\nstdout: {stdout}\nstderr: {stderr}"
                    ))
                } else {
                    Ok(format!("Command: {final_cmd}\n{stdout}"))
                }
            } else {
                Err(format!("sandbox process failed (HTTP {})", resp.status))
            }
        }
        _ => Err(format!("unknown entity tool: {tool_name}")),
    }
}

fn odata_headers(tenant: &str) -> Vec<(String, String)> {
    vec![
        ("x-tenant-id".to_string(), tenant.to_string()),
        ("x-temper-principal-kind".to_string(), "admin".to_string()),
        ("content-type".to_string(), "application/json".to_string()),
        ("accept".to_string(), "application/json".to_string()),
    ]
}

fn normalize_field_key(value: &str) -> String {
    value
        .chars()
        .filter(|ch| ch.is_alphanumeric())
        .flat_map(|ch| ch.to_lowercase())
        .collect()
}

fn direct_field_value<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    let object = value.as_object()?;
    for key in keys {
        if let Some(found) = object.get(*key) {
            return Some(found);
        }
    }
    let normalized_keys = keys
        .iter()
        .map(|key| normalize_field_key(key))
        .collect::<Vec<_>>();
    object.iter().find_map(|(key, value)| {
        let normalized_key = normalize_field_key(key);
        normalized_keys
            .iter()
            .any(|candidate| candidate == &normalized_key)
            .then_some(value)
    })
}

fn direct_field_str<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    direct_field_value(value, keys).and_then(Value::as_str)
}

fn entity_field_str<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    direct_field_value(value, &["fields"])
        .and_then(|fields| direct_field_str(fields, keys))
        .or_else(|| direct_field_str(value, keys))
}

fn agent_entity_id<'a>(agent: &'a Value) -> &'a str {
    entity_field_str(agent, &["Id", "entity_id", "id"]).unwrap_or("")
}

fn agent_display_id<'a>(agent: &'a Value) -> &'a str {
    entity_field_str(agent, &["TemperAgentId", "Id", "entity_id", "id"]).unwrap_or("?")
}

fn list_temper_agents(
    ctx: &Context,
    temper_api_url: &str,
    tenant: &str,
) -> Result<Vec<Value>, String> {
    let url = format!("{temper_api_url}/tdata/TemperAgents");
    let resp = ctx.http_call("GET", &url, &odata_headers(tenant), "")?;
    if resp.status != 200 {
        return Err(format!("temper agent listing failed (HTTP {})", resp.status));
    }
    let parsed: Value = serde_json::from_str(&resp.body).unwrap_or_else(|_| json!({}));
    Ok(parsed
        .get("value")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default())
}

fn resolve_agent_reference(
    ctx: &Context,
    temper_api_url: &str,
    tenant: &str,
    agent_reference: &str,
) -> Result<Option<Value>, String> {
    let agents = list_temper_agents(ctx, temper_api_url, tenant)?;
    Ok(agents.into_iter().find(|agent| {
        let entity_id = agent_entity_id(agent);
        let temper_agent_id = entity_field_str(agent, &["TemperAgentId"]).unwrap_or("");
        entity_id == agent_reference || temper_agent_id == agent_reference
    }))
}

/// Read session JSONL from TemperFS.
fn read_session_from_temperfs(
    ctx: &Context,
    temper_api_url: &str,
    tenant: &str,
    file_id: &str,
) -> Result<String, String> {
    let url = format!("{temper_api_url}/tdata/Files('{file_id}')/$value");
    let headers = vec![
        ("x-tenant-id".to_string(), tenant.to_string()),
        ("x-temper-principal-kind".to_string(), "admin".to_string()),
    ];
    let resp = ctx.http_call("GET", &url, &headers, "")?;
    if resp.status == 200 { Ok(resp.body) }
    else if resp.status == 404 { Ok(String::new()) }
    else { Err(format!("TemperFS session read failed (HTTP {})", resp.status)) }
}

/// Write session JSONL to TemperFS.
fn write_session_to_temperfs(
    ctx: &Context,
    temper_api_url: &str,
    tenant: &str,
    file_id: &str,
    jsonl: &str,
) -> Result<(), String> {
    let url = format!("{temper_api_url}/tdata/Files('{file_id}')/$value");
    let headers = vec![
        ("content-type".to_string(), "text/plain".to_string()),
        ("x-tenant-id".to_string(), tenant.to_string()),
        ("x-temper-principal-kind".to_string(), "admin".to_string()),
    ];
    let resp = ctx.http_call("PUT", &url, &headers, jsonl)?;
    if resp.status >= 200 && resp.status < 300 { Ok(()) }
    else { Err(format!("TemperFS session write failed (HTTP {})", resp.status)) }
}

fn resolve_temper_api_url(ctx: &Context, fields: &Value) -> String {
    fields
        .get("temper_api_url")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| {
            ctx.config
                .get("temper_api_url")
                .filter(|s| !s.is_empty())
                .cloned()
        })
        .unwrap_or_else(|| "http://127.0.0.1:3000".to_string())
}
