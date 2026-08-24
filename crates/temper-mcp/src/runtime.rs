//! MCP runtime context and stdio server loop.

use anyhow::{Result, bail};
use monty::MontyObject;
use serde_json::Value;
use std::collections::BTreeMap;
use std::time::Duration;
use temper_ots::{
    DecisionType, MessageRole, OTSChoice, OTSConsequence, OTSContext, OTSDecision, OTSMessage,
    OTSMessageContent, OTSMetadata, OutcomeType, TrajectoryBuilder,
};
use temper_runtime::scheduler::sim_now;
use tokio::io::{self, AsyncWriteExt, BufReader};

use super::McpConfig;
use super::protocol::dispatch_json_line;
use crate::trajectory_bounds::{
    MAX_STDIO_LINE_BYTES, MAX_TRAJECTORY_TOTAL_BYTES, MAX_TRAJECTORY_TURNS, StdioFrame,
    TRAJECTORY_TURN_ENVELOPE_BYTES, bounded_trajectory_actions, bump_seen, floor_char_boundary,
    json_string_cost, json_value_cost, read_stdio_frame, trajectory_storage_tenant,
    truncate_trajectory_text,
};

const OTS_UPLOAD_MAX_ATTEMPTS: u32 = 3;
const OTS_UPLOAD_RETRY_DELAY_MS: u64 = 100;

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
    pub(crate) allow_host_ops: bool,
    sandbox: temper_sandbox::runner::PersistentSandbox,
    /// OTS trajectory builder for capturing agent execution traces.
    pub(crate) trajectory: Option<TrajectoryBuilder>,
    /// Tenants observed in executed calls during this session (observability
    /// signal only; never used to route trajectory storage).
    tenants_seen: BTreeMap<String, usize>,
    /// Number of turns recorded into the current trajectory (bounds its size).
    turns_recorded: usize,
    /// Estimated serialized bytes recorded so far. Bounds the whole upload to
    /// under the server's ingest limit so a within-per-turn-cap session can't
    /// still produce a trajectory the server rejects (ARN-222).
    trajectory_bytes: usize,
    /// Whether the turn/byte cap has already been reported for this trajectory,
    /// so hitting the cap warns once rather than on every subsequent turn.
    capped_warned: bool,
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
            allow_host_ops: true,
            sandbox: temper_sandbox::runner::PersistentSandbox::new(&[("temper", "Temper", 1)]),
            trajectory: None,
            tenants_seen: BTreeMap::new(),
            turns_recorded: 0,
            trajectory_bytes: 0,
            capped_warned: false,
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
    ///
    /// Resets the per-trajectory budgets so a client that re-sends `initialize`
    /// gets a fresh trajectory with a fresh turn/byte budget rather than
    /// inheriting a consumed one.
    pub(crate) fn init_trajectory(&mut self) {
        let now = sim_now(); // determinism-ok: sim_now is DST-safe
        let agent_id = self.agent_id.as_deref().unwrap_or("unknown");
        let metadata = OTSMetadata::new("mcp-session", agent_id, OutcomeType::Success, now);

        let context = OTSContext::new();

        self.trajectory = Some(TrajectoryBuilder::new(metadata, context));
        self.turns_recorded = 0;
        self.trajectory_bytes = 0;
        self.capped_warned = false;
    }

    /// Warn once per trajectory that a capture cap was hit, so a long agent loop
    /// that keeps executing past the cap does not emit an identical warning every
    /// turn.
    fn warn_capped_once(&mut self, cap: &str, limit: usize) {
        if !self.capped_warned {
            self.capped_warned = true;
            tracing::warn!(
                cap,
                limit,
                "mcp trajectory cap reached; dropping further turns"
            );
        }
    }

    /// Record an execute tool call as an OTS turn with a decision.
    pub(crate) fn record_execute_turn(&mut self, code: &str, result: &Result<String>) {
        if self.trajectory.is_none() {
            return;
        }
        // Bound the trajectory: drop further turns once the cap is reached (ARN-222).
        if self.turns_recorded >= MAX_TRAJECTORY_TURNS {
            self.warn_capped_once("turn count", MAX_TRAJECTORY_TURNS);
            return;
        }

        let extracted_actions = extract_trajectory_actions_from_code(code);
        // Bound recorded content: a large code blob or result can't grow the
        // trajectory without limit (ARN-222).
        let code_text = truncate_trajectory_text(code);
        let result_text = match result {
            Ok(text) => truncate_trajectory_text(text),
            Err(e) => truncate_trajectory_text(&e.to_string()),
        };
        let is_failure = result.is_err();
        let action_arguments = (!extracted_actions.is_empty()).then(|| {
            serde_json::json!({
                "trajectory_actions": bounded_trajectory_actions(extracted_actions),
            })
        });

        // Total-size budget: meter the turn's *serialized* contribution (escaped
        // text, the embedded actions, the duplicated error field, and a per-turn
        // envelope) — not raw string bytes, which undercount the wire size by 2x
        // or more. Stop recording once the cumulative serialized estimate would
        // exceed the budget, so the whole upload stays under the server's ingest
        // limit and is never rejected (and silently dropped) as too large, which
        // would suppress the audit trail (ARN-222).
        let turn_bytes = TRAJECTORY_TURN_ENVELOPE_BYTES
            + json_string_cost(&code_text)
            + json_string_cost(&result_text)
            + if is_failure {
                json_string_cost(&result_text)
            } else {
                0
            }
            + action_arguments.as_ref().map(json_value_cost).unwrap_or(0);
        if self.trajectory_bytes + turn_bytes > MAX_TRAJECTORY_TOTAL_BYTES {
            self.warn_capped_once("serialized byte budget", MAX_TRAJECTORY_TOTAL_BYTES);
            return;
        }
        self.turns_recorded += 1;
        self.trajectory_bytes += turn_bytes;

        let Some(ref mut builder) = self.trajectory else {
            return;
        };

        let now = sim_now(); // determinism-ok: sim_now is DST-safe
        builder.start_turn(now);

        // User message: the Python code submitted
        builder.add_message(OTSMessage::new(
            MessageRole::User,
            OTSMessageContent::text(code_text),
            now,
        ));

        // Decision: the execution outcome
        let (outcome_str, consequence) = match result {
            Ok(_) => {
                // Assistant message: the execution result
                builder.add_message(OTSMessage::new(
                    MessageRole::Assistant,
                    OTSMessageContent::text(result_text),
                    now,
                ));
                ("success", OTSConsequence::success())
            }
            Err(_) => {
                builder.add_message(OTSMessage::new(
                    MessageRole::Assistant,
                    OTSMessageContent::text(result_text.clone()),
                    now,
                ));
                // Bound the error_type too — it is the same (already truncated) text.
                (
                    "failure",
                    OTSConsequence::failure().with_error_type(result_text),
                )
            }
        };

        let label_end = floor_char_boundary(code, 100);
        let mut choice = OTSChoice::new(format!("execute: {}", &code[..label_end]));
        if let Some(arguments) = action_arguments {
            choice = choice.with_arguments(arguments);
        }

        let decision = OTSDecision::new(DecisionType::ToolSelection, choice, consequence);
        builder.add_decision(decision);

        builder.end_turn(now);

        tracing::debug!(outcome = outcome_str, "ots.trajectory.turn_recorded");

        for meta in extract_temper_call_metadata(code) {
            // Cap the distinct-key growth of these observability maps so a single
            // large code blob can't insert unbounded unique keys (ARN-222).
            if let Some(tenant) = meta.tenant {
                // Emit the cross-tenant signal once per unique foreign tenant, the
                // turn it is first tracked — never on every turn. `bump_seen`
                // returns true only when it actually inserts a new key, so once
                // `tenants_seen` saturates (or the key is oversized) it stops
                // returning true and the warn does not spam. Storage is unaffected.
                let foreign = if tenant != self.identity_tenant {
                    Some(tenant.clone())
                } else {
                    None
                };
                let newly_tracked = bump_seen(&mut self.tenants_seen, tenant);
                if let Some(referenced) = foreign
                    && newly_tracked
                {
                    tracing::warn!(
                        identity_tenant = %self.identity_tenant,
                        referenced_tenant = %referenced,
                        "mcp session code referenced a tenant other than its identity; \
                         trajectory stored under the identity tenant"
                    );
                }
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

        self.upload_trajectory_json(json).await?;
        tracing::info!("ots.trajectory.flushed");
        Ok(trajectory_id)
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

        match self.upload_trajectory_json(json).await {
            Ok(()) => {
                tracing::info!("ots.trajectory.uploaded");
            }
            Err(e) => {
                tracing::warn!(error = %e, "ots.trajectory.upload_failed");
            }
        }
    }

    async fn upload_trajectory_json(&self, json: String) -> Result<()> {
        let max_attempts = std::env::var("TEMPER_MCP_OTS_UPLOAD_MAX_ATTEMPTS")
            .ok()
            .and_then(|value| value.trim().parse::<u32>().ok())
            .filter(|attempts| *attempts > 0)
            .unwrap_or(OTS_UPLOAD_MAX_ATTEMPTS); // determinism-ok: MCP client runtime config
        let retry_delay = Duration::from_millis(
            std::env::var("TEMPER_MCP_OTS_UPLOAD_RETRY_DELAY_MS")
                .ok()
                .and_then(|value| value.trim().parse::<u64>().ok())
                .unwrap_or(OTS_UPLOAD_RETRY_DELAY_MS),
        ); // determinism-ok: MCP client runtime config

        let mut last_error = None;
        for attempt in 1..=max_attempts {
            match self.send_trajectory_json(json.clone()).await {
                Ok(()) => return Ok(()),
                Err(error) if attempt < max_attempts && error.retryable => {
                    tracing::warn!(
                        attempt,
                        max_attempts,
                        error = %error.message,
                        "ots.trajectory.upload_retry"
                    );
                    last_error = Some(error.message);
                    tokio::time::sleep(retry_delay).await;
                }
                Err(error) => {
                    return Err(anyhow::anyhow!(error.message));
                }
            }
        }

        Err(anyhow::anyhow!(last_error.unwrap_or_else(|| {
            "OTS trajectory upload failed".to_string()
        })))
    }

    async fn send_trajectory_json(&self, json: String) -> Result<(), TrajectoryUploadError> {
        let url = format!("{}/api/ots/trajectories", self.base_url);
        let mut request = self
            .http
            .post(&url)
            .body(json)
            .header("Content-Type", "application/json")
            .header(
                "X-Tenant-Id",
                trajectory_storage_tenant(&self.identity_tenant),
            );

        // No code-derived value is placed in a request header: a `\n` or other
        // illegal byte in an attacker-controlled entity type would make the HTTP
        // client reject the whole upload, silently losing the trajectory. The
        // previous `X-Entity-Type` header was code-derived and had no server-side
        // reader, so it is dropped entirely (ARN-222). Agent/session ids below are
        // startup config, not code-derived.
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
            Ok(resp) if resp.status().is_success() => Ok(()),
            Ok(resp) => {
                let status = resp.status();
                Err(TrajectoryUploadError {
                    message: format!("HTTP {status}"),
                    retryable: status.as_u16() == 503
                        || status.as_u16() == 408
                        || status.as_u16() == 425
                        || status.as_u16() == 429
                        || status.is_server_error(),
                })
            }
            Err(error) => Err(TrajectoryUploadError {
                message: error.to_string(),
                retryable: true,
            }),
        }
    }

    pub(crate) async fn run_execute(&mut self, code: &str) -> Result<String> {
        let http = self.http.clone();
        let base_url = self.base_url.clone();
        let agent_id = self.agent_id.clone();
        let session_id = self.session_id.clone();
        let api_key = self.api_key.clone();
        let allow_host_ops = self.allow_host_ops;

        self.sandbox
            .execute(
                code,
                |function_name: String,
                 args: Vec<MontyObject>,
                 kwargs: Vec<(MontyObject, MontyObject)>| {
                    let http = http.clone();
                    let base_url = base_url.clone();
                    let agent_id = agent_id.clone();
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
                            session_id: session_id.as_deref(),
                            entity_set_resolver: None,
                            binary_path: None,
                            api_key: api_key.as_deref(),
                            internal_credential_issuer: None,
                            // Stdio operates on the developer's machine. HTTP
                            // requests override this to preserve path isolation.
                            allow_host_ops,
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

struct TrajectoryUploadError {
    message: String,
    retryable: bool,
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
    /// Tenant referenced by the call, when the call uses the tenant-first
    /// signature. Used only as a cross-tenant observability signal — never to
    /// route trajectory storage (ARN-222).
    tenant: Option<String>,
}

fn extract_temper_call_metadata(code: &str) -> Vec<TemperCallMetadata> {
    let mut out = Vec::new();
    out.extend(extract_temper_action_metadata(code));
    out.extend(extract_temper_create_metadata(code));
    out
}

fn extract_temper_action_metadata(code: &str) -> Vec<TemperCallMetadata> {
    extract_call_metadata(code, "temper.action", |args| {
        // New signature: temper.action(tenant, entity_type, id, action, params).
        // Only the tenant is retained; the legacy signature carries no tenant.
        let tenant = (args.len() >= 5)
            .then(|| parse_python_string_literal(args[0]))
            .flatten();
        TemperCallMetadata { tenant }
    })
}

fn extract_temper_create_metadata(code: &str) -> Vec<TemperCallMetadata> {
    extract_call_metadata(code, "temper.create", |args| {
        // New signature: temper.create(tenant, entity_type, fields). Only the
        // tenant is retained; the legacy signature carries no tenant.
        let tenant = (args.len() >= 3)
            .then(|| parse_python_string_literal(args[0]))
            .flatten();
        TemperCallMetadata { tenant }
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
///
/// Frames are read through [`read_stdio_frame`], which bounds each frame to
/// `MAX_STDIO_LINE_BYTES`; oversized frames are dropped and invalid UTF-8 frames
/// are skipped rather than aborting the session.
pub async fn run_stdio_server(config: McpConfig) -> Result<()> {
    let mut ctx = RuntimeContext::from_config(&config)?;
    let mut stdin = BufReader::new(io::stdin());
    let mut stdout = io::stdout();

    loop {
        let buf = match read_stdio_frame(&mut stdin).await? {
            StdioFrame::Eof => break,
            StdioFrame::TooLarge => {
                tracing::warn!(
                    limit = MAX_STDIO_LINE_BYTES,
                    "mcp.stdio.frame_too_large: dropped oversized frame"
                );
                continue;
            }
            StdioFrame::Line(buf) => buf,
        };
        let line = match std::str::from_utf8(&buf) {
            Ok(text) => text.trim(),
            Err(_) => {
                tracing::warn!("mcp.stdio.invalid_utf8: dropped frame");
                continue;
            }
        };
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
#[path = "runtime_test.rs"]
mod tests;
