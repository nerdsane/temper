//! Temper tool dispatch for MCP execute sandbox calls.

use std::process::Stdio;

use monty::MontyObject;
use reqwest::Method;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::AsyncBufReadExt;

use super::runtime::RuntimeContext;
use super::sandbox::{
    escape_odata_key, expect_json_object_arg, expect_string_arg, format_authz_denied,
    format_http_error, optional_string_arg,
};

impl RuntimeContext {
    pub(crate) async fn dispatch_temper_method(
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
            // --- Developer methods ---
            "show_spec" => {
                let tenant = expect_string_arg(args, 0, "tenant", method)?;
                let entity_type = expect_string_arg(args, 1, "entity_type", method)?;
                self.spec
                    .get(&tenant)
                    .and_then(|v| v.get("entities"))
                    .and_then(|v| v.get(&entity_type))
                    .cloned()
                    .ok_or_else(|| format!("No spec found for {tenant}/{entity_type}"))
            }
            "submit_specs" => {
                let tenant = expect_string_arg(args, 0, "tenant", method)?;
                let specs = expect_json_object_arg(args, 1, "specs", method)?;
                let payload = serde_json::json!({ "tenant": tenant, "specs": specs });
                self.temper_request(
                    &tenant,
                    Method::POST,
                    "/api/specs/load-inline".to_string(),
                    Some(payload),
                )
                .await
            }
            "get_policies" => {
                let tenant = expect_string_arg(args, 0, "tenant", method)?;
                self.temper_request(
                    &tenant,
                    Method::GET,
                    format!("/api/tenants/{tenant}/policies"),
                    None,
                )
                .await
            }
            // --- Lifecycle ---
            "start_server" => {
                // If already started, return current info.
                if let Some(&port) = self.server_port.get() {
                    let app_names: Vec<String> = self.apps.iter().map(|a| a.name.clone()).collect();
                    return Ok(serde_json::json!({
                        "port": port,
                        "storage": "memory",
                        "apps": app_names,
                        "status": "already_running"
                    }));
                }

                let binary = self.binary_path.clone().ok_or_else(|| {
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
                for a in &self.apps {
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

                // Read lines until we find the listening port and observe URL.
                let mut observe_url = String::new();
                let port = tokio::time::timeout(std::time::Duration::from_secs(30), async {
                    while let Some(line) = lines.next_line().await.map_err(|e| e.to_string())? {
                        eprintln!("[temper serve] {line}");
                        // Capture observe URL (printed before listening line)
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

                self.server_port
                    .set(port)
                    .map_err(|_| "Server port already set (race condition)".to_string())?;

                // Fallback if observe URL wasn't detected
                if observe_url.is_empty() {
                    observe_url = format!("http://localhost:{}", port + 1);
                }

                // Keep child alive and drain remaining stdout in background.
                tokio::spawn(async move {
                    while let Ok(Some(line)) = lines.next_line().await {
                        eprintln!("[temper serve] {line}");
                    }
                    let _ = child.wait().await;
                });

                let app_names: Vec<String> = self.apps.iter().map(|a| a.name.clone()).collect();
                Ok(serde_json::json!({
                    "port": port,
                    "storage": "turso",
                    "observe_url": observe_url,
                    "apps": app_names,
                    "status": "started",
                    "note": "Observe UI may be starting at the observe_url. Use it to approve/deny agent decisions."
                }))
            }
            // --- Governance methods ---
            "get_decisions" => {
                let tenant = expect_string_arg(args, 0, "tenant", method)?;
                let status = args.get(1).and_then(|a| String::try_from(a).ok());
                let path = match status {
                    Some(s) => format!("/api/tenants/{tenant}/decisions?status={s}"),
                    None => format!("/api/tenants/{tenant}/decisions"),
                };
                self.temper_governance_request(&tenant, Method::GET, path, None)
                    .await
            }
            "get_decision_status" => {
                let tenant = expect_string_arg(args, 0, "tenant", method)?;
                let decision_id = expect_string_arg(args, 1, "decision_id", method)?;
                let result = self
                    .temper_governance_request(
                        &tenant,
                        Method::GET,
                        format!("/api/tenants/{tenant}/decisions"),
                        None,
                    )
                    .await?;
                // Search through the decisions array for the matching ID.
                if let Some(decisions) = result.get("decisions").and_then(Value::as_array) {
                    for d in decisions {
                        if d.get("id").and_then(Value::as_str) == Some(&decision_id) {
                            let status =
                                d.get("status").and_then(Value::as_str).unwrap_or("unknown");
                            return Ok(serde_json::json!({
                                "decision_id": decision_id,
                                "status": status,
                                "decision": d,
                            }));
                        }
                    }
                }
                Ok(serde_json::json!({
                    "decision_id": decision_id,
                    "status": "not_found",
                }))
            }
            "poll_decision" => {
                let tenant = expect_string_arg(args, 0, "tenant", method)?;
                let decision_id = expect_string_arg(args, 1, "decision_id", method)?;
                let start = std::time::Instant::now(); // determinism-ok: wall-clock for MCP timeout only
                loop {
                    let result = self
                        .temper_governance_request(
                            &tenant,
                            Method::GET,
                            format!("/api/tenants/{tenant}/decisions"),
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
                             Ask the human to approve via the Observe UI or `temper decide` CLI, then retry."
                        ));
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(2000)).await;
                }
            }
            "upload_wasm" => {
                let tenant = expect_string_arg(args, 0, "tenant", method)?;
                let module_name = expect_string_arg(args, 1, "module_name", method)?;
                let wasm_path = expect_string_arg(args, 2, "wasm_path", method)?;

                let bytes = tokio::fs::read(&wasm_path)
                    .await
                    .map_err(|e| format!("failed to read WASM file '{}': {}", wasm_path, e))?;

                self.temper_request_bytes(
                    &tenant,
                    reqwest::Method::POST,
                    format!("/api/wasm/modules/{module_name}"),
                    bytes,
                )
                .await
            }
            "compile_wasm" => {
                let tenant = expect_string_arg(args, 0, "tenant", method)?;
                let module_name = expect_string_arg(args, 1, "module_name", method)?;
                let rust_source = expect_string_arg(args, 2, "rust_source", method)?;

                self.compile_and_upload_wasm(&tenant, &module_name, &rust_source)
                    .await
            }
            // --- Evolution observability (read-only) ---
            "get_trajectories" => {
                let tenant = expect_string_arg(args, 0, "tenant", method)?;
                let entity_type = optional_string_arg(args, 1);
                let failed_only = optional_string_arg(args, 2);
                let limit = optional_string_arg(args, 3);
                let mut path = "/observe/trajectories".to_string();
                let mut params = Vec::new();
                if let Some(ref et) = entity_type {
                    params.push(format!("entity_type={et}"));
                }
                if failed_only.as_deref() == Some("true") {
                    params.push("success=false".to_string());
                }
                if let Some(ref l) = limit {
                    params.push(format!("failed_limit={l}"));
                }
                if !params.is_empty() {
                    path.push('?');
                    path.push_str(&params.join("&"));
                }
                self.temper_request(&tenant, Method::GET, path, None).await
            }
            "get_insights" => {
                let tenant = expect_string_arg(args, 0, "tenant", method)?;
                self.temper_request(
                    &tenant,
                    Method::GET,
                    "/observe/evolution/insights".to_string(),
                    None,
                )
                .await
            }
            "get_evolution_records" => {
                let tenant = expect_string_arg(args, 0, "tenant", method)?;
                let record_type = optional_string_arg(args, 1);
                let path = match record_type {
                    Some(rt) => format!("/observe/evolution/records?record_type={rt}"),
                    None => "/observe/evolution/records".to_string(),
                };
                self.temper_request(&tenant, Method::GET, path, None).await
            }
            "check_sentinel" => {
                let tenant = expect_string_arg(args, 0, "tenant", method)?;
                self.temper_request(
                    &tenant,
                    Method::POST,
                    "/api/evolution/sentinel/check".to_string(),
                    None,
                )
                .await
            }
            "navigate" => {
                let tenant = expect_string_arg(args, 0, "tenant", method)?;
                let path = expect_string_arg(args, 1, "path", method)?;
                let params = args.get(2).and_then(|a| {
                    let s = String::try_from(a).ok()?;
                    serde_json::from_str::<serde_json::Map<String, Value>>(&s).ok()
                });

                // If params present AND path contains a dot → bound action dispatch
                if let Some(params) = params {
                    if path.contains('.') {
                        return self
                            .temper_request(
                                &tenant,
                                Method::POST,
                                format!("/tdata/{path}"),
                                Some(Value::Object(params)),
                            )
                            .await;
                    }
                    // Params but no dot → treat as filtered GET
                    return self
                        .temper_request(&tenant, Method::GET, format!("/tdata/{path}"), None)
                        .await;
                }

                // No params → GET request (entity or collection)
                self.temper_request(&tenant, Method::GET, format!("/tdata/{path}"), None)
                    .await
            }
            "approve_decision" | "deny_decision" | "set_policy" => Err(format!(
                "temper.{method}() is not available to agents. \
                 Governance write operations (approve, deny, set_policy) \
                 can only be performed by humans via the Observe UI or `temper decide` CLI."
            )),
            _ => Err(format!(
                "unknown temper method '{method}'. Available: start_server, \
                 list, get, create, action, patch, navigate, \
                 show_spec, submit_specs, get_policies, \
                 upload_wasm, compile_wasm, \
                 get_decisions, get_decision_status, poll_decision, \
                 get_trajectories, get_insights, get_evolution_records, check_sentinel"
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
        let port = self.server_port.get().ok_or_else(|| {
            "Server not running. Call `await temper.start_server()` first, \
             or restart MCP with --port to connect to an existing server."
                .to_string()
        })?;
        let url = format!("http://127.0.0.1:{port}{path}");
        let mut request = self
            .http
            .request(method, &url)
            .header("X-Tenant-Id", tenant)
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
            .map_err(|e| format!("failed to read Temper response body: {e}"))?;

        if status.is_success() {
            if text.trim().is_empty() {
                return Ok(Value::Null);
            }
            return serde_json::from_str(&text).or(Ok(Value::String(text)));
        }

        if status == reqwest::StatusCode::FORBIDDEN
            && let Some(structured) = format_authz_denied(&text)
        {
            // Return structured denial as successful content so the agent
            // can programmatically detect the denial and poll for approval.
            return Ok(structured);
        }

        Err(format_http_error(status, &text))
    }

    async fn temper_governance_request(
        &self,
        tenant: &str,
        method: Method,
        path: String,
        body: Option<Value>,
    ) -> std::result::Result<Value, String> {
        let port = self.server_port.get().ok_or_else(|| {
            "Server not running. Call `await temper.start_server()` first, \
             or restart MCP with --port to connect to an existing server."
                .to_string()
        })?;
        let url = format!("http://127.0.0.1:{port}{path}");
        let mut request = self
            .http
            .request(method, &url)
            .header("X-Tenant-Id", tenant)
            .header("Accept", "application/json");

        let admin_id = self
            .principal_id
            .as_deref()
            .unwrap_or("mcp-governance-admin");
        request = request
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
            .map_err(|e| format!("failed to read Temper response body: {e}"))?;

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

    /// Send a request with a raw binary body (e.g. WASM module bytes).
    async fn temper_request_bytes(
        &self,
        tenant: &str,
        method: Method,
        path: String,
        body: Vec<u8>,
    ) -> std::result::Result<Value, String> {
        let port = self.server_port.get().ok_or_else(|| {
            "Server not running. Call `await temper.start_server()` first, \
             or restart MCP with --port to connect to an existing server."
                .to_string()
        })?;
        let url = format!("http://127.0.0.1:{port}{path}");
        let mut request = self
            .http
            .request(method, &url)
            .header("X-Tenant-Id", tenant)
            .header("Content-Type", "application/wasm");

        if let Some(ref pid) = self.principal_id {
            request = request
                .header("X-Temper-Principal-Kind", "agent")
                .header("X-Temper-Principal-Id", pid.as_str());
        }

        request = request.body(body);

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
            return serde_json::from_str(&text).or(Ok(Value::String(text)));
        }

        Err(format_http_error(status, &text))
    }

    /// Compile Rust source into a WASM module and upload it to the server.
    ///
    /// Creates a temporary Cargo project with temper-wasm-sdk as a dependency,
    /// compiles it to wasm32-unknown-unknown, and uploads the resulting binary.
    async fn compile_and_upload_wasm(
        &self,
        tenant: &str,
        module_name: &str,
        rust_source: &str,
    ) -> std::result::Result<Value, String> {
        // Pre-assertion: check wasm32-unknown-unknown target is available
        let rustup_check = tokio::process::Command::new("rustup")
            .args(["target", "list", "--installed"])
            .output()
            .await
            .map_err(|e| format!("failed to run rustup: {e}"))?;

        let installed_targets = String::from_utf8_lossy(&rustup_check.stdout);
        if !installed_targets.contains("wasm32-unknown-unknown") {
            return Err("wasm32-unknown-unknown target not installed. \
                 Run: rustup target add wasm32-unknown-unknown"
                .to_string());
        }

        // Create temp build directory
        let build_id = uuid::Uuid::new_v4();
        let build_dir = std::path::PathBuf::from(format!("/tmp/temper-wasm-build-{build_id}"));
        let src_dir = build_dir.join("src");
        tokio::fs::create_dir_all(&src_dir)
            .await
            .map_err(|e| format!("failed to create build dir: {e}"))?;

        // Resolve SDK path (relative to this binary's location or workspace root)
        let sdk_path = self.resolve_sdk_path()?;

        // Write Cargo.toml
        let cargo_toml = format!(
            r#"[package]
name = "temper-user-module"
version = "0.1.0"
edition = "2024"

[lib]
crate-type = ["cdylib"]

[dependencies]
temper-wasm-sdk = {{ path = "{sdk_path}" }}
"#,
        );

        tokio::fs::write(build_dir.join("Cargo.toml"), &cargo_toml)
            .await
            .map_err(|e| format!("failed to write Cargo.toml: {e}"))?;

        // Write user source
        tokio::fs::write(src_dir.join("lib.rs"), rust_source)
            .await
            .map_err(|e| format!("failed to write lib.rs: {e}"))?;

        // Run cargo build with timeout
        let build_result = tokio::time::timeout(
            std::time::Duration::from_secs(120), // determinism-ok: wall-clock timeout for build
            tokio::process::Command::new("cargo")
                .arg("build")
                .arg("--target")
                .arg("wasm32-unknown-unknown")
                .arg("--release")
                .current_dir(&build_dir)
                .env("CARGO_TARGET_DIR", build_dir.join("target"))
                .output(),
        )
        .await
        .map_err(|_| "compilation timed out after 120 seconds".to_string())?
        .map_err(|e| format!("failed to run cargo build: {e}"))?;

        if !build_result.status.success() {
            let stderr = String::from_utf8_lossy(&build_result.stderr);
            // Clean up on failure
            let _ = tokio::fs::remove_dir_all(&build_dir).await;
            return Err(format!("compilation failed:\n{stderr}"));
        }

        // Read the compiled WASM binary
        let wasm_path =
            build_dir.join("target/wasm32-unknown-unknown/release/temper_user_module.wasm");
        let wasm_bytes = tokio::fs::read(&wasm_path)
            .await
            .map_err(|e| format!("failed to read compiled WASM: {e}"))?;

        let wasm_size = wasm_bytes.len();

        // Compute hash for verification
        let hash = format!("{:x}", Sha256::digest(&wasm_bytes));

        // Upload to server
        let upload_result = self
            .temper_request_bytes(
                tenant,
                reqwest::Method::POST,
                format!("/api/wasm/modules/{module_name}"),
                wasm_bytes,
            )
            .await;

        // Clean up build directory
        let _ = tokio::fs::remove_dir_all(&build_dir).await;

        match upload_result {
            Ok(_) => Ok(serde_json::json!({
                "status": "compiled",
                "module": module_name,
                "hash": hash,
                "size": wasm_size,
            })),
            Err(e) => Err(format!("compiled successfully but upload failed: {e}")),
        }
    }

    /// Resolve the path to the temper-wasm-sdk crate.
    fn resolve_sdk_path(&self) -> Result<String, String> {
        // Try to find SDK relative to the binary (workspace root / crates / temper-wasm-sdk)
        if let Some(ref binary) = self.binary_path {
            // Binary is at target/release/temper or target/debug/temper
            // Walk up to find workspace root
            let mut path = std::path::PathBuf::from(binary);
            for _ in 0..5 {
                path.pop();
                let sdk_candidate = path.join("crates/temper-wasm-sdk");
                if sdk_candidate.join("Cargo.toml").exists() {
                    return Ok(sdk_candidate.to_string_lossy().to_string());
                }
            }
        }

        // Fallback: check common development paths
        let cwd = std::env::current_dir().map_err(|e| format!("cannot get cwd: {e}"))?;
        let sdk_candidate = cwd.join("crates/temper-wasm-sdk");
        if sdk_candidate.join("Cargo.toml").exists() {
            return Ok(sdk_candidate.to_string_lossy().to_string());
        }

        Err(
            "cannot find temper-wasm-sdk crate. Ensure you are running from the temper workspace."
                .to_string(),
        )
    }
}
