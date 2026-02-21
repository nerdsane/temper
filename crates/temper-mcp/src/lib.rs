//! stdio MCP server exposing Temper Code Mode tools.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use monty::{
    DictPairs, ExcType, ExternalResult, LimitedTracker, MontyException, MontyObject, MontyRun,
    PrintWriter, ResourceLimits, RunProgress,
};
use reqwest::{Method, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::io::{self, AsyncBufReadExt, AsyncWriteExt, BufReader};

use temper_spec::parse_automaton;
use temper_spec::parse_csdl;

const MCP_PROTOCOL_VERSION: &str = "2025-11-05";
const MCP_SERVER_NAME: &str = "temper-mcp";

/// A single app spec source, loaded as `name=specs_dir` from CLI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppConfig {
    pub name: String,
    pub specs_dir: PathBuf,
}

/// Runtime config for the stdio MCP server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpConfig {
    pub temper_port: u16,
    pub apps: Vec<AppConfig>,
}

#[derive(Clone, Debug, Default)]
struct AppMetadata {
    entity_set_to_type: BTreeMap<String, String>,
    entity_type_to_set: BTreeMap<String, String>,
}

#[derive(Clone)]
struct RuntimeContext {
    spec: Value,
    app_metadata: BTreeMap<String, AppMetadata>,
    temper_port: u16,
    http: reqwest::Client,
}

impl RuntimeContext {
    fn from_config(config: &McpConfig) -> Result<Self> {
        let (spec, app_metadata) = load_apps(&config.apps)?;
        Ok(Self {
            spec,
            app_metadata,
            temper_port: config.temper_port,
            http: reqwest::Client::new(),
        })
    }

