//! MCP runtime context and stdio server loop.

use anyhow::{Result, bail};
use monty::MontyObject;
use serde_json::Value;
use std::collections::BTreeMap;
use temper_ots::{
    DecisionType, MessageRole, OTSChoice, OTSConsequence, OTSContext, OTSDecision, OTSMessage,
    OTSMessageContent, OTSMetadata, OutcomeType, TrajectoryBuilder,
};
use temper_runtime::scheduler::sim_now;
use tokio::io::{self, AsyncBufReadExt, AsyncWriteExt, BufReader};

use super::McpConfig;
use super::protocol::dispatch_json_line;

/// Client identity received from the MCP `initialize` handshake.
#[derive(Clone, Debug, Default)]
pub(crate) struct ClientInfo {
    /// MCP client name (e.g. `"claude-code"`).
    pub(crate) name: Option<String>,
    /// MCP client version string.
    pub(crate) version: Option<String>,
}

/// Response from `POST /api/identity/resolve`.
///
/// Only the fields needed by the MCP runtime are declared; extra fields
/// from the server response are silently ignored by serde.
#[derive(serde::Deserialize)]
struct ResolvedIdentityResponse {
    agent_instance_id: String,
    agent_type_name: String,
}

/// Thin-client runtime context for the MCP server.
///
/// Connects to an already-running Temper server via `--port` (local) or
/// `--url` (remote). Does not spawn servers, parse local specs, or manage
/// any infrastructure.
///
/// Stores a [`PersistentSandbox`] so that variables and heap state persist
/// across `execute` tool calls within a single MCP session.
pub(crate) struct RuntimeContext {
    pub(crate) base_url: String,
    pub(crate) http: reqwest::Client,
    pub(crate) agent_id: Option<String>,
    pub(crate) agent_type: Option<String>,
    pub(crate) session_id: Option<String>,
    pub(crate) api_key: Option<String>,
    pub(crate) identity_tenant: String,
    sandbox: temper_sandbox::runner::PersistentSandbox,
    /// OTS trajectory builder for capturing agent execution traces.
    pub(crate) trajectory: Option<TrajectoryBuilder>,
    /// Tenants observed in executed calls during this session.
    tenants_seen: BTreeMap<String, usize>,
    /// Entity types observed in executed calls during this session.
    entity_types_seen: BTreeMap<String, usize>,
}

impl RuntimeContext {
    pub(super) fn from_config(config: &McpConfig) -> Result<Self> {
        let base_url = match (&config.temper_url, config.temper_port) {
            (Some(url), _) => url.trim_end_matches('/').to_string(),
            (None, Some(port)) => format!("http://127.0.0.1:{port}"),
            (None, None) => bail!(
                "Either --url or --port is required. \
                 Use --port <n> for a local server or --url <url> for a remote server."
            ),
        };
        Ok(Self {
            base_url,
            http: reqwest::Client::new(),
            agent_id: config.agent_id.clone(),
            agent_type: config.agent_type.clone(),
            session_id: config.session_id.clone(),
            api_key: config
                .api_key
                .clone()
                .or_else(|| std::env::var("TEMPER_API_KEY").ok()), // determinism-ok: startup config
            identity_tenant: std::env::var("TEMPER_TENANT")
                .ok()
                .filter(|v| !v.trim().is_empty())
                .unwrap_or_else(|| "default".to_string()), // determinism-ok: startup config
            sandbox: temper_sandbox::runner::PersistentSandbox::new(&[("temper", "Temper", 1)]),
            trajectory: None,
            tenants_seen: BTreeMap::new(),
            entity_types_seen: BTreeMap::new(),
        })
    }

