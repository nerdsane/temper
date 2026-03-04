//! Dispatch layer for `temper.*` and `tools.*` method calls from the sandbox.
//!
//! - `temper.*` → HTTP to Temper server (entity CRUD, governance, specs)
//! - `tools.*` → Cedar `/api/authorize` check on server → execute on agent's machine

use monty::MontyObject;
use reqwest::Method;
use serde_json::Value;

use super::AgentSandbox;
use super::helpers::{
    escape_odata_key, expect_json_object_arg, expect_string_arg, format_authz_denied,
    format_http_error, optional_string_arg,
};

impl AgentSandbox {
    /// Dispatch a `temper.<method>()` call via HTTP to the Temper server.
    pub(crate) async fn dispatch_temper_method(
        &self,
        method: &str,
        args: &[MontyObject],
        kwargs: &[(MontyObject, MontyObject)],
    ) -> Result<Value, String> {
        if !kwargs.is_empty() {
            return Err(format!(
                "temper.{method} does not support keyword arguments"
            ));
        }

        // Dataclass method calls include self as first arg.
        let args = if args.is_empty() { args } else { &args[1..] };

        match method {
            "list" => {
                let entity_type = expect_string_arg(args, 0, "entity_type", method)?;
                self.temper_request(Method::GET, format!("/tdata/{entity_type}"), None)
                    .await
                    .map(|body| body.get("value").cloned().unwrap_or(body))
            }
            "get" => {
                let entity_type = expect_string_arg(args, 0, "entity_type", method)?;
                let entity_id = expect_string_arg(args, 1, "entity_id", method)?;
                let key = escape_odata_key(&entity_id);
                self.temper_request(Method::GET, format!("/tdata/{entity_type}('{key}')"), None)
                    .await
            }
            "create" => {
                let entity_type = expect_string_arg(args, 0, "entity_type", method)?;
                let fields = expect_json_object_arg(args, 1, "fields", method)?;
                self.temper_request(
                    Method::POST,
                    format!("/tdata/{entity_type}"),
                    Some(Value::Object(fields)),
                )
                .await
            }
            "action" => {
                let entity_type = expect_string_arg(args, 0, "entity_type", method)?;
                let entity_id = expect_string_arg(args, 1, "entity_id", method)?;
                let action_name = expect_string_arg(args, 2, "action_name", method)?;
                let body = expect_json_object_arg(args, 3, "body", method)?;
                let key = escape_odata_key(&entity_id);
                self.temper_request(
                    Method::POST,
                    format!("/tdata/{entity_type}('{key}')/Temper.{action_name}"),
                    Some(Value::Object(body)),
                )
                .await
            }
            "submit_specs" => {
                let specs = expect_json_object_arg(args, 0, "specs", method)?;
                let payload = serde_json::json!({ "tenant": self.tenant, "specs": specs });
                self.temper_request(
                    Method::POST,
                    "/api/specs/load-inline".to_string(),
                    Some(payload),
                )
                .await
            }
            "get_decisions" => {
                let status = optional_string_arg(args, 0);
                let path = match status {
                    Some(s) => format!("/api/tenants/{}/decisions?status={s}", self.tenant),
                    None => format!("/api/tenants/{}/decisions", self.tenant),
                };
                self.temper_governance_request(Method::GET, path, None)
                    .await
            }
            "poll_decision" => {
                let decision_id = expect_string_arg(args, 0, "decision_id", method)?;
                let start = std::time::Instant::now(); // determinism-ok: wall-clock for agent timeout only
                loop {
                    let result = self
                        .temper_governance_request(
                            Method::GET,
                            format!("/api/tenants/{}/decisions", self.tenant),
                            None,
                        )
                        .await?;
                    if let Some(decisions) = result.get("decisions").and_then(Value::as_array) {
                        for d in decisions {
                            if d.get("id").and_then(Value::as_str) == Some(&decision_id) {
                                let status = d.get("status").and_then(Value::as_str).unwrap_or("");
                                if !status.eq_ignore_ascii_case("pending") {
                                    return Ok(d.clone());
                                }
                            }
                        }
                    }
                    if start.elapsed() > std::time::Duration::from_secs(120) {
                        return Err(format!(
                            "poll_decision timed out after 120s: decision {decision_id} still pending. \
                             Ask the human to approve via `temper decide` CLI, then retry."
                        ));
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(2000)).await;
                }
            }
            _ => Err(format!(
                "unknown temper method '{method}'. Available: \
                 list, get, create, action, \
                 submit_specs, get_decisions, poll_decision"
            )),
        }
    }