    fn run_search(&self, code: &str) -> Result<String> {
        let program = wrap_user_code(code);
        let runner = MontyRun::new(program, "search.py", vec!["spec".to_string()], vec![])
            .map_err(|e| anyhow!(format_monty_exception(&e)))?;

        let input = json_to_monty_object(&self.spec);
        let mut print = PrintWriter::Disabled;
        let tracker = LimitedTracker::new(default_limits());
        let progress = runner
            .start(vec![input], tracker, &mut print)
            .map_err(|e| anyhow!(format_monty_exception(&e)))?;

        loop {
            match progress {
                RunProgress::Complete(result) => {
                    let value = monty_object_to_json(&result);
                    return serde_json::to_string(&value)
                        .context("failed to serialize search output as JSON string");
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
    }

    async fn run_execute(&self, code: &str) -> Result<String> {
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

    async fn dispatch_temper_method(
        &self,
        method: &str,
        args: &[MontyObject],
        kwargs: &[(MontyObject, MontyObject)],
    ) -> std::result::Result<Value, String> {
        if !kwargs.is_empty() {
            return Err(format!(
                "temper.{method} does not support keyword arguments in this MCP server"
            ));
        }

        // Dataclass method calls include self as the first arg.
        let args = if args.is_empty() { args } else { &args[1..] };

        match method {
            "list" => {
                let tenant = expect_string_arg(args, 0, "tenant", method)?;
                let entity = expect_string_arg(args, 1, "entity_type", method)?;
                let set = self.resolve_entity_set(&tenant, &entity);

                let body = self
                    .temper_request(&tenant, Method::GET, format!("/tdata/{set}"), None)
                    .await?;
                Ok(body.get("value").cloned().unwrap_or(body))
            }
            "get" => {
                let tenant = expect_string_arg(args, 0, "tenant", method)?;
                let entity = expect_string_arg(args, 1, "entity_type", method)?;
                let entity_id = expect_string_arg(args, 2, "entity_id", method)?;
                let set = self.resolve_entity_set(&tenant, &entity);
                let key = escape_odata_key(&entity_id);

                self.temper_request(&tenant, Method::GET, format!("/tdata/{set}('{key}')"), None)
                    .await
            }
            "create" => {
                let tenant = expect_string_arg(args, 0, "tenant", method)?;
                let entity = expect_string_arg(args, 1, "entity_type", method)?;
                let fields = expect_json_object_arg(args, 2, "fields", method)?;
                let set = self.resolve_entity_set(&tenant, &entity);

                self.temper_request(
                    &tenant,
                    Method::POST,
                    format!("/tdata/{set}"),
                    Some(Value::Object(fields)),
                )
                .await
            }
            "action" => {
                let tenant = expect_string_arg(args, 0, "tenant", method)?;
                let entity = expect_string_arg(args, 1, "entity_type", method)?;
                let entity_id = expect_string_arg(args, 2, "entity_id", method)?;
                let action_name = expect_string_arg(args, 3, "action_name", method)?;
                let body = expect_json_object_arg(args, 4, "body", method)?;
                let set = self.resolve_entity_set(&tenant, &entity);
                let key = escape_odata_key(&entity_id);

                self.temper_request(
                    &tenant,
                    Method::POST,
                    format!("/tdata/{set}('{key}')/Temper.{action_name}"),
                    Some(Value::Object(body)),
                )
                .await
            }
            "patch" => {
                let tenant = expect_string_arg(args, 0, "tenant", method)?;
                let entity = expect_string_arg(args, 1, "entity_type", method)?;
                let entity_id = expect_string_arg(args, 2, "entity_id", method)?;
                let fields = expect_json_object_arg(args, 3, "fields", method)?;
                let set = self.resolve_entity_set(&tenant, &entity);
                let key = escape_odata_key(&entity_id);

                self.temper_request(
                    &tenant,
                    Method::PATCH,
                    format!("/tdata/{set}('{key}')"),
                    Some(Value::Object(fields)),
                )
                .await
            }
            _ => Err(format!(
                "unknown temper method '{method}'. Allowed methods: list, get, create, action, patch"
            )),
        }
    }

    fn resolve_entity_set(&self, tenant: &str, entity_or_set: &str) -> String {
        if let Some(metadata) = self.app_metadata.get(tenant) {
            if metadata.entity_set_to_type.contains_key(entity_or_set) {
                return entity_or_set.to_string();
            }
            if let Some(set) = metadata.entity_type_to_set.get(entity_or_set) {
                return set.clone();
            }
            let plural_guess = format!("{entity_or_set}s");
            if metadata.entity_set_to_type.contains_key(&plural_guess) {
                return plural_guess;
            }
        }
        entity_or_set.to_string()
    }

    async fn temper_request(
        &self,
        tenant: &str,
        method: Method,
        path: String,
        body: Option<Value>,
    ) -> std::result::Result<Value, String> {
        let url = format!("http://127.0.0.1:{}{path}", self.temper_port);
        let mut request = self
            .http
            .request(method, &url)
            .header("X-Tenant-Id", tenant)
            .header("Accept", "application/json");

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
            .map_err(|e| format!("failed to read Temper response body: {e}"))?;

        if status.is_success() {
            if text.trim().is_empty() {
                return Ok(Value::Null);
            }
            return serde_json::from_str(&text).or_else(|_| Ok(Value::String(text)));
        }

        Err(format_http_error(status, &text))
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

fn load_apps(apps: &[AppConfig]) -> Result<(Value, BTreeMap<String, AppMetadata>)> {
    let mut root = Map::<String, Value>::new();
    let mut metadata = BTreeMap::<String, AppMetadata>::new();

    for app in apps {
        let mut entities = Map::<String, Value>::new();

        for path in find_files_with_suffix(&app.specs_dir, ".ioa.toml")? {
            let source = fs::read_to_string(&path)
                .with_context(|| format!("failed to read IOA spec {}", path.display()))?;
            let automaton = parse_automaton(&source)
                .with_context(|| format!("failed to parse IOA spec {}", path.display()))?;
            entities.insert(
                automaton.automaton.name.clone(),
                automaton_to_json(&automaton),
            );
        }

        root.insert(app.name.clone(), json!({ "entities": entities }));

        let csdl_path = app.specs_dir.join("model.csdl.xml");
        if csdl_path.exists() {
            let csdl_xml = fs::read_to_string(&csdl_path)
                .with_context(|| format!("failed to read CSDL {}", csdl_path.display()))?;
            let csdl = parse_csdl(&csdl_xml)
                .with_context(|| format!("failed to parse CSDL {}", csdl_path.display()))?;

            let mut app_meta = AppMetadata::default();
            for schema in &csdl.schemas {
                for container in &schema.entity_containers {
                    for set in &container.entity_sets {
                        let short_type = set
                            .entity_type
                            .rsplit('.')
                            .next()
                            .unwrap_or(&set.entity_type)
                            .to_string();

                        app_meta
                            .entity_set_to_type
                            .insert(set.name.clone(), short_type.clone());
                        app_meta
                            .entity_type_to_set
                            .entry(short_type)
                            .or_insert_with(|| set.name.clone());
                    }
                }
            }

            metadata.insert(app.name.clone(), app_meta);
        }
    }

    Ok((Value::Object(root), metadata))
}

fn automaton_to_json(automaton: &temper_spec::Automaton) -> Value {
    let vars = automaton
        .state
        .iter()
        .map(|var| {
            (
                var.name.clone(),
                json!({
                    "type": var.var_type,
                    "init": parse_var_initial(&var.var_type, &var.initial)
                }),
            )
        })
        .collect::<Map<String, Value>>();

    let actions = automaton
        .actions
        .iter()
        .map(|action| {
            json!({
                "name": action.name,
                "kind": action.kind,
                "from": action.from,
                "to": action.to,
                "guards": action.guard,
                "effects": action.effect,
                "params": action.params,
                "hint": action.hint,
            })
        })
        .collect::<Vec<_>>();

    json!({
        "states": automaton.automaton.states,
        "initial": automaton.automaton.initial,
        "actions": actions,
        "vars": vars,
    })
}

fn parse_var_initial(var_type: &str, raw: &str) -> Value {
    match var_type {
        "bool" => match raw {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            _ => Value::String(raw.to_string()),
        },
        "counter" | "int" | "integer" => raw
            .parse::<i64>()
            .map(Value::from)
            .unwrap_or_else(|_| Value::String(raw.to_string())),
        "list" | "set" if raw == "[]" => Value::Array(vec![]),
        _ => Value::String(raw.to_string()),
    }
}

fn find_files_with_suffix(root: &Path, suffix: &str) -> Result<Vec<PathBuf>> {
    if !root.exists() {
        bail!("specs path does not exist: {}", root.display());
    }

    let mut stack = vec![root.to_path_buf()];
    let mut files = Vec::new();

    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir)
            .with_context(|| format!("failed to read directory {}", dir.display()))?
        {
            let entry = entry
                .with_context(|| format!("failed to read directory entry in {}", dir.display()))?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(suffix))
            {
                files.push(path);
            }
        }
    }

    files.sort();
    Ok(files)
}

fn wrap_user_code(code: &str) -> String {
    let mut out = String::from("async def __temper_user():\n");

    if code.trim().is_empty() {
        out.push_str("    return None\n");
    } else {
        for line in code.lines() {
            out.push_str("    ");
            out.push_str(line);
            out.push('\n');
        }
    }

    out.push_str("\nawait __temper_user()\n");
    out
}

fn default_limits() -> ResourceLimits {
    ResourceLimits::new()
        .max_duration(Duration::from_secs(2))
        .max_memory(64 * 1024 * 1024)
        .max_allocations(250_000)
}

fn format_monty_exception(exception: &MontyException) -> String {
    if exception.traceback().is_empty() {
        exception.summary()
    } else {
        exception.to_string()
    }
}

fn escape_odata_key(key: &str) -> String {
    key.replace('\'', "''")
}

fn expect_string_arg(
    args: &[MontyObject],
    index: usize,
    name: &str,
    method: &str,
) -> std::result::Result<String, String> {
    let value = args.get(index).ok_or_else(|| {
        format!(
            "temper.{method} missing required argument `{name}` at position {}",
            index + 1
        )
    })?;

    String::try_from(value).map_err(|e| {
        format!(
            "temper.{method} expected `{name}` to be string, got {} ({e})",
            value.type_name()
        )
    })
}

fn expect_json_object_arg(
    args: &[MontyObject],
    index: usize,
    name: &str,
    method: &str,
) -> std::result::Result<Map<String, Value>, String> {
    let value = args.get(index).ok_or_else(|| {
        format!(
            "temper.{method} missing required argument `{name}` at position {}",
            index + 1
        )
    })?;

    match monty_object_to_json(value) {
        Value::Object(map) => Ok(map),
        other => Err(format!(
            "temper.{method} expected `{name}` to be a JSON object, got {}",
            other
        )),
    }
}

fn format_http_error(status: StatusCode, body: &str) -> String {
    let details = if body.trim().is_empty() {
        "<empty body>".to_string()
    } else if let Ok(json) = serde_json::from_str::<Value>(body) {
        json.pointer("/error/message")
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .unwrap_or_else(|| json.to_string())
    } else {
        body.to_string()
    };

    format!(
        "HTTP {} {}: {}",
        status.as_u16(),
        status.canonical_reason().unwrap_or(""),
        details
    )
}

fn json_to_monty_object(value: &Value) -> MontyObject {
    match value {
        Value::Null => MontyObject::None,
        Value::Bool(v) => MontyObject::Bool(*v),
        Value::Number(v) => {
            if let Some(i) = v.as_i64() {
                MontyObject::Int(i)
            } else if let Some(u) = v.as_u64() {
                if u <= i64::MAX as u64 {
                    MontyObject::Int(u as i64)
                } else {
                    MontyObject::String(u.to_string())
                }
            } else {
                MontyObject::Float(v.as_f64().unwrap_or_default())
            }
        }
        Value::String(v) => MontyObject::String(v.clone()),
        Value::Array(items) => MontyObject::List(items.iter().map(json_to_monty_object).collect()),
        Value::Object(map) => {
            let pairs = map
                .iter()
                .map(|(k, v)| (MontyObject::String(k.clone()), json_to_monty_object(v)))
                .collect::<Vec<_>>();
            MontyObject::Dict(DictPairs::from(pairs))
        }
    }
}

fn monty_object_to_json(value: &MontyObject) -> Value {
    match value {
        MontyObject::Ellipsis => json!({"$ellipsis": true}),
        MontyObject::None => Value::Null,
        MontyObject::Bool(v) => Value::Bool(*v),
        MontyObject::Int(v) => Value::from(*v),
        MontyObject::BigInt(v) => Value::String(v.to_string()),
        MontyObject::Float(v) => serde_json::Number::from_f64(*v)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        MontyObject::String(v) => Value::String(v.clone()),
        MontyObject::Bytes(bytes) => Value::Array(bytes.iter().copied().map(Value::from).collect()),
        MontyObject::List(items)
        | MontyObject::Tuple(items)
        | MontyObject::Set(items)
        | MontyObject::FrozenSet(items) => {
            Value::Array(items.iter().map(monty_object_to_json).collect())
        }
        MontyObject::NamedTuple {
            field_names,
            values,
            ..
        } => {
            let mut out = Map::new();
            for (field_name, field_value) in field_names.iter().zip(values.iter()) {
                out.insert(field_name.clone(), monty_object_to_json(field_value));
            }
            Value::Object(out)
        }
        MontyObject::Dict(pairs) => {
            let mut out = Map::new();
            for (key, value) in pairs {
                out.insert(monty_key_to_string(key), monty_object_to_json(value));
            }
            Value::Object(out)
        }
        MontyObject::Exception { exc_type, arg } => {
            json!({"$exception": {"type": exc_type.to_string(), "message": arg}})
        }
        MontyObject::Type(t) => Value::String(format!("<class '{}'>", t)),
        MontyObject::BuiltinFunction(name) => Value::String(name.to_string()),
        MontyObject::Path(path) => Value::String(path.clone()),
        MontyObject::Dataclass { name, attrs, .. } => {
            let mut out = Map::new();
            out.insert("$class".to_string(), Value::String(name.clone()));
            for (key, value) in attrs {
                out.insert(monty_key_to_string(key), monty_object_to_json(value));
            }
            Value::Object(out)
        }
        MontyObject::Repr(value) => Value::String(value.clone()),
        MontyObject::Cycle(_, placeholder) => Value::String(placeholder.clone()),
    }
}

fn monty_key_to_string(key: &MontyObject) -> String {
    match key {
        MontyObject::String(s) => s.clone(),
        _ => key.to_string(),
    }
}

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    #[allow(dead_code)]
    jsonrpc: Option<String>,
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Serialize)]
struct JsonRpcError {
    code: i64,
    message: String,
}

#[derive(Debug, Deserialize)]
struct ToolCallParams {
    name: String,
    #[serde(default)]
    arguments: Value,
}

async fn dispatch_json_line(ctx: &RuntimeContext, line: &str) -> Option<Value> {
    let raw: Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(error) => {
            return Some(json_rpc_error(
                None,
                -32700,
                format!("parse error: {error}"),
            ));
        }
    };

