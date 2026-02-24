//! MCP runtime context and stdio server loop.

use std::collections::BTreeMap;

use anyhow::{Context, Result, anyhow, bail};
use monty::{
    DictPairs, ExcType, ExternalResult, LimitedTracker, MontyException, MontyObject, MontyRun,
    PrintWriter, RunProgress,
};
use serde_json::Value;
use tokio::io::{self, AsyncBufReadExt, AsyncWriteExt, BufReader};

use super::McpConfig;
use super::convert::{json_to_monty_object, monty_object_to_json};
use super::protocol::dispatch_json_line;
use super::sandbox::{default_limits, format_monty_exception, wrap_user_code};
use super::spec_loader::load_apps;

#[derive(Clone, Debug, Default)]
pub(super) struct AppMetadata {
    pub(super) entity_set_to_type: BTreeMap<String, String>,
    pub(super) entity_type_to_set: BTreeMap<String, String>,
}

#[derive(Clone)]
pub(super) struct RuntimeContext {
    pub(super) spec: Value,
    pub(super) app_metadata: BTreeMap<String, AppMetadata>,
    pub(super) temper_port: u16,
    pub(super) http: reqwest::Client,
}

impl RuntimeContext {
    pub(super) fn from_config(config: &McpConfig) -> Result<Self> {
        let (spec, app_metadata) = load_apps(&config.apps)?;
        Ok(Self {
            spec,
            app_metadata,
            temper_port: config.temper_port,
            http: reqwest::Client::new(),
        })
    }

    pub(super) fn run_search(&self, code: &str) -> Result<String> {
        let program = wrap_user_code(code);
        let runner = MontyRun::new(program, "search.py", vec!["spec".to_string()], vec![])
            .map_err(|e| anyhow!(format_monty_exception(&e)))?;

        let input = json_to_monty_object(&self.spec);
        let mut print = PrintWriter::Disabled;
        let tracker = LimitedTracker::new(default_limits());
        let progress = runner
            .start(vec![input], tracker, &mut print)
            .map_err(|e| anyhow!(format_monty_exception(&e)))?;

        match progress {
            RunProgress::Complete(result) => {
                let value = monty_object_to_json(&result);
                serde_json::to_string(&value)
                    .context("failed to serialize search output as JSON string")
            }
            RunProgress::FunctionCall { function_name, .. } => {
                bail!(
                    "search sandbox denied external function call '{}'. Only `spec` is available",
                    function_name
                );
            }
            RunProgress::ResolveFutures(_) => {
                bail!("search sandbox entered unexpected async pending state");
            }
            RunProgress::OsCall { function, .. } => {
                bail!(
                    "search sandbox blocked OS access ({function:?}). Filesystem/network/env access is disabled"
                );
            }
        }
    }

    pub(super) async fn run_execute(&self, code: &str) -> Result<String> {
        let program = wrap_user_code(code);
        let runner = MontyRun::new(program, "execute.py", vec!["temper".to_string()], vec![])
            .map_err(|e| anyhow!(format_monty_exception(&e)))?;

        let temper_object = MontyObject::Dataclass {
            name: "Temper".to_string(),
            type_id: 1,
            field_names: vec![],
            attrs: DictPairs::from(Vec::<(MontyObject, MontyObject)>::new()),
            frozen: true,
        };

        let mut print = PrintWriter::Disabled;
        let tracker = LimitedTracker::new(default_limits());
        let mut progress = runner
            .start(vec![temper_object], tracker, &mut print)
            .map_err(|e| anyhow!(format_monty_exception(&e)))?;

        let mut pending_results: BTreeMap<u32, ExternalResult> = BTreeMap::new();

        loop {
            match progress {
                RunProgress::Complete(result) => {
                    let value = monty_object_to_json(&result);
                    return serde_json::to_string(&value)
                        .context("failed to serialize execute output as JSON string");
                }
                RunProgress::FunctionCall {
                    function_name,
                    args,
                    kwargs,
                    call_id,
                    method_call,
                    state,
                    ..
                } => {
                    if !method_call {
                        bail!(
                            "execute sandbox denied external function '{}': only temper.<method> is allowed",
                            function_name
                        );
                    }

                    let result = self
                        .dispatch_temper_method(&function_name, &args, &kwargs)
                        .await;
                    let ext_result = match result {
                        Ok(value) => ExternalResult::Return(json_to_monty_object(&value)),
                        Err(message) => ExternalResult::Error(MontyException::new(
                            ExcType::RuntimeError,
                            Some(message),
                        )),
                    };

                    pending_results.insert(call_id, ext_result);
                    progress = state
                        .run_pending(&mut print)
                        .map_err(|e| anyhow!(format_monty_exception(&e)))?;
                }
                RunProgress::ResolveFutures(state) => {
                    let mut ready: Vec<(u32, ExternalResult)> = Vec::new();
                    for call_id in state.pending_call_ids() {
                        if let Some(result) = pending_results.remove(call_id) {
                            ready.push((*call_id, result));
                        }
                    }

                    if ready.is_empty() {
                        bail!(
                            "execute sandbox is waiting on unresolved external calls: {:?}",
                            state.pending_call_ids()
                        );
                    }

                    progress = state
                        .resume(ready, &mut print)
                        .map_err(|e| anyhow!(format_monty_exception(&e)))?;
                }
                RunProgress::OsCall { function, .. } => {
                    bail!(
                        "execute sandbox blocked OS access ({function:?}). Filesystem/network/env access is disabled"
                    );
                }
            }
        }
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