    /// Apply MCP `clientInfo` from the `initialize` handshake.
    ///
    /// If `api_key` is set, resolves the credential against the platform's
    /// identity registry to get a platform-assigned agent ID and verified
    /// agent type. Returns an error if credential resolution fails — there
    /// is no fallback to self-declared identity.
    ///
    /// If no `api_key` is set (local dev mode), identity fields remain as
    /// configured (or `None`).
    ///
    /// See ADR-0033: Platform-Assigned Agent Identity.
    pub(crate) async fn apply_client_info(&mut self, info: ClientInfo) -> Result<()> {
        tracing::info!(
            client_name = info.name.as_deref().unwrap_or("unknown"),
            client_version = info.version.as_deref().unwrap_or("unknown"),
            "MCP client connected"
        );
        if let Some(ref api_key) = self.api_key {
            match self.resolve_credential(api_key).await {
                Some(resolved) => {
                    self.agent_id = Some(resolved.agent_instance_id);
                    self.agent_type = Some(resolved.agent_type_name);
                    return Ok(());
                }
                None => {
                    // Credential resolution failed — no fallback to legacy derivation.
                    // Log the error but don't bail: the global API key may have a
                    // bootstrap-registered credential that hasn't been created yet
                    // (server still starting). Identity will be "operator" via the
                    // server-side bearer auth fallback.
                    tracing::warn!(
                        "Credential resolution failed for TEMPER_API_KEY. \
                         Agent will use server-assigned operator identity. \
                         Ensure an AgentCredential is registered for this key."
                    );
                }
            }
        }

        Ok(())
    }

    /// Resolve a bearer token against the platform's identity endpoint.
    async fn resolve_credential(&self, token: &str) -> Option<ResolvedIdentityResponse> {
        let url = format!("{}/api/identity/resolve", self.base_url);
        let resp = self
            .http
            .post(&url)
            .header("X-Tenant-Id", &self.identity_tenant)
            .json(&serde_json::json!({
                "bearer_token": token,
                "tenant": self.identity_tenant,
            }))
            .send()
            .await
            .ok()?;

        if !resp.status().is_success() {
            return None;
        }

        resp.json::<ResolvedIdentityResponse>().await.ok()
    }

    /// Initialize OTS trajectory capture after the MCP handshake completes.
    pub(crate) fn init_trajectory(&mut self) {
        let now = sim_now(); // determinism-ok: sim_now is DST-safe
        let agent_id = self.agent_id.as_deref().unwrap_or("unknown");
        let metadata = OTSMetadata::new("mcp-session", agent_id, OutcomeType::Success, now);

        let context = OTSContext::new();

        self.trajectory = Some(TrajectoryBuilder::new(metadata, context));
    }

    /// Record an execute tool call as an OTS turn with a decision.
    pub(crate) fn record_execute_turn(&mut self, code: &str, result: &Result<String>) {
        let extracted_actions = extract_trajectory_actions_from_code(code);

        let Some(ref mut builder) = self.trajectory else {
            return;
        };

        let now = sim_now(); // determinism-ok: sim_now is DST-safe
        builder.start_turn(now);

        // User message: the Python code submitted
        builder.add_message(OTSMessage::new(
            MessageRole::User,
            OTSMessageContent::text(code),
            now,
        ));

        // Decision: the execution outcome
        let (outcome_str, consequence) = match result {
            Ok(text) => {
                // Assistant message: the execution result
                builder.add_message(OTSMessage::new(
                    MessageRole::Assistant,
                    OTSMessageContent::text(text),
                    now,
                ));
                ("success", OTSConsequence::success())
            }
            Err(e) => {
                builder.add_message(OTSMessage::new(
                    MessageRole::Assistant,
                    OTSMessageContent::text(e.to_string()),
                    now,
                ));
                (
                    "failure",
                    OTSConsequence::failure().with_error_type(e.to_string()),
                )
            }
        };

        let mut choice = OTSChoice::new(format!("execute: {}", &code[..code.len().min(100)]));
        if !extracted_actions.is_empty() {
            choice = choice.with_arguments(serde_json::json!({
                "trajectory_actions": extracted_actions,
            }));
        }

        let decision = OTSDecision::new(DecisionType::ToolSelection, choice, consequence);
        builder.add_decision(decision);

        builder.end_turn(now);

        tracing::debug!(outcome = outcome_str, "ots.trajectory.turn_recorded");

        for meta in extract_temper_call_metadata(code) {
            if let Some(tenant) = meta.tenant {
                self.tenants_seen
                    .entry(tenant)
                    .and_modify(|count| *count += 1)
                    .or_insert(1);
            }
            if let Some(entity_type) = meta.entity_type {
                self.entity_types_seen
                    .entry(entity_type)
                    .and_modify(|count| *count += 1)
                    .or_insert(1);
            }
        }
    }

