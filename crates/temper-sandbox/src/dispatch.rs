//! Unified `temper.*` method dispatch.
//!
//! Contains all `temper.*` methods dispatched via HTTP to a running Temper server.

use monty::MontyObject;
use reqwest::Method;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::helpers::{
    escape_odata_key, expect_json_object_arg, expect_string_arg, optional_string_arg,
};
use crate::http::{temper_governance_request, temper_request, temper_request_bytes};

/// Shared context for dispatching temper methods.
pub struct DispatchContext<'a> {
    /// HTTP client.
    pub http: &'a reqwest::Client,
    /// Temper server base URL (e.g. `http://127.0.0.1:3000`).
    pub base_url: &'a str,
    /// Tenant ID.
    pub tenant: &'a str,
    /// Agent principal ID for Cedar authorization.
    pub principal_id: Option<&'a str>,
    /// Optional closure to resolve entity type to entity set name.
    pub entity_set_resolver: Option<&'a (dyn Fn(&str) -> String + Send + Sync)>,
    /// Optional path to temper binary (for `compile_wasm` SDK resolution).
    pub binary_path: Option<&'a std::path::Path>,
    /// Optional API key for authentication.
    pub api_key: Option<&'a str>,
}

impl<'a> DispatchContext<'a> {
    /// Resolve an entity type name to an entity set name.
    fn resolve_set(&self, entity_or_set: &str) -> String {
        if let Some(resolver) = self.entity_set_resolver {
            resolver(entity_or_set)
        } else {
            entity_or_set.to_string()
        }
    }
}

/// Dispatch a `temper.<method>()` call via HTTP to the Temper server.
pub async fn dispatch_temper_method(
    ctx: &DispatchContext<'_>,
    method: &str,
    args: &[MontyObject],
    kwargs: &[(MontyObject, MontyObject)],
) -> Result<Value, String> {
    if !kwargs.is_empty() {
        return Err(format!(
            "temper.{method} does not support keyword arguments"
        ));
    }

    match method {
        // --- Entity CRUD ---
        "list" | "get" | "create" | "action" | "patch" | "navigate" | "get_agent_id" => {
            dispatch_entity(ctx, method, args).await
        }
        // --- Spec management ---
        "submit_specs" | "get_policies" => dispatch_specs(ctx, method, args).await,
        // --- Governance ---
        "get_decisions" | "get_decision_status" | "poll_decision" => {
            dispatch_governance(ctx, method, args).await
        }
        // --- WASM ---
        "upload_wasm" | "compile_wasm" => dispatch_wasm(ctx, method, args).await,
        // --- Evolution / Observe ---
        "get_trajectories" | "get_insights" | "get_evolution_records" | "check_sentinel" => {
            dispatch_evolution(ctx, method, args).await
        }
        // --- OS App Catalog ---
        "list_apps" | "install_app" => dispatch_os_apps(ctx, method, args).await,
        // --- Discovery ---
        "specs" => {
            temper_request(
                ctx.http,
                ctx.base_url,
                ctx.tenant,
                ctx.principal_id,
                ctx.api_key,
                Method::GET,
                "/observe/specs",
                None,
            )
            .await
        }
        "spec_detail" => {
            let entity_type = expect_string_arg(args, 0, "entity_type", method)?;
            temper_request(
                ctx.http,
                ctx.base_url,
                ctx.tenant,
                ctx.principal_id,
                ctx.api_key,
                Method::GET,
                &format!("/observe/specs/{entity_type}"),
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
             list, get, create, action, patch, navigate, get_agent_id, \
             submit_specs, get_policies, \
             upload_wasm, compile_wasm, \
             get_decisions, get_decision_status, poll_decision, \
             get_trajectories, get_insights, get_evolution_records, check_sentinel, \
             list_apps, install_app, \
             specs, spec_detail"
        )),
    }
}

