//! Dispatch layer for `tools.*` method calls from the sandbox.
//!
//! `tools.*` → Cedar `/api/authorize` check on server → execute on agent's machine.
//!
//! `temper.*` dispatch is handled by `temper_sandbox::dispatch`.

use monty::MontyObject;
use serde_json::Value;

use temper_sandbox::helpers::expect_string_arg;

/// Governance approval timeout (5 minutes).
const GOVERNANCE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// Typed governance event — replaces stringly-typed `Fn(&str, &str)`.
#[derive(Debug, Clone)]
pub enum GovernanceEvent {
    /// Cedar policy allowed the action.
    Allowed { action: String, resource_id: String },
    /// Action denied — waiting for human approval.
    Waiting {
        decision_id: String,
        action: String,
        resource_id: String,
    },
    /// Decision resolved (approved or denied).
    Resolved { decision_id: String, approved: bool },
}

/// Callback type for typed governance events from the sandbox dispatch layer.
pub type GovernanceCallback = std::sync::Arc<dyn Fn(GovernanceEvent) + Send + Sync>;

/// Data sent to the governance resolver when a tool call is denied.
pub struct GovernancePrompt {
    /// Server-assigned pending-decision ID (e.g. `PD-019cbb53-...`).
    pub decision_id: String,
    /// Cedar action name (e.g. `execute`, `read`, `write`).
    pub action: String,
    /// Cedar resource type (e.g. `Shell`, `FileSystem`).
    pub resource_type: String,
    /// Cedar resource identifier (e.g. `/usr/bin/ls`, `/tmp/foo.txt`).
    pub resource_id: String,
}

/// Approval scope for a governance decision.
#[derive(Debug, Clone, Copy)]
pub enum GovernanceScope {
    /// This exact agent + action + resource.
    Narrow,
    /// This agent + action on any resource of this type.
    Medium,
    /// This agent + any action on any resource of this type.
    Broad,
}

impl GovernanceScope {
    /// Serialize to the string the server API expects.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Narrow => "narrow",
            Self::Medium => "medium",
            Self::Broad => "broad",
        }
    }
}

/// User's inline governance decision.
pub enum GovernanceDecision {
    /// Approve with a scope.
    Approve { scope: GovernanceScope },
    /// Deny the request.
    Deny,
    /// Fall back to external approval only (Observe UI / `temper decide`).
    Wait,
}

/// Resolver function called to prompt the user for an inline governance decision.
///
/// Runs on a blocking thread (`spawn_blocking`) so it can do synchronous I/O.
pub type GovernanceResolverFn =
    std::sync::Arc<dyn Fn(GovernancePrompt) -> GovernanceDecision + Send + Sync>;

/// Bundles governance callback + resolver to reduce parameter sprawl.
///
/// Passed as `Option<&GovernanceContext>` through the dispatch chain.
#[derive(Clone)]
pub struct GovernanceContext {
    /// Callback for governance events (allowed, waiting, resolved).
    pub on_event: GovernanceCallback,
    /// Optional inline resolver for user prompting.
    pub resolver: Option<GovernanceResolverFn>,
}