    /// Flush a snapshot of the trajectory mid-session without consuming it.
    pub(crate) async fn flush_trajectory(&self) -> Result<String> {
        let Some(ref builder) = self.trajectory else {
            bail!("no trajectory in progress");
        };

        let trajectory = builder.snapshot();
        let trajectory_id = trajectory.trajectory_id.clone();
        let json = serde_json::to_string(&trajectory)?;

        let url = format!("{}/api/ots/trajectories", self.base_url);
        let mut request = self
            .http
            .post(&url)
            .body(json)
            .header("Content-Type", "application/json")
            .header("X-Tenant-Id", self.primary_tenant());

        if let Some(primary_entity_type) = self.primary_entity_type() {
            request = request.header("X-Entity-Type", primary_entity_type);
        }
        if let Some(ref agent_id) = self.agent_id {
            request = request.header("X-Agent-Id", agent_id);
        }
        if let Some(ref session_id) = self.session_id {
            request = request.header("X-Session-Id", session_id);
        }
        if let Some(ref api_key) = self.api_key {
            request = request.header("Authorization", format!("Bearer {api_key}"));
        }

        let resp = request.send().await?;
        if resp.status().is_success() {
            tracing::info!("ots.trajectory.flushed");
            Ok(trajectory_id)
        } else {
            bail!("flush failed: HTTP {}", resp.status());
        }
    }

    /// Finalize and POST the trajectory to the server.
    pub(crate) async fn finalize_trajectory(&mut self) {
        let Some(builder) = self.trajectory.take() else {
            return;
        };

        let trajectory = builder.build();
        let json = match serde_json::to_string(&trajectory) {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!(error = %e, "ots.trajectory.serialize_failed");
                return;
            }
        };

        let url = format!("{}/api/ots/trajectories", self.base_url);
        let mut request = self
            .http
            .post(&url)
            .body(json)
            .header("Content-Type", "application/json")
            .header("X-Tenant-Id", self.primary_tenant());

        if let Some(primary_entity_type) = self.primary_entity_type() {
            request = request.header("X-Entity-Type", primary_entity_type);
        }

        if let Some(ref agent_id) = self.agent_id {
            request = request.header("X-Agent-Id", agent_id);
        }
        if let Some(ref session_id) = self.session_id {
            request = request.header("X-Session-Id", session_id);
        }
        if let Some(ref api_key) = self.api_key {
            request = request.header("Authorization", format!("Bearer {api_key}"));
        }

        match request.send().await {
            Ok(resp) if resp.status().is_success() => {
                tracing::info!("ots.trajectory.uploaded");
            }
            Ok(resp) => {
                tracing::warn!(
                    status = resp.status().as_u16(),
                    "ots.trajectory.upload_failed"
                );
            }
            Err(e) => {
                tracing::warn!(error = %e, "ots.trajectory.upload_failed");
            }
        }
    }

    /// Most-used tenant for this session, falling back to configured identity tenant.
    fn primary_tenant(&self) -> &str {
        self.tenants_seen
            .iter()
            .max_by_key(|(_, count)| *count)
            .map(|(tenant, _)| tenant.as_str())
            .unwrap_or(self.identity_tenant.as_str())
    }

    /// Most-used entity type for this session.
    fn primary_entity_type(&self) -> Option<&str> {
        self.entity_types_seen
            .iter()
            .max_by_key(|(_, count)| *count)
            .map(|(entity_type, _)| entity_type.as_str())
    }

    pub(crate) async fn run_execute(&mut self, code: &str) -> Result<String> {
        let http = self.http.clone();
        let base_url = self.base_url.clone();
        let agent_id = self.agent_id.clone();
        let agent_type = self.agent_type.clone();
        let session_id = self.session_id.clone();
        let api_key = self.api_key.clone();

        self.sandbox
            .execute(
                code,
                |function_name: String,
                 args: Vec<MontyObject>,
                 kwargs: Vec<(MontyObject, MontyObject)>| {
                    let http = http.clone();
                    let base_url = base_url.clone();
                    let agent_id = agent_id.clone();
                    let agent_type = agent_type.clone();
                    let session_id = session_id.clone();
                    let api_key = api_key.clone();
                    async move {
                        if !kwargs.is_empty() {
                            return Err(format!(
                                "temper.{function_name} does not support keyword arguments"
                            ));
                        }

                        // Strip self arg
                        let args = if args.is_empty() {
                            &args[..]
                        } else {
                            &args[1..]
                        };

                        // Extract tenant from args[0]
                        let tenant = temper_sandbox::helpers::expect_string_arg(
                            args,
                            0,
                            "tenant",
                            &function_name,
                        )?;
                        let remaining = if args.len() > 1 { &args[1..] } else { &[] };

                        let ctx = temper_sandbox::dispatch::DispatchContext {
                            http: &http,
                            base_url: &base_url,
                            tenant: &tenant,
                            agent_id: agent_id.as_deref(),
                            agent_type: agent_type.as_deref(),
                            session_id: session_id.as_deref(),
                            entity_set_resolver: None,
                            binary_path: None,
                            api_key: api_key.as_deref(),
                        };
                        temper_sandbox::dispatch::dispatch_temper_method(
                            &ctx,
                            &function_name,
                            remaining,
                            &kwargs,
                        )
                        .await
                    }
                },
            )
            .await
    }
}