/// Dispatch entity CRUD and navigation methods.
async fn dispatch_entity(
    ctx: &DispatchContext<'_>,
    method: &str,
    args: &[MontyObject],
) -> Result<Value, String> {
    match method {
        "list" => {
            let entity = expect_string_arg(args, 0, "entity_type", method)?;
            let filter = optional_string_arg(args, 1);
            let set = ctx.resolve_set(&entity);
            let path = match filter {
                Some(f) => {
                    let encoded = f.replace(' ', "%20").replace('\'', "%27");
                    format!("/tdata/{set}?$filter={encoded}")
                }
                None => format!("/tdata/{set}"),
            };
            let body = temper_request(
                ctx.http,
                ctx.base_url,
                ctx.tenant,
                ctx.principal_id,
                ctx.api_key,
                Method::GET,
                &path,
                None,
            )
            .await?;
            Ok(body.get("value").cloned().unwrap_or(body))
        }
        "get_agent_id" => Ok(serde_json::json!(ctx.principal_id.unwrap_or(""))),
        "get" => {
            let entity = expect_string_arg(args, 0, "entity_type", method)?;
            let entity_id = expect_string_arg(args, 1, "entity_id", method)?;
            let set = ctx.resolve_set(&entity);
            let key = escape_odata_key(&entity_id);
            temper_request(
                ctx.http,
                ctx.base_url,
                ctx.tenant,
                ctx.principal_id,
                ctx.api_key,
                Method::GET,
                &format!("/tdata/{set}('{key}')"),
                None,
            )
            .await
        }
        "create" => {
            let entity = expect_string_arg(args, 0, "entity_type", method)?;
            let fields = expect_json_object_arg(args, 1, "fields", method)?;
            let set = ctx.resolve_set(&entity);
            let payload = Value::Object(fields);
            temper_request(
                ctx.http,
                ctx.base_url,
                ctx.tenant,
                ctx.principal_id,
                ctx.api_key,
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
            let set = ctx.resolve_set(&entity);
            let key = escape_odata_key(&entity_id);
            let payload = Value::Object(body);
            temper_request(
                ctx.http,
                ctx.base_url,
                ctx.tenant,
                ctx.principal_id,
                ctx.api_key,
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
            let set = ctx.resolve_set(&entity);
            let key = escape_odata_key(&entity_id);
            let payload = Value::Object(fields);
            temper_request(
                ctx.http,
                ctx.base_url,
                ctx.tenant,
                ctx.principal_id,
                ctx.api_key,
                Method::PATCH,
                &format!("/tdata/{set}('{key}')"),
                Some(&payload),
            )
            .await
        }
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
                        ctx.http,
                        ctx.base_url,
                        ctx.tenant,
                        ctx.principal_id,
                        ctx.api_key,
                        Method::POST,
                        &format!("/tdata/{path}"),
                        Some(&payload),
                    )
                    .await;
                }
                return temper_request(
                    ctx.http,
                    ctx.base_url,
                    ctx.tenant,
                    ctx.principal_id,
                    ctx.api_key,
                    Method::GET,
                    &format!("/tdata/{path}"),
                    None,
                )
                .await;
            }

            temper_request(
                ctx.http,
                ctx.base_url,
                ctx.tenant,
                ctx.principal_id,
                ctx.api_key,
                Method::GET,
                &format!("/tdata/{path}"),
                None,
            )
            .await
        }
        _ => unreachable!("dispatch_entity called with non-entity method"),
    }
}

/// Dispatch spec management methods.
async fn dispatch_specs(
    ctx: &DispatchContext<'_>,
    method: &str,
    args: &[MontyObject],
) -> Result<Value, String> {
    match method {
        "submit_specs" => {
            let specs = expect_json_object_arg(args, 0, "specs", method)?;
            let payload = serde_json::json!({ "tenant": ctx.tenant, "specs": specs });
            temper_request(
                ctx.http,
                ctx.base_url,
                ctx.tenant,
                ctx.principal_id,
                ctx.api_key,
                Method::POST,
                "/api/specs/load-inline",
                Some(&payload),
            )
            .await
        }
        "get_policies" => {
            temper_request(
                ctx.http,
                ctx.base_url,
                ctx.tenant,
                ctx.principal_id,
                ctx.api_key,
                Method::GET,
                &format!("/api/tenants/{}/policies", ctx.tenant),
                None,
            )
            .await
        }
        _ => unreachable!("dispatch_specs called with non-specs method"),
    }
}

