//! Dispatch layer for `tools.*` method calls from the sandbox.
//!
//! `tools.*` → Cedar `/api/authorize` check on server → execute on agent's machine.
//!
//! `temper.*` dispatch is handled by `temper_sandbox::dispatch`.

use monty::MontyObject;
use serde_json::Value;

use temper_sandbox::helpers::expect_string_arg;

/// Dispatch a `tools.<method>()` call — Cedar-gated local execution.
pub(crate) async fn dispatch_tools_method(
    http: &reqwest::Client,
    server_url: &str,
    tenant: &str,
    principal_id: &std::sync::Arc<std::sync::Mutex<Option<String>>>,
    method: &str,
    args: &[MontyObject],
) -> Result<Value, String> {
    // Dataclass method calls include self as first arg.
    let args = if args.is_empty() { args } else { &args[1..] };

    // Determine Cedar resource info for authorization.
    let (action, resource_type, resource_id) = match method {
        "bash" => {
            let command = expect_string_arg(args, 0, "command", method)?;
            ("execute", "Shell", command)
        }
        "read" => {
            let path = expect_string_arg(args, 0, "path", method)?;
            ("read", "FileSystem", path)
        }
        "write" => {
            let path = expect_string_arg(args, 0, "path", method)?;
            ("write", "FileSystem", path)
        }
        "ls" => {
            let path = expect_string_arg(args, 0, "path", method)?;
            ("list", "FileSystem", path)
        }
        _ => {
            return Err(format!(
                "unknown tools method '{method}'. Available: bash, read, write, ls"
            ));
        }
    };

    // Cedar authorization check via server.
    authorize_tool(http, server_url, tenant, principal_id, action, resource_type, &resource_id).await?;

    // Execute locally.
    match method {
        "bash" => {
            let command = expect_string_arg(args, 0, "command", method)?;
            let output = tokio::process::Command::new("sh")
                .arg("-c")
                .arg(&command)
                .output()
                .await
                .map_err(|e| format!("failed to execute command: {e}"))?;

            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let mut result = stdout.to_string();
            if !stderr.is_empty() {
                result.push_str("\n[stderr] ");
                result.push_str(&stderr);
            }
            Ok(Value::String(result))
        }
        "read" => {
            let path = expect_string_arg(args, 0, "path", method)?;
            let content = tokio::fs::read_to_string(&path)
                .await
                .map_err(|e| format!("failed to read '{path}': {e}"))?;
            Ok(Value::String(content))
        }
        "write" => {
            let path = expect_string_arg(args, 0, "path", method)?;
            let content = expect_string_arg(args, 1, "content", method)?;
            if let Some(parent) = std::path::Path::new(&path).parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| format!("failed to create parent dirs: {e}"))?;
            }
            tokio::fs::write(&path, &content)
                .await
                .map_err(|e| format!("failed to write '{path}': {e}"))?;
            Ok(serde_json::json!({ "written": path, "bytes": content.len() }))
        }
        "ls" => {
            let path = expect_string_arg(args, 0, "path", method)?;
            let mut entries = Vec::new();
            let mut dir = tokio::fs::read_dir(&path)
                .await
                .map_err(|e| format!("failed to list '{path}': {e}"))?;
            while let Some(entry) = dir
                .next_entry()
                .await
                .map_err(|e| format!("failed to read dir entry: {e}"))?
            {
                entries.push(Value::String(
                    entry.file_name().to_string_lossy().to_string(),
                ));
            }
            Ok(Value::Array(entries))
        }
        _ => unreachable!(),
    }
}

/// Check Cedar authorization for a `tools.*` call via the server.
///
/// If denied, creates a PendingDecision and polls until the human
/// approves or rejects it (or timeout). This blocks the sandbox
/// execution until governance is resolved.
async fn authorize_tool(
    http: &reqwest::Client,
    server_url: &str,
    tenant: &str,
    principal_id: &std::sync::Arc<std::sync::Mutex<Option<String>>>,
    action: &str,
    resource_type: &str,
    resource_id: &str,
) -> Result<(), String> {
    let agent_id = {
        let g = principal_id.lock().unwrap(); // ci-ok: infallible lock
        g.as_deref().unwrap_or("agent").to_string()
    };
    let url = format!("{server_url}/api/authorize");
    let payload = serde_json::json!({
        "agent_id": agent_id,
        "action": action,
        "resource_type": resource_type,
        "resource_id": resource_id,
    });

    let response = http
        .post(&url)
        .header("X-Tenant-Id", tenant)
        .json(&payload)
        .send()
        .await
        .map_err(|e| format!("Cedar authorization check failed: {e}"))?;

    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|e| format!("failed to read authorization response: {e}"))?;

    if !status.is_success() {
        return Err(format!(
            "Cedar authorization failed (HTTP {status}): {text}"
        ));
    }

    let body: Value = serde_json::from_str(&text).unwrap_or_default();
    if body.get("allowed").and_then(Value::as_bool) == Some(true) {
        return Ok(());
    }

    // Denied — poll for human approval.
    let decision_id = body
        .get("decision_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    if decision_id.is_empty() {
        return Err(format!(
            "tools.{action} on '{resource_id}' denied by Cedar policy (no decision ID)"
        ));
    }

    eprintln!(
        "  [governance] tools.{action}(\"{resource_id}\") needs approval: {decision_id}"
    );
    eprintln!("  [governance] Waiting for human decision via `temper decide` or Observe UI...");

    let poll_url = format!("{server_url}/api/tenants/{tenant}/decisions?status=all");
    let start = std::time::Instant::now(); // determinism-ok: CLI timeout
    let timeout = std::time::Duration::from_secs(300);

    loop {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        if start.elapsed() > timeout {
            return Err(format!(
                "tools.{action} on '{resource_id}' denied — approval timed out after 5 min. \
                 Decision: {decision_id}"
            ));
        }

        let poll_resp = http
            .get(&poll_url)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| format!("failed to poll decisions: {e}"))?;

        let poll_text = poll_resp
            .text()
            .await
            .map_err(|e| format!("failed to read poll response: {e}"))?;

        let poll_body: Value = serde_json::from_str(&poll_text).unwrap_or_default();
        let decisions = poll_body
            .get("decisions")
            .and_then(Value::as_array)
            .or_else(|| poll_body.as_array())
            .cloned()
            .unwrap_or_default();

        for d in &decisions {
            if d.get("id").and_then(Value::as_str) == Some(&decision_id) {
                let status = d
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("");

                match status {
                    "Approved" | "approved" => {
                        eprintln!("  [governance] Approved! Proceeding.");
                        return Ok(());
                    }
                    "Denied" | "denied" | "Rejected" | "rejected" => {
                        return Err(format!(
                            "tools.{action} on '{resource_id}' denied by human. Decision: {decision_id}"
                        ));
                    }
                    _ => {} // Still pending, keep polling.
                }
            }
        }
    }
}
