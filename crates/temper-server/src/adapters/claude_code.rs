//! Claude Code local CLI adapter.

use std::time::Instant;

use async_trait::async_trait;
use tokio::process::Command;

use super::{AdapterContext, AdapterError, AdapterResult, AgentAdapter};

/// Adapter implementation for local `claude` CLI execution.
#[derive(Debug, Default)]
pub struct ClaudeCodeAdapter;

#[async_trait]
impl AgentAdapter for ClaudeCodeAdapter {
    fn adapter_type(&self) -> &str {
        "claude_code"
    }

    async fn execute(&self, ctx: AdapterContext) -> Result<AdapterResult, AdapterError> {
        let checkpoint = checkpoint_from_state(&ctx);
        let run = run_claude(&ctx, checkpoint.as_deref()).await;

        match run {
            Ok(result) => Ok(result),
            Err(e) => {
                // Retry once without resume when the checkpoint/session is stale.
                if checkpoint.is_some()
                    && e.to_string()
                        .to_ascii_lowercase()
                        .contains("unknown session")
                {
                    run_claude(&ctx, None).await
                } else {
                    Err(e)
                }
            }
        }
    }
}

fn checkpoint_from_state(ctx: &AdapterContext) -> Option<String> {
    ctx.entity_state
        .get("fields")
        .and_then(|v| v.get("checkpoint"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

async fn run_claude(
    ctx: &AdapterContext,
    resume: Option<&str>,
) -> Result<AdapterResult, AdapterError> {
    let started = Instant::now(); // determinism-ok: wall-clock timing for external process

    let command_name = ctx
        .integration_config
        .get("command")
        .map(String::as_str)
        .unwrap_or("claude");

    let mut command = Command::new(command_name);
    // determinism-ok: process spawn for agent execution
    command
        .arg("--print")
        .arg("--output-format")
        .arg("stream-json")
        .arg("--verbose");

    if let Some(session) = resume {
        command.arg("--resume").arg(session);
    }

    if let Some(skills_path) = ctx.integration_config.get("skills_path")
        && !skills_path.trim().is_empty()
    {
        command.arg("--add-dir").arg(skills_path);
    }

    if let Some(extra_args) = ctx.integration_config.get("args") {
        for arg in extra_args.split_whitespace() {
            command.arg(arg);
        }
    }

    if let Some(workdir) = ctx.integration_config.get("workdir")
        && !workdir.trim().is_empty()
    {
        command.current_dir(workdir);
    }

    // Pass platform-minted credential for identity resolution (ADR-0033).
    // The spawned agent uses this token to authenticate back to Temper,
    // and the platform resolves it to a verified identity.
    if let Some(ref api_key) = ctx.agent_ctx.agent_api_key {
        command.env("TEMPER_API_KEY", api_key);
    }
    command
        .env("TEMPER_RUN_ID", ctx.entity_id.clone())
        .env("TEMPER_TASK_ID", ctx.entity_id.clone())
        .env("TEMPER_WAKE_REASON", ctx.trigger_action.clone());

    if let Some(prompt) = ctx.integration_config.get("prompt")
        && !prompt.trim().is_empty()
    {
        command.arg(prompt);
    }

    let output = command
        .output()
        .await
        .map_err(|e| AdapterError::Invocation(format!("failed to spawn '{command_name}': {e}")))?;

    let duration_ms = started.elapsed().as_millis() as u64;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if output.status.success() {
        let callback_params = parse_stream_json_output(&stdout);
        Ok(AdapterResult::success(callback_params, duration_ms))
    } else {
        let detail = if stderr.trim().is_empty() {
            stdout.trim().to_string()
        } else {
            stderr.trim().to_string()
        };
        Ok(AdapterResult::failure(detail, duration_ms))
    }
}

fn parse_stream_json_output(stdout: &str) -> serde_json::Value {
    let mut merged = serde_json::Map::new();
    let mut last_json: Option<serde_json::Value> = None;

    for line in stdout.lines() {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(line) {
            if let Some(obj) = value.as_object() {
                for (k, v) in obj {
                    merged.insert(k.clone(), v.clone());
                }
            }
            last_json = Some(value);
        }
    }

    let mut out = serde_json::json!({
        "raw_output": stdout.trim(),
    });

    if let Some(obj) = out.as_object_mut() {
        if !merged.is_empty() {
            obj.insert("stream".to_string(), serde_json::Value::Object(merged));
        }
        if let Some(last) = last_json {
            obj.insert("result".to_string(), last);
        }
    }

    out
}
