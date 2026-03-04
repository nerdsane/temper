//! MCP runtime context and stdio server loop.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use monty::{
    DictPairs, ExcType, ExternalResult, LimitedTracker, MontyException, MontyObject, MontyRun,
    PrintWriter, RunProgress,
};
use serde_json::Value;
use tokio::io::{self, AsyncBufReadExt, AsyncWriteExt, BufReader};

use super::McpConfig;
use super::protocol::dispatch_json_line;
use super::sandbox::{default_limits, expect_string_arg, format_monty_exception, wrap_user_code};
use super::spec_loader::load_apps;
use temper_sandbox::convert::{json_to_monty_object, monty_object_to_json};

#[derive(Clone, Debug, Default)]
pub(crate) struct AppMetadata {
    pub(crate) entity_set_to_type: BTreeMap<String, String>,
    pub(crate) entity_type_to_set: BTreeMap<String, String>,
}

#[derive(Clone)]
pub(crate) struct RuntimeContext {
    pub(crate) spec: Value,
    pub(crate) app_metadata: BTreeMap<String, AppMetadata>,
    /// Port of the Temper HTTP server. Set at construction when an explicit
    /// port is provided, or later by `start_server` in self-contained mode.
    pub(crate) server_port: Arc<std::sync::OnceLock<u16>>,
    /// App configurations (used to build `--app` args when spawning server).
    pub(crate) apps: Vec<crate::AppConfig>,
    /// Path to the temper binary (for spawning `temper serve`).
    pub(crate) binary_path: Option<std::path::PathBuf>,
    pub(crate) http: reqwest::Client,
    pub(crate) principal_id: Option<String>,
}

impl RuntimeContext {
    pub(super) fn from_config(config: &McpConfig) -> Result<Self> {
        let (spec, app_metadata) = load_apps(&config.apps)?;
        let server_port = Arc::new(std::sync::OnceLock::new());
        if let Some(p) = config.temper_port {
            let _ = server_port.set(p);
        }
        Ok(Self {
            spec,
            app_metadata,
            server_port,
            apps: config.apps.clone(),
            binary_path: std::env::current_exe().ok(),
            http: reqwest::Client::new(),
            principal_id: config.principal_id.clone(),
        })
    }