/// Dispatch governance methods.
async fn dispatch_governance(
    ctx: &DispatchContext<'_>,
    method: &str,
    args: &[MontyObject],
) -> Result<Value, String> {
    match method {
        "get_decisions" => {
            let status = optional_string_arg(args, 0);
            let path = match status {
                Some(s) => format!("/api/tenants/{}/decisions?status={s}", ctx.tenant),
                None => format!("/api/tenants/{}/decisions", ctx.tenant),
            };
            temper_governance_request(
                ctx.http,
                ctx.base_url,
                ctx.tenant,
                ctx.principal_id,
                ctx.api_key,
                Method::GET,
                &path,
                None,
            )
            .await
        }
        "get_decision_status" => {
            let decision_id = expect_string_arg(args, 0, "decision_id", method)?;
            let result = temper_governance_request(
                ctx.http,
                ctx.base_url,
                ctx.tenant,
                ctx.principal_id,
                ctx.api_key,
                Method::GET,
                &format!("/api/tenants/{}/decisions", ctx.tenant),
                None,
            )
            .await?;
            if let Some(decisions) = result.get("decisions").and_then(Value::as_array) {
                for d in decisions {
                    if d.get("id").and_then(Value::as_str) == Some(&decision_id) {
                        let status = d.get("status").and_then(Value::as_str).unwrap_or("unknown");
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
            let config = crate::governance::PollConfig::default();
            let http = ctx.http.clone();
            let base_url = ctx.base_url.to_string();
            let tenant_str = ctx.tenant.to_string();
            let pid = ctx.principal_id.map(|s| s.to_string());
            let api_key = ctx.api_key.map(|s| s.to_string());
            let (decision, _outcome) =
                crate::governance::poll_decision(&decision_id, &config, || {
                    let http = http.clone();
                    let base_url = base_url.clone();
                    let tenant_str = tenant_str.clone();
                    let pid = pid.clone();
                    let api_key = api_key.clone();
                    async move {
                        temper_governance_request(
                            &http,
                            &base_url,
                            &tenant_str,
                            pid.as_deref(),
                            api_key.as_deref(),
                            Method::GET,
                            &format!("/api/tenants/{tenant_str}/decisions"),
                            None,
                        )
                        .await
                    }
                })
                .await?;
            Ok(decision)
        }
        _ => unreachable!("dispatch_governance called with non-governance method"),
    }
}

/// Dispatch WASM methods.
async fn dispatch_wasm(
    ctx: &DispatchContext<'_>,
    method: &str,
    args: &[MontyObject],
) -> Result<Value, String> {
    match method {
        "upload_wasm" => {
            let module_name = expect_string_arg(args, 0, "module_name", method)?;
            let wasm_path = expect_string_arg(args, 1, "wasm_path", method)?;
            let bytes = tokio::fs::read(&wasm_path)
                .await
                .map_err(|e| format!("failed to read WASM file '{}': {}", wasm_path, e))?;
            temper_request_bytes(
                ctx.http,
                ctx.base_url,
                ctx.tenant,
                ctx.principal_id,
                ctx.api_key,
                Method::POST,
                &format!("/api/wasm/modules/{module_name}"),
                bytes,
            )
            .await
        }
        "compile_wasm" => {
            let module_name = expect_string_arg(args, 0, "module_name", method)?;
            let rust_source = expect_string_arg(args, 1, "rust_source", method)?;
            compile_and_upload_wasm(ctx, &module_name, &rust_source).await
        }
        _ => unreachable!("dispatch_wasm called with non-wasm method"),
    }
}

/// Dispatch evolution and observability methods.
async fn dispatch_evolution(
    ctx: &DispatchContext<'_>,
    method: &str,
    args: &[MontyObject],
) -> Result<Value, String> {
    match method {
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
            temper_request(
                ctx.http,
                ctx.base_url,
                ctx.tenant,
                ctx.principal_id,
                ctx.api_key,
                Method::GET,
                &path,
                None,
            )
            .await
        }
        "get_insights" => {
            temper_request(
                ctx.http,
                ctx.base_url,
                ctx.tenant,
                ctx.principal_id,
                ctx.api_key,
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
            temper_request(
                ctx.http,
                ctx.base_url,
                ctx.tenant,
                ctx.principal_id,
                ctx.api_key,
                Method::GET,
                &path,
                None,
            )
            .await
        }
        "check_sentinel" => {
            temper_request(
                ctx.http,
                ctx.base_url,
                ctx.tenant,
                ctx.principal_id,
                ctx.api_key,
                Method::POST,
                "/api/evolution/sentinel/check",
                None,
            )
            .await
        }
        _ => unreachable!("dispatch_evolution called with non-evolution method"),
    }
}

/// Dispatch OS app catalog methods.
async fn dispatch_os_apps(
    ctx: &DispatchContext<'_>,
    method: &str,
    args: &[MontyObject],
) -> Result<Value, String> {
    match method {
        "list_apps" => {
            temper_request(
                ctx.http,
                ctx.base_url,
                ctx.tenant,
                ctx.principal_id,
                ctx.api_key,
                Method::GET,
                "/api/os-apps",
                None,
            )
            .await
        }
        "install_app" => {
            let app_name = expect_string_arg(args, 0, "app_name", method)?;
            let payload = serde_json::json!({ "tenant": ctx.tenant });
            temper_request(
                ctx.http,
                ctx.base_url,
                ctx.tenant,
                ctx.principal_id,
                ctx.api_key,
                Method::POST,
                &format!("/api/os-apps/{app_name}/install"),
                Some(&payload),
            )
            .await
        }
        _ => unreachable!("dispatch_os_apps called with non-os-app method"),
    }
}

/// Compile Rust source into a WASM module and upload it to the server.
async fn compile_and_upload_wasm(
    ctx: &DispatchContext<'_>,
    module_name: &str,
    rust_source: &str,
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

    let sdk_path = resolve_sdk_path(ctx.binary_path)?;

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

    let wasm_path = build_dir.join("target/wasm32-unknown-unknown/release/temper_user_module.wasm");
    let wasm_bytes = tokio::fs::read(&wasm_path)
        .await
        .map_err(|e| format!("failed to read compiled WASM: {e}"))?;

    let wasm_size = wasm_bytes.len();
    let hash = format!("{:x}", Sha256::digest(&wasm_bytes));

    let upload_result = temper_request_bytes(
        ctx.http,
        ctx.base_url,
        ctx.tenant,
        ctx.principal_id,
        ctx.api_key,
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