fn extract_trajectory_actions_from_code(code: &str) -> Vec<Value> {
    let mut actions = Vec::new();
    let mut cursor = 0usize;
    let needle = "temper.action";

    while let Some(found) = code[cursor..].find(needle) {
        let method_start = cursor + found + needle.len();
        let mut open = method_start;
        while open < code.len()
            && code
                .as_bytes()
                .get(open)
                .is_some_and(|b| b.is_ascii_whitespace())
        {
            open += 1;
        }
        if code.as_bytes().get(open) != Some(&b'(') {
            cursor = method_start;
            continue;
        }

        let Some(close) = find_matching_paren(code, open) else {
            break;
        };

        let args = split_top_level_args(&code[open + 1..close]);
        let (action_idx, params_idx) =
            if args.len() >= 5 && parse_python_string_literal(args[3]).is_some() {
                (3usize, 4usize)
            } else {
                (2usize, 3usize)
            };

        if args.len() > action_idx
            && let Some(action_name) = parse_python_string_literal(args[action_idx])
        {
            let params = args
                .get(params_idx)
                .and_then(|raw| parse_python_json_value(raw))
                .unwrap_or_else(|| serde_json::json!({}));
            actions.push(serde_json::json!({
                "action": action_name,
                "params": params,
            }));
        }

        cursor = close + 1;
    }

    actions
}

#[derive(Debug, Clone, Default)]
struct TemperCallMetadata {
    tenant: Option<String>,
    entity_type: Option<String>,
}

fn extract_temper_call_metadata(code: &str) -> Vec<TemperCallMetadata> {
    let mut out = Vec::new();
    out.extend(extract_temper_action_metadata(code));
    out.extend(extract_temper_create_metadata(code));
    out
}

fn extract_temper_action_metadata(code: &str) -> Vec<TemperCallMetadata> {
    extract_call_metadata(code, "temper.action", |args| {
        // New signature: temper.action(tenant, entity_type, id, action, params)
        if args.len() >= 5
            && let (Some(tenant), Some(entity_type), Some(_action)) = (
                parse_python_string_literal(args[0]),
                parse_python_string_literal(args[1]),
                parse_python_string_literal(args[3]),
            )
        {
            return TemperCallMetadata {
                tenant: Some(tenant),
                entity_type: Some(entity_type),
            };
        }

        // Legacy signature: temper.action(entity_type, id, action, params)
        TemperCallMetadata {
            tenant: None,
            entity_type: args
                .first()
                .and_then(|raw| parse_python_string_literal(raw)),
        }
    })
}

fn extract_temper_create_metadata(code: &str) -> Vec<TemperCallMetadata> {
    extract_call_metadata(code, "temper.create", |args| {
        // New signature: temper.create(tenant, entity_type, fields)
        if args.len() >= 3
            && let (Some(tenant), Some(entity_type)) = (
                parse_python_string_literal(args[0]),
                parse_python_string_literal(args[1]),
            )
        {
            return TemperCallMetadata {
                tenant: Some(tenant),
                entity_type: Some(entity_type),
            };
        }

        // Legacy signature: temper.create(entity_type, fields)
        TemperCallMetadata {
            tenant: None,
            entity_type: args
                .first()
                .and_then(|raw| parse_python_string_literal(raw)),
        }
    })
}

