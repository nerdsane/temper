//! Coding Agent Runner — WASM module for spawning coding agent CLI processes.
//!
//! Maps agent_type to CLI commands and executes them in the sandbox.
//! Supports claude-code, codex, pi, and opencode.

use temper_wasm_sdk::prelude::*;

#[unsafe(no_mangle)]
pub extern "C" fn run(_ctx_ptr: i32, _ctx_len: i32) -> i32 {
    let result = (|| -> Result<(), String> {
        let ctx = Context::from_host()?;
        ctx.log("info", "coding_agent_runner: starting");

        let fields = ctx.entity_state.get("fields").cloned().unwrap_or(json!({}));
        let sandbox_url = fields.get("sandbox_url").and_then(|v| v.as_str()).unwrap_or("");
        let workdir = fields.get("workdir").and_then(|v| v.as_str()).unwrap_or("/workspace");

        if sandbox_url.is_empty() {
            return Err("coding_agent_runner: sandbox_url is empty".to_string());
        }

        // Read tool input from trigger params
        let input = ctx.trigger_params.get("input").cloned().unwrap_or(json!({}));
        let agent_type = input.get("agent_type").and_then(|v| v.as_str()).unwrap_or("claude-code");
        let task = input.get("task").and_then(|v| v.as_str()).unwrap_or("");
        let task_workdir = input.get("workdir").and_then(|v| v.as_str()).unwrap_or(workdir);

        if task.is_empty() {
            return Err("coding_agent_runner: task is empty".to_string());
        }

        // Map agent_type to CLI command
        let command = match agent_type {
            "claude-code" => format!("claude --permission-mode bypassPermissions --print '{}'", escape_single_quotes(task)),
            "codex" => format!("codex exec '{}'", escape_single_quotes(task)),
            "pi" => format!("pi -p '{}'", escape_single_quotes(task)),
            "opencode" => format!("opencode run '{}'", escape_single_quotes(task)),
            other => return Err(format!("coding_agent_runner: unsupported agent_type: {other}")),
        };

        ctx.log("info", &format!("coding_agent_runner: running {agent_type}: {}", &command[..command.len().min(100)]));

        // Execute via sandbox bash API
        let url = format!("{sandbox_url}/v1/processes/run");
        let body = serde_json::to_string(&json!({
            "command": command,
            "workdir": task_workdir,
        })).unwrap_or_default();

        let headers = vec![("content-type".to_string(), "application/json".to_string())];
        let resp = ctx.http_call("POST", &url, &headers, &body)?;

        let output = if resp.status >= 200 && resp.status < 300 {
            if let Ok(parsed) = serde_json::from_str::<Value>(&resp.body) {
                let stdout = parsed.get("stdout").and_then(|v| v.as_str()).unwrap_or("");
                let stderr = parsed.get("stderr").and_then(|v| v.as_str()).unwrap_or("");
                let exit_code = parsed.get("exit_code").and_then(|v| v.as_i64()).unwrap_or(-1);
                let mut out = String::new();
                if !stdout.is_empty() { out.push_str(stdout); }
                if !stderr.is_empty() {
                    if !out.is_empty() { out.push('\n'); }
                    out.push_str("STDERR: ");
                    out.push_str(stderr);
                }
                if exit_code != 0 {
                    out.push_str(&format!("\n(exit code: {exit_code})"));
                }
                out
            } else {
                resp.body
            }
        } else {
            format!("Error (HTTP {}): {}", resp.status, &resp.body[..resp.body.len().min(500)])
        };

        // Return the output as a tool result
        set_success_result("HandleToolResults", &json!({
            "pending_tool_calls": json!([{
                "type": "tool_result",
                "tool_use_id": input.get("tool_use_id").and_then(|v| v.as_str()).unwrap_or("unknown"),
                "content": output,
            }]).to_string(),
        }));

        Ok(())
    })();

    if let Err(e) = result {
        set_error_result(&e);
    }
    0
}

fn escape_single_quotes(s: &str) -> String {
    s.replace('\'', "'\\''")
}