    /// Dispatch a `tools.<method>()` call — Cedar-gated local execution.
    pub(crate) async fn dispatch_tools_method(
        &self,
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
        self.authorize_tool(action, resource_type, &resource_id)
            .await?;

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

    // ── HTTP helpers ────────────────────────────────────────────────────

    /// Send a request to the Temper server with agent principal headers.
    async fn temper_request(
        &self,
        method: Method,
        path: String,
        body: Option<Value>,
    ) -> Result<Value, String> {
        let url = format!("{}{path}", self.server_url);
        let mut request = self
            .http
            .request(method, &url)
            .header("X-Tenant-Id", &self.tenant)
            .header("Accept", "application/json");

        if let Some(ref pid) = self.principal_id {
            request = request
                .header("X-Temper-Principal-Kind", "agent")
                .header("X-Temper-Principal-Id", pid.as_str());
        }

        if let Some(ref payload) = body {
            request = request.json(payload);
        }

        let response = request
            .send()
            .await
            .map_err(|e| format!("failed to call Temper at {url}: {e}"))?;

        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|e| format!("failed to read response body: {e}"))?;

        if status.is_success() {
            if text.trim().is_empty() {
                return Ok(Value::Null);
            }
            return serde_json::from_str(&text).or(Ok(Value::String(text)));
        }

        if status == reqwest::StatusCode::FORBIDDEN
            && let Some(structured) = format_authz_denied(&text)
        {
            return Ok(structured);
        }

        Err(format_http_error(status, &text))
    }

    /// Send a governance request (admin principal).
    async fn temper_governance_request(
        &self,
        method: Method,
        path: String,
        body: Option<Value>,
    ) -> Result<Value, String> {
        let url = format!("{}{path}", self.server_url);
        let admin_id = self
            .principal_id
            .as_deref()
            .unwrap_or("agent-governance-admin");
        let mut request = self
            .http
            .request(method, &url)
            .header("X-Tenant-Id", &self.tenant)
            .header("Accept", "application/json")
            .header("X-Temper-Principal-Kind", "admin")
            .header("X-Temper-Principal-Id", admin_id);

        if let Some(ref payload) = body {
            request = request.json(payload);
        }

        let response = request
            .send()
            .await
            .map_err(|e| format!("failed to call Temper at {url}: {e}"))?;

        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|e| format!("failed to read response body: {e}"))?;

        if status.is_success() {
            if text.trim().is_empty() {
                return Ok(Value::Null);
            }
            return serde_json::from_str(&text).or(Ok(Value::String(text)));
        }

        Err(format_http_error(status, &text))
    }

    /// Check Cedar authorization for a `tools.*` call via the server.
    async fn authorize_tool(
        &self,
        action: &str,
        resource_type: &str,
        resource_id: &str,
    ) -> Result<(), String> {
        let principal = self.principal_id.as_deref().unwrap_or("agent");
        let url = format!("{}/api/authorize", self.server_url);
        let payload = serde_json::json!({
            "principal": principal,
            "action": action,
            "resource_type": resource_type,
            "resource_id": resource_id,
        });

        let response = self
            .http
            .post(&url)
            .header("X-Tenant-Id", &self.tenant)
            .json(&payload)
            .send()
            .await
            .map_err(|e| format!("Cedar authorization check failed: {e}"))?;

        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|e| format!("failed to read authorization response: {e}"))?;

        if status.is_success() {
            let body: Value = serde_json::from_str(&text).unwrap_or_default();
            if body.get("allowed").and_then(Value::as_bool) == Some(true) {
                return Ok(());
            }
            let decision_id = body
                .get("decision_id")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            return Err(format!(
                "tools.{action} on '{resource_id}' denied by Cedar policy. Decision: {decision_id}"
            ));
        }

        // Treat non-success as authorization failure.
        Err(format!(
            "Cedar authorization failed (HTTP {status}): {text}"
        ))
    }
}
