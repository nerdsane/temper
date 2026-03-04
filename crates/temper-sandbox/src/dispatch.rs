//! Unified `temper.*` method dispatch.
//!
//! Contains all temper methods except `start_server` (MCP-only, spawns child
//! process) and `show_spec` (MCP-only, reads local spec data).

use monty::MontyObject;
use reqwest::Method;
use sha2::{Digest, Sha256};
use serde_json::Value;

use crate::helpers::{
    escape_odata_key, expect_json_object_arg, expect_string_arg, optional_string_arg,
};
use crate::http::{temper_governance_request, temper_request, temper_request_bytes};

/// Dispatch a `temper.<method>()` call via HTTP to the Temper server.
///
/// Parameters:
/// - `http`: HTTP client
/// - `base_url`: Temper server base URL (e.g. `http://127.0.0.1:3000`)
/// - `tenant`: Tenant ID
/// - `principal_id`: Agent principal ID for Cedar authorization
/// - `method`: Method name (e.g. `"list"`, `"create"`)
/// - `args`: Positional arguments from the Monty call (self already stripped)
/// - `kwargs`: Keyword arguments (currently rejected)
/// - `entity_set_resolver`: Optional closure to resolve entity type to set name
/// - `binary_path`: Optional path to temper binary (for compile_wasm SDK resolution)
pub async fn dispatch_temper_method(
    http: &reqwest::Client,
    base_url: &str,
    tenant: &str,
    principal_id: Option<&str>,
    method: &str,
    args: &[MontyObject],
    kwargs: &[(MontyObject, MontyObject)],
    entity_set_resolver: Option<&(dyn Fn(&str) -> String + Send + Sync)>,
    binary_path: Option<&std::path::Path>,
) -> Result<Value, String> {
    if !kwargs.is_empty() {
        return Err(format!(
            "temper.{method} does not support keyword arguments"
        ));
    }

    let resolve_set = |entity_or_set: &str| -> String {
        if let Some(resolver) = entity_set_resolver {
            resolver(entity_or_set)
        } else {
            entity_or_set.to_string()
        }
    };

    match method {
        // --- Entity CRUD ---
        "list" => {
            let entity = expect_string_arg(args, 0, "entity_type", method)?;
            let set = resolve_set(&entity);
            let body = temper_request(
                http,
                base_url,
                tenant,
                principal_id,
                Method::GET,
                &format!("/tdata/{set}"),
                None,
            )
            .await?;
            Ok(body.get("value").cloned().unwrap_or(body))
        }
        "get" => {
            let entity = expect_string_arg(args, 0, "entity_type", method)?;
            let entity_id = expect_string_arg(args, 1, "entity_id", method)?;
            let set = resolve_set(&entity);
            let key = escape_odata_key(&entity_id);
            temper_request(
                http,
                base_url,
                tenant,
                principal_id,
                Method::GET,
                &format!("/tdata/{set}('{key}')"),
                None,
            )
            .await
        }
        "create" => {
            let entity = expect_string_arg(args, 0, "entity_type", method)?;
            let fields = expect_json_object_arg(args, 1, "fields", method)?;
            let set = resolve_set(&entity);
            let payload = Value::Object(fields);
            temper_request(
                http,
                base_url,
                tenant,
                principal_id,
                Method::POST,
                &format!("/tdata/{set}"),
                Some(&payload),
            )
            .await
        }
        "action" => {
            let entity = expect_string_arg(args, 0, "entity_type", method)?;
            let entity_id = expect_string_arg(args, 1, "entity_id", method)?;
            let action_name = expect_string_arg(args, 2, "action_name", method)?;
            let body = expect_json_object_arg(args, 3, "body", method)?;
            let set = resolve_set(&entity);
            let key = escape_odata_key(&entity_id);
            let payload = Value::Object(body);
            temper_request(
                http,
                base_url,
                tenant,
                principal_id,
                Method::POST,
                &format!("/tdata/{set}('{key}')/Temper.{action_name}"),
                Some(&payload),
            )
            .await
        }
        "patch" => {
            let entity = expect_string_arg(args, 0, "entity_type", method)?;
            let entity_id = expect_string_arg(args, 1, "entity_id", method)?;
            let fields = expect_json_object_arg(args, 2, "fields", method)?;
            let set = resolve_set(&entity);
            let key = escape_odata_key(&entity_id);
            let payload = Value::Object(fields);
            temper_request(
                http,
                base_url,
                tenant,
                principal_id,
                Method::PATCH,
                &format!("/tdata/{set}('{key}')"),
                Some(&payload),
            )
            .await
        }
        // --- Developer methods ---
        "submit_specs" => {
            let specs = expect_json_object_arg(args, 0, "specs", method)?;
            let payload = serde_json::json!({ "tenant": tenant, "specs": specs });
            temper_request(
                http,
                base_url,
                tenant,
                principal_id,
                Method::POST,
                "/api/specs/load-inline",
                Some(&payload),
            )
            .await
        }
        "get_policies" => {
            temper_request(
                http,
                base_url,
                tenant,
                principal_id,
                Method::GET,
                &format!("/api/tenants/{tenant}/policies"),
                None,
            )
            .await
        }
        // --- Governance methods ---
        "get_decisions" => {
            let status = optional_string_arg(args, 0);
            let path = match status {
                Some(s) => format!("/api/tenants/{tenant}/decisions?status={s}"),
                None => format!("/api/tenants/{tenant}/decisions"),
            };
            temper_governance_request(http, base_url, tenant, principal_id, Method::GET, &path, None)
                .await
        }
        "get_decision_status" => {
            let decision_id = expect_string_arg(args, 0, "decision_id", method)?;
            let result = temper_governance_request(
                http,
                base_url,
                tenant,
                principal_id,
                Method::GET,
                &format!("/api/tenants/{tenant}/decisions"),
                None,
            )
            .await?;
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
            let decision_id = expect_string_arg(args, 0, "decision_id", method)?;
            let start = std::time::Instant::now(); // determinism-ok: wall-clock for timeout only
            loop {
                let result = temper_governance_request(
                    http,
                    base_url,
                    tenant,
                    principal_id,
                    Method::GET,
                    &format!("/api/tenants/{tenant}/decisions"),
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
        // --- WASM ---
        "upload_wasm" => {
            let module_name = expect_string_arg(args, 0, "module_name", method)?;
            let wasm_path = expect_string_arg(args, 1, "wasm_path", method)?;
            let bytes = tokio::fs::read(&wasm_path)
                .await
                .map_err(|e| format!("failed to read WASM file '{}': {}", wasm_path, e))?;
            temper_request_bytes(
                http,
                base_url,
                tenant,
                principal_id,
                Method::POST,
                &format!("/api/wasm/modules/{module_name}"),
                bytes,
            )
            .await
        }
        "compile_wasm" => {
            let module_name = expect_string_arg(args, 0, "module_name", method)?;
            let rust_source = expect_string_arg(args, 1, "rust_source", method)?;
            compile_and_upload_wasm(http, base_url, tenant, principal_id, &module_name, &rust_source, binary_path)
                .await
        }
        // --- Evolution observability (read-only) ---
        "get_trajectories" => {
            let entity_type = optional_string_arg(args, 0);
            let failed_only = optional_string_arg(args, 1);
            let limit = optional_string_arg(args, 2);
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
            temper_request(http, base_url, tenant, principal_id, Method::GET, &path, None).await
        }
        "get_insights" => {
            temper_request(
                http,
                base_url,
                tenant,
                principal_id,
                Method::GET,
                "/observe/evolution/insights",
                None,
            )
            .await
        }
        "get_evolution_records" => {
            let record_type = optional_string_arg(args, 0);
            let path = match record_type {
                Some(rt) => format!("/observe/evolution/records?record_type={rt}"),
                None => "/observe/evolution/records".to_string(),
            };
            temper_request(http, base_url, tenant, principal_id, Method::GET, &path, None).await
        }
        "check_sentinel" => {
            temper_request(
                http,
                base_url,
                tenant,
                principal_id,
                Method::POST,
                "/api/evolution/sentinel/check",
                None,
            )
            .await
        }
        // --- Navigation ---
        "navigate" => {
            let path = expect_string_arg(args, 0, "path", method)?;
            let params = args.get(1).and_then(|a| {
                let s = String::try_from(a).ok()?;
                serde_json::from_str::<serde_json::Map<String, Value>>(&s).ok()
            });

            if let Some(params) = params {
                if path.contains('.') {
                    let payload = Value::Object(params);
                    return temper_request(
                        http,
                        base_url,
                        tenant,
                        principal_id,
                        Method::POST,
                        &format!("/tdata/{path}"),
                        Some(&payload),
                    )
                    .await;
                }
                return temper_request(
                    http,
                    base_url,
                    tenant,
                    principal_id,
                    Method::GET,
                    &format!("/tdata/{path}"),
                    None,
                )
                .await;
            }

            temper_request(
                http,
                base_url,
                tenant,
                principal_id,
                Method::GET,
                &format!("/tdata/{path}"),
                None,
            )
            .await
        }
        // --- Blocked methods ---
        "approve_decision" | "deny_decision" | "set_policy" => Err(format!(
            "temper.{method}() is not available to agents. \
             Governance write operations (approve, deny, set_policy) \
             can only be performed by humans via the Observe UI or `temper decide` CLI."
        )),
        _ => Err(format!(
            "unknown temper method '{method}'. Available: \
             list, get, create, action, patch, navigate, \
             submit_specs, get_policies, \
             upload_wasm, compile_wasm, \
             get_decisions, get_decision_status, poll_decision, \
             get_trajectories, get_insights, get_evolution_records, check_sentinel"
        )),
    }
}

/// Compile Rust source into a WASM module and upload it to the server.
async fn compile_and_upload_wasm(
    http: &reqwest::Client,
    base_url: &str,
    tenant: &str,
    principal_id: Option<&str>,
    module_name: &str,
    rust_source: &str,
    binary_path: Option<&std::path::Path>,
) -> Result<Value, String> {
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

    let build_id = uuid::Uuid::new_v4(); // determinism-ok: build directory uniqueness
    let build_dir = std::path::PathBuf::from(format!("/tmp/temper-wasm-build-{build_id}"));
    let src_dir = build_dir.join("src");
    tokio::fs::create_dir_all(&src_dir)
        .await
        .map_err(|e| format!("failed to create build dir: {e}"))?;

    let sdk_path = resolve_sdk_path(binary_path)?;

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

    tokio::fs::write(src_dir.join("lib.rs"), rust_source)
        .await
        .map_err(|e| format!("failed to write lib.rs: {e}"))?;

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
        let _ = tokio::fs::remove_dir_all(&build_dir).await;
        return Err(format!("compilation failed:\n{stderr}"));
    }

    let wasm_path =
        build_dir.join("target/wasm32-unknown-unknown/release/temper_user_module.wasm");
    let wasm_bytes = tokio::fs::read(&wasm_path)
        .await
        .map_err(|e| format!("failed to read compiled WASM: {e}"))?;

    let wasm_size = wasm_bytes.len();
    let hash = format!("{:x}", Sha256::digest(&wasm_bytes));

    let upload_result = temper_request_bytes(
        http,
        base_url,
        tenant,
        principal_id,
        Method::POST,
        &format!("/api/wasm/modules/{module_name}"),
        wasm_bytes,
    )
    .await;

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
fn resolve_sdk_path(binary_path: Option<&std::path::Path>) -> Result<String, String> {
    if let Some(binary) = binary_path {
        let mut path = binary.to_path_buf();
        for _ in 0..5 {
            path.pop();
            let sdk_candidate = path.join("crates/temper-wasm-sdk");
            if sdk_candidate.join("Cargo.toml").exists() {
                return Ok(sdk_candidate.to_string_lossy().to_string());
            }
        }
    }

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