    pub(crate) fn run_search(&self, code: &str) -> Result<String> {
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

    pub(crate) async fn run_execute(&self, code: &str) -> Result<String> {
        let http = self.http.clone();
        let server_port = self.server_port.clone();
        let app_metadata = self.app_metadata.clone();
        let principal_id = self.principal_id.clone();
        let binary_path = self.binary_path.clone();
        let spec = self.spec.clone();
        let apps = self.apps.clone();

        temper_sandbox::runner::run_sandbox(
            code,
            "execute.py",
            &[("temper", "Temper", 1)],
            |function_name: String, args: Vec<MontyObject>, kwargs: Vec<(MontyObject, MontyObject)>| {
                let http = http.clone();
                let server_port = server_port.clone();
                let app_metadata = app_metadata.clone();
                let principal_id = principal_id.clone();
                let binary_path = binary_path.clone();
                let spec = spec.clone();
                let apps = apps.clone();
                async move {
                    if !kwargs.is_empty() {
                        return Err(format!(
                            "temper.{function_name} does not support keyword arguments in this MCP server"
                        ));
                    }

                    // Strip self arg
                    let args = if args.is_empty() { &args[..] } else { &args[1..] };

                    // Handle MCP-only methods before tenant extraction
                    if function_name == "start_server" {
                        return handle_start_server(&server_port, &binary_path, &apps).await;
                    }
                    if function_name == "show_spec" {
                        let tenant = temper_sandbox::helpers::expect_string_arg(args, 0, "tenant", &function_name)?;
                        let entity_type = temper_sandbox::helpers::expect_string_arg(args, 1, "entity_type", &function_name)?;
                        return spec
                            .get(&tenant)
                            .and_then(|v| v.get("entities"))
                            .and_then(|v| v.get(&entity_type))
                            .cloned()
                            .ok_or_else(|| format!("No spec found for {tenant}/{entity_type}"));
                    }

                    // Extract tenant from args[0] for MCP multi-tenant dispatch
                    let tenant = temper_sandbox::helpers::expect_string_arg(args, 0, "tenant", &function_name)?;
                    let remaining = if args.len() > 1 { &args[1..] } else { &[] };

                    let port = server_port.get().ok_or_else(|| {
                        "Server not running. Call `await temper.start_server()` first, \
                         or restart MCP with --port to connect to an existing server."
                            .to_string()
                    })?;
                    let base_url = format!("http://127.0.0.1:{port}");

                    let metadata = app_metadata.get(&tenant);
                    let resolver = |entity_or_set: &str| -> String {
                        if let Some(meta) = metadata {
                            if meta.entity_set_to_type.contains_key(entity_or_set) {
                                return entity_or_set.to_string();
                            }
                            if let Some(set) = meta.entity_type_to_set.get(entity_or_set) {
                                return set.clone();
                            }
                            let plural_guess = format!("{entity_or_set}s");
                            if meta.entity_set_to_type.contains_key(&plural_guess) {
                                return plural_guess;
                            }
                        }
                        entity_or_set.to_string()
                    };

                    temper_sandbox::dispatch::dispatch_temper_method(
                        &http,
                        &base_url,
                        &tenant,
                        principal_id.as_deref(),
                        &function_name,
                        remaining,
                        &kwargs,
                        Some(&resolver),
                        binary_path.as_deref(),
                    )
                    .await
                }
            },
        )
        .await
    }
}

/// Handle the `start_server` MCP-only method (spawns `temper serve`).
async fn handle_start_server(
    server_port: &Arc<std::sync::OnceLock<u16>>,
    binary_path: &Option<std::path::PathBuf>,
    apps: &[crate::AppConfig],
) -> std::result::Result<Value, String> {
    use std::process::Stdio;
    use tokio::io::AsyncBufReadExt as _;

    if let Some(&port) = server_port.get() {
        let app_names: Vec<String> = apps.iter().map(|a| a.name.clone()).collect();
        return Ok(serde_json::json!({
            "port": port,
            "storage": "memory",
            "apps": app_names,
            "status": "already_running"
        }));
    }

    let binary = binary_path.clone().ok_or_else(|| {
        "Cannot determine temper binary path. \
         Ensure the MCP server is running from the temper CLI."
            .to_string()
    })?;

    let mut cmd = tokio::process::Command::new(&binary);
    cmd.arg("serve")
        .arg("--port")
        .arg("0")
        .arg("--storage")
        .arg("turso")
        .arg("--observe");
    for a in apps {
        cmd.arg("--app")
            .arg(format!("{}={}", a.name, a.specs_dir.display()));
    }
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::inherit());
    cmd.kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Failed to spawn temper serve: {e}"))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "No stdout from child process".to_string())?;
    let mut lines = tokio::io::BufReader::new(stdout).lines();

    let mut observe_url = String::new();
    let port = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while let Some(line) = lines.next_line().await.map_err(|e| e.to_string())? {
            eprintln!("[temper serve] {line}");
            let trimmed = line.trim();
            if trimmed.starts_with("Observe UI: ") {
                observe_url = trimmed
                    .strip_prefix("Observe UI: ")
                    .unwrap_or("")
                    .to_string();
            }
            if let Some(rest) = line.strip_prefix("Listening on http://0.0.0.0:") {
                return rest
                    .trim()
                    .parse::<u16>()
                    .map_err(|e| format!("invalid port: {e}"));
            }
        }
        Err::<u16, String>("Server exited before reporting listening port".to_string())
    })
    .await
    .map_err(|_| "Timed out waiting for server to start (30s)".to_string())??;

    server_port
        .set(port)
        .map_err(|_| "Server port already set (race condition)".to_string())?;

    if observe_url.is_empty() {
        observe_url = format!("http://localhost:{}", port + 1);
    }

    tokio::spawn(async move {
        while let Ok(Some(line)) = lines.next_line().await {
            eprintln!("[temper serve] {line}");
        }
        let _ = child.wait().await;
    });

    let app_names: Vec<String> = apps.iter().map(|a| a.name.clone()).collect();
    Ok(serde_json::json!({
        "port": port,
        "storage": "turso",
        "observe_url": observe_url,
        "apps": app_names,
        "status": "started",
        "note": "Observe UI may be starting at the observe_url. Use it to approve/deny agent decisions."
    }))
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
