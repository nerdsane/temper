//! Sandbox Provisioner — WASM module for provisioning sandboxes.
//!
//! Provisions a sandbox (static URL from config, or E2B REST API) and returns
//! the sandbox connection details. Also creates a TemperFS Workspace and File
//! for conversation storage (content-addressable, versioned, Cedar-governed).
//!
//! Priority order:
//! 1. sandbox_url from entity state (set via Configure — for local dev)
//! 2. sandbox_url from integration config (default local sandbox)
//! 3. E2B REST API (for deployed/Railway — requires e2b_api_key secret)
//!
//! Build: `cargo build --target wasm32-unknown-unknown --release`

use temper_wasm_sdk::prelude::*;

/// Entry point.
#[unsafe(no_mangle)]
pub extern "C" fn run(_ctx_ptr: i32, _ctx_len: i32) -> i32 {
    let result = (|| -> Result<(), String> {
        let ctx = Context::from_host()?;
        ctx.log("info", "sandbox_provisioner: starting");

        let fields = ctx.entity_state.get("fields").cloned().unwrap_or(json!({}));

        let user_message = fields
            .get("user_message")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if user_message.is_empty() {
            return Err("agent not configured — user_message is empty".to_string());
        }

        // Provision sandbox
        let sandbox_result = provision_sandbox(&ctx)?;
        ctx.log(
            "info",
            &format!(
                "sandbox_provisioner: sandbox ready at {}",
                sandbox_result.sandbox_url
            ),
        );

        // Create TemperFS Workspace + File for conversation storage
        let temper_api_url = ctx
            .config
            .get("temper_api_url")
            .cloned()
            .unwrap_or_else(|| "http://127.0.0.1:3000".to_string());

        let entity_id = ctx
            .entity_state
            .get("entity_id")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");

        let tenant = &ctx.tenant;

        let fs_result = create_conversation_storage(&ctx, &temper_api_url, tenant, entity_id);

        let (workspace_id, conversation_file_id, file_manifest_id) = match fs_result {
            Ok((ws, conv, manifest)) => (ws, conv, manifest),
            Err(e) => {
                ctx.log(
                    "warn",
                    &format!(
                        "sandbox_provisioner: TemperFS setup failed: {e}, falling back to inline"
                    ),
                );
                (String::new(), String::new(), String::new())
            }
        };

        // Return sandbox + TemperFS details to the state machine
        set_success_result(
            "SandboxReady",
            &json!({
                "sandbox_url": sandbox_result.sandbox_url,
                "sandbox_id": sandbox_result.sandbox_id,
                "workspace_id": workspace_id,
                "conversation_file_id": conversation_file_id,
                "file_manifest_id": file_manifest_id,
            }),
        );

        Ok(())
    })();

    if let Err(e) = result {
        set_error_result(&e);
    }
    0
}

struct SandboxResult {
    sandbox_url: String,
    sandbox_id: String,
}