/// Dispatch a `tools.<method>()` call — Cedar-gated local execution.
pub(crate) async fn dispatch_tools_method(
    http: &reqwest::Client,
    server_url: &str,
    tenant: &str,
    principal_id: &std::sync::Arc<std::sync::Mutex<Option<String>>>,
    method: &str,
    args: &[MontyObject],
    governance: Option<&GovernanceContext>,
) -> Result<Value, String> {
    // Dataclass method calls include self as first arg.
    let args = if args.is_empty() { args } else { &args[1..] };

    // Determine Cedar resource info for authorization.
    let (action, resource_type, resource_id) = match method {
        "bash" => {
            let command = expect_string_arg(args, 0, "command", method)?;
            if let Some(reason) = is_network_command(&command) {
                return Err(format!("{reason}\n\n{INTEGRATION_GUIDANCE}"));
            }
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
    authorize_tool(
        http,
        server_url,
        tenant,
        principal_id,
        action,
        resource_type,
        &resource_id,
        governance,
    )
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

/// Check Cedar authorization for a `tools.*` call via the server.
///
/// If denied, creates a PendingDecision and races an inline user prompt
/// (via resolver) against external approval polling. Falls back to
/// polling-only when no resolver is provided.
#[allow(clippy::too_many_arguments)]
async fn authorize_tool(
    http: &reqwest::Client,
    server_url: &str,
    tenant: &str,
    principal_id: &std::sync::Arc<std::sync::Mutex<Option<String>>>,
    action: &str,
    resource_type: &str,
    resource_id: &str,
    governance: Option<&GovernanceContext>,
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

    let resp_status = response.status();
    let text = response
        .text()
        .await
        .map_err(|e| format!("failed to read authorization response: {e}"))?;

    if !resp_status.is_success() {
        return Err(format!(
            "Cedar authorization failed (HTTP {resp_status}): {text}"
        ));
    }

    let body: Value = serde_json::from_str(&text).unwrap_or_default();
    if body.get("allowed").and_then(Value::as_bool) == Some(true) {
        // Fire allowed notification for action history.
        if let Some(ctx) = governance {
            (ctx.on_event)(GovernanceEvent::Allowed {
                action: format!("tools.{action}"),
                resource_id: resource_id.to_string(),
            });
        }
        return Ok(());
    }

    // Denied — get decision ID for approval flow.
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

    // Fire governance notification (spinner / display).
    if let Some(ctx) = governance {
        (ctx.on_event)(GovernanceEvent::Waiting {
            decision_id: decision_id.clone(),
            action: format!("tools.{action}"),
            resource_id: resource_id.to_string(),
        });
    } else {
        eprintln!("  [governance] tools.{action}(\"{resource_id}\") needs approval: {decision_id}");
        eprintln!("  [governance] Waiting for human decision via `temper decide` or Observe UI...");
    }

    // If a resolver is available, race inline prompt vs server polling.
    let resolver = governance.and_then(|ctx| ctx.resolver.clone());
    if let Some(resolver) = resolver {
        let prompt = GovernancePrompt {
            decision_id: decision_id.clone(),
            action: action.to_string(),
            resource_type: resource_type.to_string(),
            resource_id: resource_id.to_string(),
        };
        let mut user_task = tokio::task::spawn_blocking(move || resolver(prompt));

        let start = std::time::Instant::now(); // determinism-ok: CLI timeout

        loop {
            tokio::select! {
                biased;
                result = &mut user_task => {
                    match result.unwrap_or(GovernanceDecision::Wait) {
                        GovernanceDecision::Approve { scope } => {
                            submit_decision(http, server_url, tenant, &decision_id, true, Some(&scope)).await?;
                            fire_resolved(governance, &decision_id, true);
                            return Ok(());
                        }
                        GovernanceDecision::Deny => {
                            submit_decision(http, server_url, tenant, &decision_id, false, None).await?;
                            fire_resolved(governance, &decision_id, false);
                            return Err(format!(
                                "tools.{action} on '{resource_id}' denied by user. \
                                 Decision: {decision_id}"
                            ));
                        }
                        GovernanceDecision::Wait => break, // fall to polling-only
                    }
                }
                _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {
                    if start.elapsed() > GOVERNANCE_TIMEOUT {
                        user_task.abort();
                        return Err(format!(
                            "tools.{action} on '{resource_id}' denied — approval timed out \
                             after 5 min. Decision: {decision_id}"
                        ));
                    }
                    // Poll server — if approved externally, cancel prompt and return.
                    if let Some(approved) = check_decision_status(http, server_url, tenant, &decision_id).await? {
                        user_task.abort();
                        fire_resolved(governance, &decision_id, approved);
                        if approved {
                            return Ok(());
                        }
                        return Err(format!(
                            "tools.{action} on '{resource_id}' denied externally. \
                             Decision: {decision_id}"
                        ));
                    }
                }
            }
        }
    }

    // Polling-only loop (no resolver, or user chose Wait).
    poll_for_decision(
        http,
        server_url,
        tenant,
        action,
        resource_id,
        &decision_id,
        governance,
    )
    .await
}

/// Poll the server until the decision is approved/denied or timeout.
async fn poll_for_decision(
    http: &reqwest::Client,
    server_url: &str,
    tenant: &str,
    action: &str,
    resource_id: &str,
    decision_id: &str,
    governance: Option<&GovernanceContext>,
) -> Result<(), String> {
    let start = std::time::Instant::now(); // determinism-ok: CLI timeout

    loop {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        if start.elapsed() > GOVERNANCE_TIMEOUT {
            return Err(format!(
                "tools.{action} on '{resource_id}' denied — approval timed out after 5 min. \
                 Decision: {decision_id}"
            ));
        }

        if let Some(approved) = check_decision_status(http, server_url, tenant, decision_id).await?
        {
            fire_resolved(governance, decision_id, approved);
            if approved {
                return Ok(());
            }
            return Err(format!(
                "tools.{action} on '{resource_id}' denied by human. Decision: {decision_id}"
            ));
        }
    }
}

/// Check the status of a specific decision on the server.
///
/// Returns `Some(true)` if approved, `Some(false)` if denied, `None` if still pending.
async fn check_decision_status(
    http: &reqwest::Client,
    server_url: &str,
    tenant: &str,
    decision_id: &str,
) -> Result<Option<bool>, String> {
    let poll_url = format!("{server_url}/api/tenants/{tenant}/decisions");
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
    let empty = vec![];
    let decisions = poll_body
        .get("decisions")
        .and_then(Value::as_array)
        .or_else(|| poll_body.as_array())
        .unwrap_or(&empty);

    for d in decisions {
        if d.get("id").and_then(Value::as_str) == Some(decision_id) {
            let status = d.get("status").and_then(Value::as_str).unwrap_or("");
            match status {
                "Approved" | "approved" => return Ok(Some(true)),
                "Denied" | "denied" | "Rejected" | "rejected" => return Ok(Some(false)),
                _ => {}
            }
        }
    }

    Ok(None)
}

/// Submit a governance decision (approve or deny) via the server API.
///
/// Uses `POST /api/tenants/{tenant}/decisions/{id}/approve` or `.../deny`,
/// matching the server route definitions in `temper-server/src/api.rs`.
async fn submit_decision(
    http: &reqwest::Client,
    server_url: &str,
    tenant: &str,
    decision_id: &str,
    approved: bool,
    scope: Option<&GovernanceScope>,
) -> Result<(), String> {
    let verb = if approved { "approve" } else { "deny" };
    let url = format!("{server_url}/api/tenants/{tenant}/decisions/{decision_id}/{verb}");

    let mut payload = serde_json::json!({ "decided_by": "agent-inline" });
    if let Some(s) = scope {
        payload["scope"] = serde_json::Value::String(s.as_str().to_string());
    }

    http.post(&url)
        .json(&payload)
        .send()
        .await
        .map_err(|e| format!("failed to submit {verb}: {e}"))?;

    Ok(())
}

/// Guidance message returned when a network command is detected.
///
/// Instructs the LLM to use `[[integration]]` sections in IOA specs instead.
pub(crate) const INTEGRATION_GUIDANCE: &str = "\
tools.bash() is for LOCAL operations only (files, compilation, grep, process management, etc.).\n\
All external / network access must go through [[integration]] in your IOA spec.\n\
\n\
Option 1 — built-in http_fetch module:\n\
  [[integration]]\n\
  name = \"fetch_data\"\n\
  trigger = { state = \"Fetching\", action = \"Fetch\" }\n\
  on_success = \"FetchSucceeded\"\n\
  on_failure = \"FetchFailed\"\n\
  module = \"http_fetch\"\n\
  [integration.config]\n\
  url = \"https://api.example.com/data\"\n\
  method = \"GET\"\n\
\n\
Option 2 — custom WASM module:\n\
  module = \"my_module\"\n\
  ... then compile via temper.compile_wasm(\"my_module\", \"rust source\")\n\
\n\
Steps: 1) Add [[integration]] to your spec\n\
       2) Submit via temper.submit_specs()\n\
       3) Create entity and invoke the triggering action";

/// Check whether a shell command attempts network / external access.
///
/// Returns a human-readable reason when the command should use `[[integration]]`
/// instead of `tools.bash()`. Returns `None` for purely local commands.
fn is_network_command(command: &str) -> Option<&'static str> {
    let lower = command.to_lowercase();

    // ── HTTP client programs ────────────────────────────────────────
    for tool in &["curl", "wget", "http"] {
        if has_program(&lower, tool) {
            return Some(
                "HTTP client tool detected. \
                 Use [[integration]] with module = \"http_fetch\" instead.",
            );
        }
    }

    // ── URLs anywhere in the command ────────────────────────────────
    if lower.contains("http://") || lower.contains("https://") {
        return Some(
            "Command contains a URL. \
             External HTTP requests must go through [[integration]].",
        );
    }

    // ── Network tools ───────────────────────────────────────────────
    for tool in &["nc", "ncat", "netcat", "ssh", "scp", "sftp", "ftp"] {
        if has_program(&lower, tool) {
            return Some(
                "Network tool detected (ssh/nc/ftp/scp/sftp). \
                 Use [[integration]] for external access.",
            );
        }
    }

    // ── rsync with remote host (contains `:`) ───────────────────────
    if has_program(&lower, "rsync") && lower.contains(':') {
        return Some(
            "Remote rsync detected. \
             Use [[integration]] for remote file transfers.",
        );
    }

    // ── DNS / ping to external hosts ────────────────────────────────
    for tool in &["dig", "nslookup"] {
        if has_program(&lower, tool) {
            return Some("DNS lookup tools must go through [[integration]].");
        }
    }

    if has_program(&lower, "ping") && !lower.contains("localhost") && !lower.contains("127.0.0.1") {
        return Some("Ping to external hosts must go through [[integration]].");
    }

    // ── Package managers that fetch from remote registries ──────────
    for pattern in &["pip install", "npm install", "cargo install"] {
        if lower.contains(pattern) {
            return Some(
                "Package install commands fetch from remote registries. \
                 Use [[integration]] for external access.",
            );
        }
    }

    // ── Scripting with network libraries ────────────────────────────
    for pattern in &[
        "requests.get",
        "requests.post",
        "requests.put",
        "requests.delete",
        "urllib",
        "fetch(",
        "http.client",
    ] {
        if lower.contains(pattern) {
            return Some(
                "Network library usage detected in script. \
                 Use [[integration]] for external access.",
            );
        }
    }

    None
}

/// Check if `program` appears as a command name in a shell string.
///
/// Handles pipes, semicolons, `&&`, `||`, and subshells by splitting on
/// shell metacharacters and checking each segment's first word.
fn has_program(cmd: &str, program: &str) -> bool {
    for segment in cmd.split(['|', ';', '&', '(', ')', '`', '\n']) {
        let trimmed = segment.trim();
        if trimmed == program {
            return true;
        }
        // Check that program is the first word (followed by whitespace).
        if trimmed.starts_with(program) {
            let next = trimmed.as_bytes().get(program.len()).copied();
            if matches!(next, Some(b' ') | Some(b'\t')) {
                return true;
            }
        }
    }
    false
}

/// Wait for governance approval on a `temper.*` denial.
///
/// Called from `dispatch_method` when a `temper.*` call returns a
/// `{denied: true, pending_decision: "PD-xxx"}` response. Runs the
/// same inline-resolver + polling flow as `authorize_tool`, so the
/// user gets the same interactive prompt for `temper.submit_specs()`
/// as they do for `tools.bash()`.
///
/// Returns `Ok(())` on approval (caller should retry the request),
/// `Err(...)` on denial or timeout.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn resolve_temper_denial(
    http: &reqwest::Client,
    server_url: &str,
    tenant: &str,
    decision_id: &str,
    action: &str,
    resource_id: &str,
    resource_type: &str,
    governance: &GovernanceContext,
) -> Result<(), String> {
    // Fire waiting notification.
    (governance.on_event)(GovernanceEvent::Waiting {
        decision_id: decision_id.to_string(),
        action: action.to_string(),
        resource_id: resource_id.to_string(),
    });

    // If inline resolver is available, race it against server polling.
    if let Some(ref resolver) = governance.resolver {
        let resolver = resolver.clone();
        let prompt = GovernancePrompt {
            decision_id: decision_id.to_string(),
            action: action.to_string(),
            resource_type: resource_type.to_string(),
            resource_id: resource_id.to_string(),
        };
        let mut user_task = tokio::task::spawn_blocking(move || resolver(prompt));
        let start = std::time::Instant::now(); // determinism-ok: CLI timeout

        loop {
            tokio::select! {
                biased;
                result = &mut user_task => {
                    match result.unwrap_or(GovernanceDecision::Wait) {
                        GovernanceDecision::Approve { scope } => {
                            submit_decision(http, server_url, tenant, decision_id, true, Some(&scope)).await?;
                            fire_resolved(Some(governance), decision_id, true);
                            return Ok(());
                        }
                        GovernanceDecision::Deny => {
                            submit_decision(http, server_url, tenant, decision_id, false, None).await?;
                            fire_resolved(Some(governance), decision_id, false);
                            return Err(format!(
                                "temper.{action} denied by user. Decision: {decision_id}"
                            ));
                        }
                        GovernanceDecision::Wait => break,
                    }
                }
                _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {
                    if start.elapsed() > GOVERNANCE_TIMEOUT {
                        user_task.abort();
                        return Err(format!(
                            "temper.{action} denied — approval timed out after 5 min. \
                             Decision: {decision_id}"
                        ));
                    }
                    if let Some(approved) = check_decision_status(http, server_url, tenant, decision_id).await? {
                        user_task.abort();
                        fire_resolved(Some(governance), decision_id, approved);
                        if approved {
                            return Ok(());
                        }
                        return Err(format!(
                            "temper.{action} denied externally. Decision: {decision_id}"
                        ));
                    }
                }
            }
        }
    }

    // Polling-only fallback.
    poll_for_decision(
        http,
        server_url,
        tenant,
        action,
        resource_id,
        decision_id,
        Some(governance),
    )
    .await
}

/// Fire the governance resolved callback.
fn fire_resolved(governance: Option<&GovernanceContext>, decision_id: &str, approved: bool) {
    if let Some(ctx) = governance {
        (ctx.on_event)(GovernanceEvent::Resolved {
            decision_id: decision_id.to_string(),
            approved,
        });
    } else if approved {
        eprintln!("  [governance] Approved! Proceeding.");
    } else {
        eprintln!("  [governance] Denied.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── HTTP client tools ───────────────────────────────────────────

    #[test]
    fn blocks_curl() {
        assert!(is_network_command("curl https://api.example.com").is_some());
    }

    #[test]
    fn blocks_wget() {
        assert!(is_network_command("wget https://example.com/file.tar.gz").is_some());
    }

    #[test]
    fn blocks_httpie() {
        assert!(is_network_command("http GET https://api.example.com/users").is_some());
    }

    #[test]
    fn blocks_curl_in_pipe() {
        assert!(is_network_command("echo test | curl -d @- https://api.example.com").is_some());
    }

    #[test]
    fn blocks_curl_after_semicolon() {
        assert!(is_network_command("echo hello; curl https://api.example.com").is_some());
    }

    #[test]
    fn blocks_curl_after_and() {
        assert!(is_network_command("cd /tmp && curl https://api.example.com").is_some());
    }

    // ── URLs in commands ────────────────────────────────────────────

    #[test]
    fn blocks_http_url() {
        assert!(is_network_command("python fetch.py http://example.com").is_some());
    }

    #[test]
    fn blocks_https_url() {
        assert!(is_network_command("node script.js https://api.example.com").is_some());
    }

    // ── Network tools ───────────────────────────────────────────────

    #[test]
    fn blocks_nc() {
        assert!(is_network_command("nc example.com 80").is_some());
    }

    #[test]
    fn blocks_ncat() {
        assert!(is_network_command("ncat --ssl example.com 443").is_some());
    }

    #[test]
    fn blocks_netcat() {
        assert!(is_network_command("netcat -l 8080").is_some());
    }

    #[test]
    fn blocks_ssh() {
        assert!(is_network_command("ssh user@host.example.com").is_some());
    }

    #[test]
    fn blocks_scp() {
        assert!(is_network_command("scp file.txt user@host:/tmp/").is_some());
    }

    #[test]
    fn blocks_sftp() {
        assert!(is_network_command("sftp user@host.example.com").is_some());
    }

    #[test]
    fn blocks_ftp() {
        assert!(is_network_command("ftp ftp.example.com").is_some());
    }

    #[test]
    fn blocks_remote_rsync() {
        assert!(is_network_command("rsync -avz ./files/ user@host:/backup/").is_some());
    }

    #[test]
    fn allows_local_rsync() {
        assert!(is_network_command("rsync -avz /src/ /dest/").is_none());
    }

    // ── DNS / ping ──────────────────────────────────────────────────

    #[test]
    fn blocks_dig() {
        assert!(is_network_command("dig example.com").is_some());
    }

    #[test]
    fn blocks_nslookup() {
        assert!(is_network_command("nslookup example.com").is_some());
    }

    #[test]
    fn blocks_ping_external() {
        assert!(is_network_command("ping google.com").is_some());
    }

    #[test]
    fn allows_ping_localhost() {
        assert!(is_network_command("ping localhost").is_none());
    }

    #[test]
    fn allows_ping_loopback() {
        assert!(is_network_command("ping 127.0.0.1").is_none());
    }

    // ── Package managers ────────────────────────────────────────────

    #[test]
    fn blocks_pip_install() {
        assert!(is_network_command("pip install requests").is_some());
    }

    #[test]
    fn blocks_npm_install() {
        assert!(is_network_command("npm install express").is_some());
    }

    #[test]
    fn blocks_cargo_install() {
        assert!(is_network_command("cargo install ripgrep").is_some());
    }

    // ── Scripting with network libs ─────────────────────────────────

    #[test]
    fn blocks_requests_get() {
        assert!(
            is_network_command("python -c \"import requests; requests.get('http://x')\"").is_some()
        );
    }

    #[test]
    fn blocks_urllib() {
        assert!(
            is_network_command("python -c \"import urllib; urllib.request.urlopen('http://x')\"")
                .is_some()
        );
    }

    #[test]
    fn blocks_fetch() {
        assert!(is_network_command("node -e \"fetch('http://api.example.com')\"").is_some());
    }

    #[test]
    fn blocks_http_client() {
        assert!(is_network_command("python -c \"import http.client\"").is_some());
    }

    // ── Allowed local commands ──────────────────────────────────────

    #[test]
    fn allows_ls() {
        assert!(is_network_command("ls -la /tmp").is_none());
    }

    #[test]
    fn allows_cat() {
        assert!(is_network_command("cat /etc/hosts").is_none());
    }

    #[test]
    fn allows_grep() {
        assert!(is_network_command("grep -r 'pattern' ./src").is_none());
    }

    #[test]
    fn allows_cargo_build() {
        assert!(is_network_command("cargo build --release").is_none());
    }

    #[test]
    fn allows_cargo_test() {
        assert!(is_network_command("cargo test --workspace").is_none());
    }

    #[test]
    fn allows_rustc() {
        assert!(is_network_command("rustc main.rs").is_none());
    }

    #[test]
    fn allows_python_script() {
        assert!(is_network_command("python script.py").is_none());
    }

    #[test]
    fn allows_echo() {
        assert!(is_network_command("echo hello world").is_none());
    }

    #[test]
    fn allows_sed() {
        assert!(is_network_command("sed -i 's/old/new/g' file.txt").is_none());
    }

    #[test]
    fn allows_awk() {
        assert!(is_network_command("awk '{print $1}' data.csv").is_none());
    }

    #[test]
    fn allows_mkdir() {
        assert!(is_network_command("mkdir -p /tmp/project").is_none());
    }

    #[test]
    fn allows_cp_mv_rm() {
        assert!(
            is_network_command("cp file.txt /tmp/ && mv /tmp/file.txt /tmp/renamed.txt").is_none()
        );
    }

    // ── has_program helper ──────────────────────────────────────────

    #[test]
    fn has_program_at_start() {
        assert!(has_program("curl http://x", "curl"));
    }

    #[test]
    fn has_program_after_pipe() {
        assert!(has_program("echo x | curl -d @-", "curl"));
    }

    #[test]
    fn has_program_after_semicolon() {
        assert!(has_program("echo x; ssh host", "ssh"));
    }

    #[test]
    fn has_program_after_and_and() {
        assert!(has_program("cd /tmp && wget http://x", "wget"));
    }

    #[test]
    fn has_program_no_false_positive_substring() {
        // "curly" should not match "curl"
        assert!(!has_program("echo curly", "curl"));
    }

    #[test]
    fn has_program_no_false_positive_grep_httpie() {
        // "grep http" should not match program "http" — "grep" is the program
        assert!(!has_program("grep http file.txt", "http"));
    }

    // ── Error message includes guidance ─────────────────────────────

    #[test]
    fn error_includes_integration_guidance() {
        let cmd = "curl https://api.example.com/data";
        let result = is_network_command(cmd);
        assert!(result.is_some());
        // The caller appends INTEGRATION_GUIDANCE; verify it's non-empty.
        assert!(INTEGRATION_GUIDANCE.contains("[[integration]]"));
        assert!(INTEGRATION_GUIDANCE.contains("http_fetch"));
        assert!(INTEGRATION_GUIDANCE.contains("compile_wasm"));
    }
}
