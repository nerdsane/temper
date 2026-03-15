//! Codex local CLI adapter.

use std::time::Instant;

use async_trait::async_trait;
use tokio::process::Command;

use super::{AdapterContext, AdapterError, AdapterResult, AgentAdapter};

/// Adapter implementation for local `codex` CLI execution.
#[derive(Debug, Default)]
pub struct CodexAdapter;

#[async_trait]
impl AgentAdapter for CodexAdapter {
    fn adapter_type(&self) -> &str {
        "codex"
    }

    async fn execute(&self, ctx: AdapterContext) -> Result<AdapterResult, AdapterError> {
        let started = Instant::now();

        let command_name = ctx
            .integration_config
            .get("command")
            .map(String::as_str)
            .unwrap_or("codex");

        let mut command = Command::new(command_name);
        // determinism-ok: process spawn for agent execution

        if let Some(workdir) = ctx.integration_config.get("workdir")
            && !workdir.trim().is_empty()
        {
            command.current_dir(workdir);
        }

        if let Some(codex_home) = ctx.integration_config.get("codex_home")
            && !codex_home.trim().is_empty()
        {
            command.env("CODEX_HOME", codex_home);
        }

        command
            .env(
                "TEMPER_AGENT_ID",
                ctx.agent_ctx.agent_id.clone().unwrap_or_default(),
            )
            .env("TEMPER_RUN_ID", ctx.entity_id.clone())
            .env("TEMPER_TASK_ID", ctx.entity_id.clone())
            .env("TEMPER_WAKE_REASON", ctx.trigger_action.clone());

        let checkpoint = checkpoint_from_state(&ctx);
        if let Some(session_id) = checkpoint.as_deref() {
            command.arg("resume").arg(session_id).arg("-");
        } else {
            command.arg("exec").arg("--json");
            if let Some(prompt) = ctx.integration_config.get("prompt")
                && !prompt.trim().is_empty()
            {
                command.arg(prompt);
            }
        }

        if let Some(extra_args) = ctx.integration_config.get("args") {
            for arg in extra_args.split_whitespace() {
                command.arg(arg);
            }
        }

        let output = command.output().await.map_err(|e| {
            AdapterError::Invocation(format!("failed to spawn '{command_name}': {e}"))
        })?;

        let duration_ms = started.elapsed().as_millis() as u64;
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        if output.status.success() {
            Ok(AdapterResult::success(
                parse_codex_json(&stdout),
                duration_ms,
            ))
        } else {
            let detail = if stderr.trim().is_empty() {
                stdout.trim().to_string()
            } else {
                stderr.trim().to_string()
            };
            Ok(AdapterResult::failure(detail, duration_ms))
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

fn parse_codex_json(stdout: &str) -> serde_json::Value {
    for line in stdout.lines().rev() {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(line) {
            return value;
        }
    }
    serde_json::json!({ "raw_output": stdout.trim() })
}