    dispatch_json_value(ctx, raw).await
}

async fn dispatch_json_value(ctx: &RuntimeContext, raw: Value) -> Option<Value> {
    let request: JsonRpcRequest = match serde_json::from_value(raw) {
        Ok(value) => value,
        Err(error) => {
            return Some(json_rpc_error(
                None,
                -32600,
                format!("invalid request: {error}"),
            ));
        }
    };

    let id = request.id.clone();

    let result = match request.method.as_str() {
        "initialize" => Ok(json!({
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": {
                "tools": {
                    "listChanged": false
                }
            },
            "serverInfo": {
                "name": MCP_SERVER_NAME,
                "version": env!("CARGO_PKG_VERSION")
            },
            "instructions": "Use search(code) to inspect loaded specs and execute(code) to call Temper APIs."
        })),
        "tools/list" => Ok(json!({ "tools": tool_definitions() })),
        "tools/call" => {
            let params: ToolCallParams = match serde_json::from_value(request.params) {
                Ok(value) => value,
                Err(error) => {
                    return Some(json_rpc_error(
                        id,
                        -32602,
                        format!("invalid tools/call params: {error}"),
                    ));
                }
            };

            let code = match params.arguments.get("code").and_then(Value::as_str) {
                Some(code) => code,
                None => {
                    return Some(json_rpc_error(
                        id,
                        -32602,
                        "tools/call missing required `arguments.code` string".to_string(),
                    ));
                }
            };

            let tool_result = match params.name.as_str() {
                "search" => ctx.run_search(code),
                "execute" => ctx.run_execute(code).await,
                other => Err(anyhow!(format!("unknown tool '{other}'"))),
            };

            Ok(match tool_result {
                Ok(text) => json!({
                    "content": [{"type": "text", "text": text}],
                    "isError": false
                }),
                Err(error) => json!({
                    "content": [{"type": "text", "text": error.to_string()}],
                    "isError": true
                }),
            })
        }
        "ping" => Ok(json!({})),
        "initialized" | "notifications/initialized" => {
            // Notification-style methods intentionally produce no response.
            return None;
        }
        method => Err(anyhow!(format!("method not found: {method}"))),
    };

    // Notifications (no id) do not require a response.
    let response_id = id?;

    Some(match result {
        Ok(payload) => json!({
            "jsonrpc": "2.0",
            "id": response_id,
            "result": payload,
        }),
        Err(error) => {
            let (code, message) = if error.to_string().starts_with("method not found") {
                (-32601, error.to_string())
            } else {
                (-32602, error.to_string())
            };
            json_rpc_error(Some(response_id), code, message)
        }
    })
}

