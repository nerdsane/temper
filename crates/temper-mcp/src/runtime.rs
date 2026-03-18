//! MCP runtime context and stdio server loop.

use anyhow::{Result, bail};
use monty::MontyObject;
use tokio::io::{self, AsyncBufReadExt, AsyncWriteExt, BufReader};

use super::McpConfig;
use super::protocol::dispatch_json_line;

/// Client identity received from the MCP `initialize` handshake.
#[derive(Clone, Debug, Default)]
pub(crate) struct ClientInfo {
    pub(crate) name: Option<String>,
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
    /// Identity from the MCP client's `initialize` request.
    pub(crate) client_info: ClientInfo,
    /// Timestamp when this MCP session started (used for agent ID derivation).
    pub(crate) started_at: std::time::Instant,
    sandbox: temper_sandbox::runner::PersistentSandbox,
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
            client_info: ClientInfo::default(),
            started_at: std::time::Instant::now(), // determinism-ok: startup config, used only for agent ID derivation
            sandbox: temper_sandbox::runner::PersistentSandbox::new(&[("temper", "Temper", 1)]),
        })
    }

    /// Apply MCP `clientInfo` from the `initialize` handshake.
    ///
    /// If `api_key` is set, resolves the credential against the platform's
    /// identity registry to get a platform-assigned agent ID and verified
    /// agent type. Falls back to SHA-256 derivation if resolution fails.
    ///
    /// See ADR-0033: Platform-Assigned Agent Identity.
    pub(crate) async fn apply_client_info(&mut self, info: ClientInfo) {
        // Try credential-based identity resolution first (ADR-0033).
        if let Some(ref api_key) = self.api_key {
            if let Some(resolved) = self.resolve_credential(api_key).await {
                self.agent_id = Some(resolved.agent_instance_id);
                self.agent_type = Some(resolved.agent_type_name);
                self.client_info = info;
                return;
            }
            // Resolution failed — fall back to legacy derivation.
            tracing::warn!("Credential resolution failed; falling back to SHA-256 ID derivation");
        }

        // Legacy fallback: derive agent_type from client name, agent_id from hash.
        if let Some(ref name) = info.name {
            self.agent_type = Some(name.clone());
        }

        if self.agent_id.is_none() || self.agent_id.as_deref() == Some("mcp-agent") {
            let client_name = info.name.as_deref().unwrap_or("unknown");
            let client_version = info.version.as_deref().unwrap_or("0");
            let host = hostname::get()
                .map(|h| h.to_string_lossy().into_owned())
                .unwrap_or_else(|_| "unknown".to_string()); // determinism-ok: startup config
            let nonce = self.started_at.elapsed().as_nanos();

            use sha2::{Digest, Sha256};
            let mut hasher = Sha256::new();
            hasher.update(format!("{client_name}:{client_version}:{host}:{nonce}"));
            let hash_bytes = hasher.finalize();
            let hash: String = hash_bytes.iter().map(|b| format!("{b:02x}")).collect();

            let prefix = match client_name {
                "claude-code" => "cc",
                "codex-cli" => "cx",
                _ => "mc",
            };
            self.agent_id = Some(format!("{prefix}-{}", &hash[..12]));
        }

        self.client_info = info;
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

    Ok(())
}