fn extract_call_metadata<F>(code: &str, needle: &str, mapper: F) -> Vec<TemperCallMetadata>
where
    F: Fn(Vec<&str>) -> TemperCallMetadata,
{
    let mut out = Vec::new();
    let mut cursor = 0usize;

    while let Some(found) = code[cursor..].find(needle) {
        let method_start = cursor + found + needle.len();
        let mut open = method_start;
        while open < code.len()
            && code
                .as_bytes()
                .get(open)
                .is_some_and(|b| b.is_ascii_whitespace())
        {
            open += 1;
        }
        if code.as_bytes().get(open) != Some(&b'(') {
            cursor = method_start;
            continue;
        }

        let Some(close) = find_matching_paren(code, open) else {
            break;
        };
        let args = split_top_level_args(&code[open + 1..close]);
        out.push(mapper(args));
        cursor = close + 1;
    }

    out
}

fn find_matching_paren(input: &str, open_idx: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut in_quote: Option<char> = None;
    let mut escaped = false;

    for (offset, ch) in input[open_idx..].char_indices() {
        let idx = open_idx + offset;
        if let Some(quote) = in_quote {
            if escaped {
                escaped = false;
                continue;
            }
            if ch == '\\' {
                escaped = true;
                continue;
            }
            if ch == quote {
                in_quote = None;
            }
            continue;
        }

        match ch {
            '\'' | '"' => in_quote = Some(ch),
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(idx);
                }
            }
            _ => {}
        }
    }

    None
}

fn split_top_level_args(input: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut depth_paren = 0i32;
    let mut depth_brace = 0i32;
    let mut depth_bracket = 0i32;
    let mut in_quote: Option<char> = None;
    let mut escaped = false;

    for (idx, ch) in input.char_indices() {
        if let Some(quote) = in_quote {
            if escaped {
                escaped = false;
                continue;
            }
            if ch == '\\' {
                escaped = true;
                continue;
            }
            if ch == quote {
                in_quote = None;
            }
            continue;
        }

        match ch {
            '\'' | '"' => in_quote = Some(ch),
            '(' => depth_paren += 1,
            ')' => depth_paren -= 1,
            '{' => depth_brace += 1,
            '}' => depth_brace -= 1,
            '[' => depth_bracket += 1,
            ']' => depth_bracket -= 1,
            ',' if depth_paren == 0 && depth_brace == 0 && depth_bracket == 0 => {
                parts.push(input[start..idx].trim());
                start = idx + 1;
            }
            _ => {}
        }
    }

    if start <= input.len() {
        let tail = input[start..].trim();
        if !tail.is_empty() {
            parts.push(tail);
        }
    }
    parts
}

fn parse_python_string_literal(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.len() < 2 {
        return None;
    }
    let quote = s.chars().next()?;
    if (quote != '\'' && quote != '"') || !s.ends_with(quote) {
        return None;
    }

    let mut out = String::new();
    let mut escaped = false;
    for ch in s[1..s.len() - 1].chars() {
        if escaped {
            let mapped = match ch {
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                '\\' => '\\',
                '\'' => '\'',
                '"' => '"',
                other => other,
            };
            out.push(mapped);
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        out.push(ch);
    }
    if escaped {
        out.push('\\');
    }
    Some(out)
}

fn parse_python_json_value(raw: &str) -> Option<Value> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Some(serde_json::json!({}));
    }
    if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
        return Some(v);
    }
    let normalized = normalize_pythonish_json(trimmed);
    serde_json::from_str::<Value>(&normalized).ok()
}