fn json_rpc_error(id: Option<Value>, code: i64, message: String) -> Value {
    let error = JsonRpcError { code, message };
    json!({
        "jsonrpc": "2.0",
        "id": id.unwrap_or(Value::Null),
        "error": error,
    })
}

fn tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "search",
            "description": "Run sandboxed Python against loaded IOA specs. The code receives `spec` and must `return` JSON-serializable data.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "code": {
                        "type": "string",
                        "description": "Python snippet. Use `return ...` as the final statement."
                    }
                },
                "required": ["code"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "execute",
            "description": "Run sandboxed Python against live Temper API. The code receives `temper` with methods: list/get/create/action/patch.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "code": {
                        "type": "string",
                        "description": "Python snippet. Use async calls like `await temper.list(...)` and `return ...`."
                    }
                },
                "required": ["code"],
                "additionalProperties": false
            }
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeMap;

    use axum::Router;
    use temper_runtime::ActorSystem;
    use temper_server::ServerState;
    use tempfile::tempdir;
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;

    fn write_temp_specs() -> tempfile::TempDir {
        let dir = tempdir().expect("tempdir");
        let specs = dir.path();

        fs::write(
            specs.join("order.ioa.toml"),
            include_str!("../../../test-fixtures/specs/order.ioa.toml"),
        )
        .expect("write ioa");

        fs::write(
            specs.join("model.csdl.xml"),
            include_str!("../../../test-fixtures/specs/model.csdl.xml"),
        )
        .expect("write csdl");

        dir
    }

    fn app(name: &str, specs_dir: &Path) -> AppConfig {
        AppConfig {
            name: name.to_string(),
            specs_dir: specs_dir.to_path_buf(),
        }
    }

    async fn rpc(ctx: &RuntimeContext, request: Value) -> Value {
        dispatch_json_value(ctx, request)
            .await
            .expect("response expected")
    }

    fn call_tool_request(id: i64, tool: &str, code: &str) -> Value {
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {
                "name": tool,
                "arguments": {
                    "code": code
                }
            }
        })
    }

    fn tool_text(response: &Value) -> (&str, bool) {
        let is_error = response
            .pointer("/result/isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let text = response
            .pointer("/result/content/0/text")
            .and_then(Value::as_str)
            .unwrap_or("");
        (text, is_error)
    }

    async fn start_test_temper_server() -> (u16, oneshot::Sender<()>) {
        let csdl_xml = include_str!("../../../test-fixtures/specs/model.csdl.xml");
        let csdl = parse_csdl(csdl_xml).expect("parse csdl");

        let mut ioa_sources = BTreeMap::new();
        ioa_sources.insert(
            "Order".to_string(),
            include_str!("../../../test-fixtures/specs/order.ioa.toml").to_string(),
        );

        let state = ServerState::with_specs(
            ActorSystem::new("temper-mcp-tests"),
            csdl,
            csdl_xml.to_string(),
            ioa_sources,
        );

        let router: Router = temper_server::build_router(state);
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            let server = axum::serve(listener, router)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await;
            if let Err(err) = server {
                panic!("test server failed: {err}");
            }
        });

        (port, shutdown_tx)
    }

    #[tokio::test]
    async fn mcp_initialize_handshake() {
        let ctx = RuntimeContext::from_config(&McpConfig {
            temper_port: 3001,
            apps: vec![],
        })
        .expect("ctx");

        let response = rpc(
            &ctx,
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-11-05"
                }
            }),
        )
        .await;

        assert_eq!(response["result"]["protocolVersion"], MCP_PROTOCOL_VERSION);
        assert_eq!(response["result"]["serverInfo"]["name"], MCP_SERVER_NAME);
        assert!(response["result"]["capabilities"]["tools"].is_object());
    }

    #[tokio::test]
    async fn search_returns_filtered_spec_data() {
        let tmp = write_temp_specs();
        let ctx = RuntimeContext::from_config(&McpConfig {
            temper_port: 3001,
            apps: vec![app("demo", tmp.path())],
        })
        .expect("ctx");

        let response = rpc(
            &ctx,
            call_tool_request(
                2,
                "search",
                "return [a['name'] for e in spec['demo']['entities'].values() for a in e['actions'] if a['name'] == 'SubmitOrder']",
            ),
        )
        .await;

        let (text, is_error) = tool_text(&response);
        assert!(!is_error, "search should succeed: {response:#}");
        let parsed: Value = serde_json::from_str(text).expect("tool text should be json");
        assert_eq!(parsed, json!(["SubmitOrder"]));
    }

    #[tokio::test]
    async fn execute_creates_entity_and_reads_it_back() {
        let tmp = write_temp_specs();
        let (port, shutdown) = start_test_temper_server().await;
        let ctx = RuntimeContext::from_config(&McpConfig {
            temper_port: port,
            apps: vec![app("demo", tmp.path())],
        })
        .expect("ctx");

        let response = rpc(
            &ctx,
            call_tool_request(
                3,
                "execute",
                r#"
created = await temper.create('demo', 'Orders', {'id': 'mcp-create-1', 'customer': 'Alice'})
fetched = await temper.get('demo', 'Orders', 'mcp-create-1')
return fetched['fields']['customer']
"#,
            ),
        )
        .await;

        let _ = shutdown.send(());

        let (text, is_error) = tool_text(&response);
        assert!(!is_error, "execute should succeed: {response:#}");
        let parsed: Value = serde_json::from_str(text).expect("tool text should be json");
        assert_eq!(parsed, Value::String("Alice".to_string()));
    }

    #[tokio::test]
    async fn execute_invalid_action_returns_409_cleanly() {
        let tmp = write_temp_specs();
        let (port, shutdown) = start_test_temper_server().await;
        let ctx = RuntimeContext::from_config(&McpConfig {
            temper_port: port,
            apps: vec![app("demo", tmp.path())],
        })
        .expect("ctx");

        let response = rpc(
            &ctx,
            call_tool_request(
                4,
                "execute",
                r#"
await temper.create('demo', 'Orders', {'id': 'mcp-bad-action-1'})
await temper.action('demo', 'Orders', 'mcp-bad-action-1', 'ShipOrder', {'Reason': 'invalid from draft'})
return 'unreachable'
"#,
            ),
        )
        .await;

        let _ = shutdown.send(());

        let (text, is_error) = tool_text(&response);
        assert!(is_error, "execute should return tool error: {response:#}");
        assert!(
            text.contains("409"),
            "expected 409 in error message: {text}"
        );
    }

    #[tokio::test]
    async fn sandbox_blocks_filesystem_access() {
        let tmp = write_temp_specs();
        let ctx = RuntimeContext::from_config(&McpConfig {
            temper_port: 3001,
            apps: vec![app("demo", tmp.path())],
        })
        .expect("ctx");

        let response = rpc(
            &ctx,
            call_tool_request(5, "search", "import os\nreturn open('/etc/passwd').read()"),
        )
        .await;

        let (text, is_error) = tool_text(&response);
        assert!(is_error, "search should fail for filesystem access");
        assert!(
            text.contains("blocked OS access")
                || text.contains("sandbox")
                || text.contains("NameError")
                || text.contains("open"),
            "expected sandbox error, got: {text}"
        );
    }

    #[tokio::test]
    async fn execute_supports_compound_operation() {
        let tmp = write_temp_specs();
        let (port, shutdown) = start_test_temper_server().await;
        let ctx = RuntimeContext::from_config(&McpConfig {
            temper_port: port,
            apps: vec![app("demo", tmp.path())],
        })
        .expect("ctx");

        let response = rpc(
            &ctx,
            call_tool_request(
                6,
                "execute",
                r#"
await temper.create('demo', 'Orders', {'id': 'mcp-compound-1'})
await temper.action('demo', 'Orders', 'mcp-compound-1', 'CancelOrder', {'Reason': 'cancel test'})
fetched = await temper.get('demo', 'Orders', 'mcp-compound-1')
return fetched['status']
"#,
            ),
        )
        .await;

        let _ = shutdown.send(());

        let (text, is_error) = tool_text(&response);
        assert!(!is_error, "compound execute should succeed: {response:#}");
        let parsed: Value = serde_json::from_str(text).expect("tool text should be json");
        assert_eq!(parsed, Value::String("Cancelled".to_string()));
    }
}