/// Provision a sandbox. Priority order:
/// 1. sandbox_url from entity state (set via Configure action) or integration config
/// 2. E2B REST API (requires e2b_api_key in integration config)
fn provision_sandbox(ctx: &Context) -> Result<SandboxResult, String> {
    let fields = ctx.entity_state.get("fields").cloned().unwrap_or(json!({}));

    // Priority 1: sandbox_url from entity state (set at Configure time) or config.
    let static_url = fields
        .get("sandbox_url")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| {
            ctx.config
                .get("sandbox_url")
                .filter(|s| !s.is_empty())
                .cloned()
        })
        .or_else(|| {
            ctx.trigger_params
                .get("sandbox_url")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        });
    if let Some(url) = static_url {
        ctx.log(
            "info",
            &format!("sandbox_provisioner: using static sandbox_url: {url}"),
        );
        return Ok(SandboxResult {
            sandbox_url: url,
            sandbox_id: "static-sandbox".to_string(),
        });
    }

    // Priority 2: E2B REST API (requires e2b_api_key).
    let e2b_api_key = ctx.config.get("e2b_api_key").cloned().unwrap_or_default();

    if e2b_api_key.is_empty() || e2b_api_key.contains("{secret:") {
        return Err("no sandbox_url configured and no e2b_api_key available — \
             set sandbox_url via Configure or store e2b_api_key secret"
            .to_string());
    }

    ctx.log("info", "sandbox_provisioner: provisioning via E2B API");

    let e2b_api_url = ctx
        .config
        .get("e2b_api_url")
        .cloned()
        .unwrap_or_else(|| "https://api.e2b.dev".to_string());

    let template_id = ctx
        .config
        .get("e2b_template_id")
        .cloned()
        .unwrap_or_else(|| "base".to_string());

    // Create sandbox via E2B REST API
    let create_url = format!("{e2b_api_url}/sandboxes");
    let headers = vec![
        ("x-api-key".to_string(), e2b_api_key.clone()),
        ("content-type".to_string(), "application/json".to_string()),
    ];

    let body = json!({
        "templateID": template_id,
        "timeout": 600,
    });

    let resp = ctx.http_call("POST", &create_url, &headers, &body.to_string())?;

    if resp.status < 200 || resp.status >= 300 {
        return Err(format!(
            "E2B sandbox creation failed (HTTP {}): {}",
            resp.status,
            &resp.body[..resp.body.len().min(500)]
        ));
    }

    let parsed: Value = serde_json::from_str(&resp.body)
        .map_err(|e| format!("failed to parse E2B response: {e}"))?;

    let sandbox_id = parsed
        .get("sandboxID")
        .or_else(|| parsed.get("sandbox_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    let client_id = parsed
        .get("clientID")
        .or_else(|| parsed.get("client_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // E2B sandbox URL: envd daemon on port 49983 at domain e2b.app.
    // URL format: https://{port}-{sandbox_id}.{domain} (port comes FIRST).
    // File ops (read/write) are plain HTTP on this endpoint.
    let sandbox_url = parsed
        .get("sandbox_url")
        .or_else(|| parsed.get("url"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("https://49983-{sandbox_id}.e2b.app"));

    ctx.log(
        "info",
        &format!(
            "sandbox_provisioner: E2B sandbox created: id={sandbox_id}, client={client_id}, url={sandbox_url}"
        ),
    );

    Ok(SandboxResult {
        sandbox_url,
        sandbox_id,
    })
}

/// Create a TemperFS Workspace, conversation File, and manifest File.
/// Returns (workspace_entity_id, conversation_file_id, manifest_file_id).
fn create_conversation_storage(
    ctx: &Context,
    temper_api_url: &str,
    tenant: &str,
    agent_id: &str,
) -> Result<(String, String, String), String> {
    let headers = vec![
        ("content-type".to_string(), "application/json".to_string()),
        ("x-tenant-id".to_string(), tenant.to_string()),
        ("x-temper-principal-kind".to_string(), "system".to_string()),
    ];

    // 1. Create Workspace
    let ws_body = json!({
        "WorkspaceId": format!("agent-{agent_id}"),
        "name": format!("Agent {agent_id} Workspace"),
        "owner_id": agent_id,
        "quota_bytes": "104857600"
    });

    let ws_url = format!("{temper_api_url}/tdata/Workspaces");
    let ws_resp = ctx.http_call("POST", &ws_url, &headers, &ws_body.to_string())?;

    if ws_resp.status < 200 || ws_resp.status >= 300 {
        return Err(format!(
            "Workspace creation failed (HTTP {}): {}",
            ws_resp.status,
            &ws_resp.body[..ws_resp.body.len().min(300)]
        ));
    }

    let ws_parsed: Value = serde_json::from_str(&ws_resp.body)
        .map_err(|e| format!("parse workspace response: {e}"))?;
    let workspace_id = ws_parsed
        .get("entity_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    ctx.log(
        "info",
        &format!("sandbox_provisioner: created workspace {workspace_id}"),
    );

    // 2. Create File for conversation
    let file_body = json!({
        "FileId": format!("conv-{agent_id}"),
        "workspace_id": workspace_id,
        "name": "conversation.json",
        "mime_type": "application/json",
        "path": "/conversation.json"
    });

    let file_url = format!("{temper_api_url}/tdata/Files");
    let file_resp = ctx.http_call("POST", &file_url, &headers, &file_body.to_string())?;

    if file_resp.status < 200 || file_resp.status >= 300 {
        return Err(format!(
            "File creation failed (HTTP {}): {}",
            file_resp.status,
            &file_resp.body[..file_resp.body.len().min(300)]
        ));
    }

    let file_parsed: Value =
        serde_json::from_str(&file_resp.body).map_err(|e| format!("parse file response: {e}"))?;
    let file_id = file_parsed
        .get("entity_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    ctx.log(
        "info",
        &format!("sandbox_provisioner: created conversation file {file_id}"),
    );

    // 3. Write initial empty conversation
    let init_conv = json!({"messages": []}).to_string();
    let value_url = format!("{temper_api_url}/tdata/Files('{file_id}')/$value");
    let value_headers = vec![
        ("content-type".to_string(), "application/json".to_string()),
        ("x-tenant-id".to_string(), tenant.to_string()),
        ("x-temper-principal-kind".to_string(), "system".to_string()),
    ];
    let value_resp = ctx.http_call("PUT", &value_url, &value_headers, &init_conv)?;

    if value_resp.status < 200 || value_resp.status >= 300 {
        ctx.log(
            "warn",
            &format!(
                "sandbox_provisioner: initial $value write failed (HTTP {})",
                value_resp.status
            ),
        );
    }

    // 4. Create manifest File for sandbox fsync
    let manifest_body = json!({
        "FileId": format!("manifest-{agent_id}"),
        "workspace_id": workspace_id,
        "name": "file_manifest.json",
        "mime_type": "application/json",
        "path": "/file_manifest.json"
    });

    let manifest_resp = ctx.http_call("POST", &file_url, &headers, &manifest_body.to_string())?;

    if manifest_resp.status < 200 || manifest_resp.status >= 300 {
        return Err(format!(
            "Manifest File creation failed (HTTP {}): {}",
            manifest_resp.status,
            &manifest_resp.body[..manifest_resp.body.len().min(300)]
        ));
    }

    let manifest_parsed: Value = serde_json::from_str(&manifest_resp.body)
        .map_err(|e| format!("parse manifest response: {e}"))?;
    let manifest_id = manifest_parsed
        .get("entity_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    ctx.log(
        "info",
        &format!("sandbox_provisioner: created manifest file {manifest_id}"),
    );

    // 5. Write initial empty manifest
    let init_manifest = json!({"files": {}, "synced_at_turn": 0}).to_string();
    let manifest_value_url = format!("{temper_api_url}/tdata/Files('{manifest_id}')/$value");
    let manifest_value_resp =
        ctx.http_call("PUT", &manifest_value_url, &value_headers, &init_manifest)?;

    if manifest_value_resp.status < 200 || manifest_value_resp.status >= 300 {
        ctx.log(
            "warn",
            &format!(
                "sandbox_provisioner: initial manifest $value write failed (HTTP {})",
                manifest_value_resp.status
            ),
        );
    }

    Ok((workspace_id, file_id, manifest_id))
}
