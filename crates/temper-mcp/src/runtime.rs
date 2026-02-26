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
use super::sandbox::{default_limits, expect_string_arg, format_monty_exception, wrap_user_code};
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
    pub(super) principal_id: Option<String>,
}

impl RuntimeContext {
    pub(super) fn from_config(config: &McpConfig) -> Result<Self> {
        let (spec, app_metadata) = load_apps(&config.apps)?;
        Ok(Self {
            spec,
            app_metadata,
            temper_port: config.temper_port,
            http: reqwest::Client::new(),
            principal_id: config.principal_id.clone(),
        })
    }

    pub(super) fn run_search(&self, code: &str) -> Result<String> {
        let program = wrap_user_code(code);
        let runner = MontyRun::new(program, "search.py", vec!["spec".to_string()], vec![])
            .map_err(|e| anyhow!(format_monty_exception(&e)))?;

        let spec_object = MontyObject::Dataclass {
            name: "TemperSpec".to_string(),
            type_id: 10,
            field_names: vec![],
            attrs: DictPairs::from(Vec::<(MontyObject, MontyObject)>::new()),
            frozen: true,
        };

        let mut print = PrintWriter::Disabled;
        let tracker = LimitedTracker::new(default_limits());
        let mut progress = runner
            .start(vec![spec_object], tracker, &mut print)
            .map_err(|e| anyhow!(format_monty_exception(&e)))?;

        let mut pending_results: BTreeMap<u32, ExternalResult> = BTreeMap::new();

        loop {
            match progress {
                RunProgress::Complete(result) => {
                    let value = monty_object_to_json(&result);
                    return serde_json::to_string(&value)
                        .context("failed to serialize search output as JSON string");
                }
                RunProgress::FunctionCall {
                    function_name,
                    args,
                    call_id,
                    method_call,
                    state,
                    ..
                } => {
                    if !method_call {
                        bail!(
                            "search sandbox denied external function call '{}'. Only spec.<method> is allowed",
                            function_name
                        );
                    }

                    let result = self.dispatch_spec_method(&function_name, &args);
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
                            "search sandbox is waiting on unresolved calls: {:?}",
                            state.pending_call_ids()
                        );
                    }

                    progress = state
                        .resume(ready, &mut print)
                        .map_err(|e| anyhow!(format_monty_exception(&e)))?;
                }
                RunProgress::OsCall { function, .. } => {
                    bail!(
                        "search sandbox blocked OS access ({function:?}). Filesystem/network/env access is disabled"
                    );
                }
            }
        }
    }

    /// Dispatch a method call on the `spec` Dataclass object.
    fn dispatch_spec_method(
        &self,
        method: &str,
        args: &[MontyObject],
    ) -> std::result::Result<Value, String> {
        // Dataclass method calls include self as args[0].
        let args = if args.is_empty() { args } else { &args[1..] };

        match method {
            "tenants" => {
                let tenants: Vec<String> = self
                    .spec
                    .as_object()
                    .map(|obj| obj.keys().cloned().collect())
                    .unwrap_or_default();
                Ok(serde_json::json!(tenants))
            }
            "entities" => {
                let tenant = expect_string_arg(args, 0, "tenant", "spec.entities")?;
                let entities: Vec<String> = self
                    .spec
                    .get(&tenant)
                    .and_then(|v| v.get("entities"))
                    .and_then(Value::as_object)
                    .map(|obj| obj.keys().cloned().collect())
                    .unwrap_or_default();
                Ok(serde_json::json!(entities))
            }
            "describe" => {
                let tenant = expect_string_arg(args, 0, "tenant", "spec.describe")?;
                let entity_type = expect_string_arg(args, 1, "entity_type", "spec.describe")?;
                self.spec
                    .get(&tenant)
                    .and_then(|v| v.get("entities"))
                    .and_then(|v| v.get(&entity_type))
                    .cloned()
                    .ok_or_else(|| format!("No spec found for {tenant}/{entity_type}"))
            }
            "actions" => {
                let tenant = expect_string_arg(args, 0, "tenant", "spec.actions")?;
                let entity_type = expect_string_arg(args, 1, "entity_type", "spec.actions")?;
                let actions = self
                    .spec
                    .get(&tenant)
                    .and_then(|v| v.get("entities"))
                    .and_then(|v| v.get(&entity_type))
                    .and_then(|v| v.get("actions"))
                    .cloned()
                    .unwrap_or(serde_json::json!([]));
                Ok(actions)
            }
            "actions_from" => {
                let tenant = expect_string_arg(args, 0, "tenant", "spec.actions_from")?;
                let entity_type = expect_string_arg(args, 1, "entity_type", "spec.actions_from")?;
                let state = expect_string_arg(args, 2, "state", "spec.actions_from")?;
                let filtered: Vec<Value> = self
                    .spec
                    .get(&tenant)
                    .and_then(|v| v.get("entities"))
                    .and_then(|v| v.get(&entity_type))
                    .and_then(|v| v.get("actions"))
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter(|a| {
                                // `from` can be a string or an array of strings
                                match a.get("from") {
                                    Some(Value::String(s)) => s == &state,
                                    Some(Value::Array(arr)) => {
                                        arr.iter().any(|v| v.as_str() == Some(state.as_str()))
                                    }
                                    _ => false,
                                }
                            })
                            .cloned()
                            .collect()
                    })
                    .unwrap_or_default();
                Ok(serde_json::json!(filtered))
            }
            "raw" => {
                let tenant = expect_string_arg(args, 0, "tenant", "spec.raw")?;
                let entity_type = expect_string_arg(args, 1, "entity_type", "spec.raw")?;
                self.spec
                    .get(&tenant)
                    .and_then(|v| v.get("entities"))
                    .and_then(|v| v.get(&entity_type))
                    .cloned()
                    .ok_or_else(|| format!("No spec found for {tenant}/{entity_type}"))
            }
            _ => Err(format!(
                "unknown spec method '{method}'. Available: tenants, entities, describe, actions, actions_from, raw"
            )),
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
