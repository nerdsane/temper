//! MCP runtime context and stdio server loop.

use anyhow::{Result, bail};
use monty::MontyObject;
use tokio::io::{self, AsyncBufReadExt, AsyncWriteExt, BufReader};

use super::McpConfig;
use super::protocol::dispatch_json_line;

/// Thin-client runtime context for the MCP server.
///
/// Connects to an already-running Temper server via `--port` (local) or
/// `--url` (remote). Does not spawn servers, parse local specs, or manage
/// any infrastructure.
#[derive(Clone)]
pub(crate) struct RuntimeContext {
    pub(crate) base_url: String,
    pub(crate) http: reqwest::Client,
    pub(crate) principal_id: Option<String>,
    pub(crate) api_key: Option<String>,
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
            principal_id: config.principal_id.clone(),
            api_key: config
                .api_key
                .clone()
                .or_else(|| std::env::var("TEMPER_API_KEY").ok()), // determinism-ok: startup config
        })
    }

    pub(crate) async fn run_execute(&self, code: &str) -> Result<String> {
        let http = self.http.clone();
        let base_url = self.base_url.clone();
        let principal_id = self.principal_id.clone();
        let api_key = self.api_key.clone();

        temper_sandbox::runner::run_sandbox(
            code,
            "execute.py",
            &[("temper", "Temper", 1)],
            |function_name: String,
             args: Vec<MontyObject>,
             kwargs: Vec<(MontyObject, MontyObject)>| {
                let http = http.clone();
                let base_url = base_url.clone();
                let principal_id = principal_id.clone();
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
                        principal_id: principal_id.as_deref(),
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
    let ctx = RuntimeContext::from_config(&config)?;
    let stdin = BufReader::new(io::stdin());
    let mut lines = stdin.lines();
    let mut stdout = io::stdout();

    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        if let Some(response) = dispatch_json_line(&ctx, line).await {
            let encoded = serde_json::to_string(&response)?;
            stdout.write_all(encoded.as_bytes()).await?;
            stdout.write_all(b"\n").await?;
            stdout.flush().await?;
        }
    }

    Ok(())
}