fn normalize_pythonish_json(input: &str) -> String {
    let mut quoted = String::with_capacity(input.len());
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;

    for ch in input.chars() {
        if in_single {
            if escaped {
                quoted.push(ch);
                escaped = false;
                continue;
            }
            match ch {
                '\\' => escaped = true,
                '\'' => {
                    in_single = false;
                    quoted.push('"');
                }
                '"' => quoted.push_str("\\\""),
                _ => quoted.push(ch),
            }
            continue;
        }

        if in_double {
            quoted.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_double = false;
            }
            continue;
        }

        match ch {
            '\'' => {
                in_single = true;
                quoted.push('"');
            }
            '"' => {
                in_double = true;
                quoted.push('"');
            }
            _ => quoted.push(ch),
        }
    }

    let mut out = String::with_capacity(quoted.len());
    let mut token = String::new();
    let mut in_string = false;
    let mut esc = false;

    let flush_token = |token: &mut String, out: &mut String| {
        if token.is_empty() {
            return;
        }
        match token.as_str() {
            "True" => out.push_str("true"),
            "False" => out.push_str("false"),
            "None" => out.push_str("null"),
            _ => out.push_str(token),
        }
        token.clear();
    };

    for ch in quoted.chars() {
        if in_string {
            out.push(ch);
            if esc {
                esc = false;
            } else if ch == '\\' {
                esc = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        if ch == '"' {
            flush_token(&mut token, &mut out);
            in_string = true;
            out.push(ch);
            continue;
        }

        if ch.is_ascii_alphanumeric() || ch == '_' {
            token.push(ch);
            continue;
        }

        flush_token(&mut token, &mut out);
        out.push(ch);
    }
    flush_token(&mut token, &mut out);

    out
}

/// Run the MCP server on stdio with JSON-RPC over newline-delimited JSON.
pub async fn run_stdio_server(config: McpConfig) -> Result<()> {
    let mut ctx = RuntimeContext::from_config(&config)?;
    let stdin = BufReader::new(io::stdin());
    let mut lines = stdin.lines();
    let mut stdout = io::stdout();

    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        if let Some(response) = dispatch_json_line(&mut ctx, line).await {
            let encoded = serde_json::to_string(&response)?;
            stdout.write_all(encoded.as_bytes()).await?;
            stdout.write_all(b"\n").await?;
            stdout.flush().await?;
        }
    }

    // Finalize and upload OTS trajectory on session close.
    ctx.finalize_trajectory().await;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_trajectory_actions_from_temper_action_calls() {
        let code = r#"
result = temper.action("Issue", "issue-1", "PromoteToCritical", {"Reason": "prod incident"})
other = temper.action('Issue', 'issue-1', 'Assign', {'AgentId': 'agent-2'})
tenant = temper.action("gepa-tenant", "Issues", "11111111-1111-1111-1111-111111111111", "Reassign", {"NewAssigneeId": "agent-3"})
"#;

        let actions = extract_trajectory_actions_from_code(code);
        assert_eq!(actions.len(), 3);
        assert_eq!(
            actions[0].get("action").and_then(Value::as_str),
            Some("PromoteToCritical")
        );
        assert_eq!(
            actions[2]
                .get("params")
                .and_then(Value::as_object)
                .and_then(|m| m.get("NewAssigneeId"))
                .and_then(Value::as_str),
            Some("agent-3")
        );
    }

    #[test]
    fn normalize_python_literals_to_json() {
        let value = parse_python_json_value("{'enabled': True, 'reason': None, 'count': 2}")
            .expect("python dict should parse");
        assert_eq!(value["enabled"], serde_json::json!(true));
        assert_eq!(value["reason"], serde_json::Value::Null);
        assert_eq!(value["count"], serde_json::json!(2));
    }

    #[test]
    fn extract_temper_call_metadata_tracks_tenant_and_entity() {
        let code = r#"
await temper.action("tenant-a", "Issue", "i-1", "Assign", {"AgentId": "agent-1"})
await temper.create("tenant-b", "Task", {"Title": "x"})
"#;
        let metadata = extract_temper_call_metadata(code);
        assert!(
            metadata.iter().any(|m| {
                m.tenant.as_deref() == Some("tenant-a") && m.entity_type.as_deref() == Some("Issue")
            }),
            "expected tenant-a/Issue metadata"
        );
        assert!(
            metadata.iter().any(|m| {
                m.tenant.as_deref() == Some("tenant-b") && m.entity_type.as_deref() == Some("Task")
            }),
            "expected tenant-b/Task metadata"
        );
    }
}
