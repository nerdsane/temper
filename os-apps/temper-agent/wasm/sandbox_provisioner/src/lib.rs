//! Sandbox Provisioner — WASM module for provisioning sandboxes.
//!
//! Provisions a sandbox (static URL from config, E2B REST API, or Tensorlake REST API)
//! and returns the sandbox connection details. Also creates a TemperFS Workspace and
//! File for conversation storage (content-addressable, versioned, Cedar-governed).
//!
//! Priority order:
//! 1. sandbox_provider == "tensorlake" → Tensorlake REST API (requires tensorlake_api_key secret)
//! 2. sandbox_url from entity state (set via Configure — for local dev)
//! 3. sandbox_url from integration config (default local sandbox)
//! 4. E2B REST API (for deployed/Railway — requires e2b_api_key secret)
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

        let entity_id = ctx
            .entity_state
            .get("entity_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let sandbox_provider = fields
            .get("sandbox_provider")
            .and_then(|v| v.as_str())
            .unwrap_or("local");

        // Emit structured telemetry for sandbox provisioning.
        let _ = ctx.log_structured("info", "sandbox.provision", &json!({
            "agent.run_id": entity_id,
            "agent.sandbox_provider": sandbox_provider,
        }));

        let user_message = fields
            .get("user_message")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if user_message.is_empty() {
            return Err("agent not configured — user_message is empty".to_string());
        }

        // Provider allocation is the durable boundary for a per-run sandbox.
        // `Provision` records it before any later bootstrap operation (TemperFS
        // setup or a private clone) can fail. This prevents deletion from
        // losing the only provider resource identifier after a failed clone.
        let mut sandbox_result = match ctx.trigger_action.as_str() {
            "Provision" => {
                let sandbox_result = provision_sandbox(&ctx)?;
                ctx.log(
                    "info",
                    &format!(
                        "sandbox_provisioner: sandbox allocated at {}",
                        sandbox_result.sandbox_url
                    ),
                );
                set_success_result(
                    "SandboxAllocated",
                    &json!({
                        "sandbox_url": sandbox_result.sandbox_url,
                        "sandbox_id": sandbox_result.sandbox_id,
                        "sandbox_provider": sandbox_result.sandbox_provider,
                    }),
                );
                return Ok(());
            }
            "SandboxAllocated" => allocated_sandbox_from_fields(&fields)?,
            action => {
                return Err(format!(
                    "sandbox_provisioner cannot handle trigger action {action:?}"
                ));
            }
        };

        // This follow-up invocation runs only after SandboxAllocated has
        // persisted the sandbox identity in entity state. Tensorlake creation
        // is asynchronous, so readiness polling belongs here rather than in
        // the allocation stage: a timeout cannot lose the teardown target.
        if sandbox_result.sandbox_provider == "tensorlake" {
            sandbox_result.sandbox_url = wait_for_tensorlake_ready(&ctx, &sandbox_result.sandbox_id)?;
        }
        ctx.log(
            "info",
            &format!(
                "sandbox_provisioner: bootstrapping allocated sandbox at {}",
                sandbox_result.sandbox_url
            ),
        );

        // Create TemperFS Workspace + File for conversation storage.
        // Prefer per-run override from Configure state, then integration config.
        let temper_api_url = resolve_temper_api_url(&ctx, &fields);

        let entity_id = ctx
            .entity_state
            .get("entity_id")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");

        let tenant = &ctx.tenant;

        let fs_result =
            create_conversation_storage(&ctx, &temper_api_url, tenant, entity_id, user_message);

        let (workspace_id, conversation_file_id, file_manifest_id, session_file_id, session_leaf_id) =
            match fs_result {
            Ok((ws, conv, manifest, session_file_id, session_leaf_id)) => {
                (ws, conv, manifest, session_file_id, session_leaf_id)
            }
            Err(e) => {
                ctx.log(
                    "warn",
                    &format!(
                        "sandbox_provisioner: TemperFS bootstrap failed at {temper_api_url}/tdata (tenant={tenant}, agent={entity_id}): {e}. Ensure os-app 'temper-fs' is installed for this tenant and temper_api_url is correct. Falling back to inline."
                    ),
                );
                (
                    String::new(),
                    String::new(),
                    String::new(),
                    String::new(),
                    String::new(),
                )
            }
        };

        // Clone repo into sandbox if repo_url is set.
        let repo_url = fields
            .get("repo_url")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let repo_ref = fields
            .get("repo_ref")
            .and_then(|v| v.as_str())
            .unwrap_or("main");

        let configured_workdir = fields
            .get("workdir")
            .and_then(|v| v.as_str())
            .unwrap_or("/workspace")
            .to_string();

        // effective_workdir starts as whatever was configured (unchanged
        // behavior for local/E2B, or Tensorlake with no repo_url). Only
        // overwritten below when a Tensorlake clone reports back the real
        // path it landed at.
        let mut effective_workdir = configured_workdir.clone();

        if !repo_url.is_empty() {
            // A clone failure is a hard failure of provisioning, not a
            // logged-and-ignored warning: repo_url being set means the run
            // is meaningless without the repo, so silently continuing into
            // an empty sandbox would only surface later as confusing
            // "file not found" tool errors instead of a clear cause here.
            let cloned_path = clone_repo_into_sandbox(
                &ctx,
                &sandbox_result.sandbox_url,
                repo_url,
                repo_ref,
                &fields,
            )?;
            ctx.log("info", "sandbox_provisioner: repo cloned successfully");
            if let Some(path) = cloned_path {
                effective_workdir = path;
            }
        }

        // Return sandbox + TemperFS details to the state machine
        set_success_result(
            "SandboxReady",
            &json!({
                "sandbox_url": sandbox_result.sandbox_url,
                "sandbox_id": sandbox_result.sandbox_id,
                "workspace_id": workspace_id,
                "conversation_file_id": conversation_file_id,
                "file_manifest_id": file_manifest_id,
                "session_file_id": session_file_id,
                "session_leaf_id": session_leaf_id,
                "workdir": effective_workdir,
            }),
        );

        Ok(())
    })();

    if let Err(e) = result {
        set_error_result(&e);
    }
    0
}

#[derive(Debug)]
struct SandboxResult {
    sandbox_url: String,
    sandbox_id: String,
    sandbox_provider: String,
}

/// Reconstruct the previously persisted provider allocation for bootstrap.
fn allocated_sandbox_from_fields(fields: &Value) -> Result<SandboxResult, String> {
    let sandbox_url = fields
        .get("sandbox_url")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "SandboxAllocated is missing sandbox_url".to_string())?
        .to_string();
    let sandbox_id = fields
        .get("sandbox_id")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "SandboxAllocated is missing sandbox_id".to_string())?
        .to_string();
    let sandbox_provider = fields
        .get("sandbox_provider")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "SandboxAllocated is missing sandbox_provider".to_string())?
        .to_string();

    Ok(SandboxResult {
        sandbox_url,
        sandbox_id,
        sandbox_provider,
    })
}

#[cfg(test)]
mod allocation_tests {
    use super::allocated_sandbox_from_fields;
    use temper_wasm_sdk::prelude::json;

    #[test]
    fn reconstructs_durable_allocation_for_bootstrap() {
        let allocation = allocated_sandbox_from_fields(&json!({
            "sandbox_id": "sandbox-123",
            "sandbox_url": "https://sandbox-123.sandbox.tensorlake.ai",
            "sandbox_provider": "tensorlake",
        }))
        .expect("persisted allocation must be usable by bootstrap");

        assert_eq!(allocation.sandbox_id, "sandbox-123");
        assert_eq!(
            allocation.sandbox_url,
            "https://sandbox-123.sandbox.tensorlake.ai"
        );
        assert_eq!(allocation.sandbox_provider, "tensorlake");
    }

    #[test]
    fn rejects_incomplete_allocation_before_bootstrap() {
        let error = allocated_sandbox_from_fields(&json!({
            "sandbox_url": "https://sandbox.tensorlake.ai",
            "sandbox_provider": "tensorlake",
        }))
        .expect_err("missing sandbox id must not be bootstrapped");

        assert!(error.contains("sandbox_id"));
    }
}

fn resolve_temper_api_url(ctx: &Context, fields: &Value) -> String {
    fields
        .get("temper_api_url")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| match ctx.config.get("temper_api_url").map(String::as_str) {
            Some(value) if !value.trim().is_empty() && !value.contains("{secret:") => {
                Some(value.to_string())
            }
            _ => None,
        })
        .unwrap_or_else(|| "http://127.0.0.1:3000".to_string())
}

/// Provision a sandbox. Priority order:
/// 1. sandbox_provider == "tensorlake" → Tensorlake REST API (requires tensorlake_api_key)
/// 2. sandbox_url from entity state (set via Configure action) or integration config
/// 3. E2B REST API (requires e2b_api_key in integration config)
fn provision_sandbox(ctx: &Context) -> Result<SandboxResult, String> {
    let fields = ctx.entity_state.get("fields").cloned().unwrap_or(json!({}));

    // Priority 0: Tensorlake REST API (requires tensorlake_api_key secret).
    let sandbox_provider = fields
        .get("sandbox_provider")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if sandbox_provider == "tensorlake" {
        return provision_tensorlake(ctx, &fields);
    }

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
            sandbox_provider: "local".to_string(),
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
        sandbox_provider: "e2b".to_string(),
    })
}

/// Create a TemperFS Workspace, conversation File, manifest File, and session file.
/// Returns (workspace_entity_id, conversation_file_id, manifest_file_id, session_file_id, session_leaf_id).
fn create_conversation_storage(
    ctx: &Context,
    temper_api_url: &str,
    tenant: &str,
    agent_id: &str,
    user_message: &str,
) -> Result<(String, String, String, String, String), String> {
    let headers = vec![
        ("content-type".to_string(), "application/json".to_string()),
        ("x-tenant-id".to_string(), tenant.to_string()),
        ("x-temper-principal-kind".to_string(), "admin".to_string()),
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
        ("x-temper-principal-kind".to_string(), "admin".to_string()),
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

    let (session_file_id, session_leaf_id) =
        create_session_tree(ctx, temper_api_url, tenant, &workspace_id, agent_id, user_message);

    Ok((
        workspace_id,
        file_id,
        manifest_id,
        session_file_id,
        session_leaf_id,
    ))
}

/// Create a session tree JSONL file in TemperFS.
/// Returns (session_file_id, session_leaf_id). Non-fatal on failure.
fn create_session_tree(
    ctx: &Context,
    temper_api_url: &str,
    tenant: &str,
    workspace_id: &str,
    agent_id: &str,
    user_message: &str,
) -> (String, String) {
    let headers = vec![
        ("content-type".to_string(), "application/json".to_string()),
        ("x-tenant-id".to_string(), tenant.to_string()),
        ("x-temper-principal-kind".to_string(), "admin".to_string()),
    ];

    // Create session JSONL file in TemperFS
    let session_file_body = json!({
        "FileId": format!("session-{agent_id}"),
        "workspace_id": workspace_id,
        "name": "session.jsonl",
        "mime_type": "text/plain",
        "path": "/session.jsonl"
    });
    let session_file_resp = match ctx.http_call(
        "POST",
        &format!("{temper_api_url}/tdata/Files"),
        &headers,
        &serde_json::to_string(&session_file_body).unwrap_or_default(),
    ) {
        Ok(resp) => resp,
        Err(e) => {
            ctx.log("warn", &format!("Failed to create session file: {e}"));
            return (String::new(), String::new());
        }
    };

    let session_file_id = if session_file_resp.status >= 200 && session_file_resp.status < 300 {
        let parsed: Value =
            serde_json::from_str(&session_file_resp.body).unwrap_or(json!({}));
        parsed
            .get("entity_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    } else {
        ctx.log(
            "warn",
            &format!(
                "Failed to create session file (HTTP {})",
                session_file_resp.status
            ),
        );
        return (String::new(), String::new());
    };

    if session_file_id.is_empty() {
        return (String::new(), String::new());
    }

    // Initialize session file with JSONL header + first user message
    let header_id = format!("h-{agent_id}");
    let header_entry = json!({
        "id": header_id,
        "parentId": null,
        "type": "header",
        "version": 1,
        "tokens": 0
    });
    let header_line = serde_json::to_string(&header_entry).unwrap_or_default();

    let session_leaf_id = format!("u-{agent_id}-0");
    let user_entry = json!({
        "id": session_leaf_id,
        "parentId": header_id,
        "type": "message",
        "role": "user",
        "content": user_message,
        "tokens": user_message.len() / 4
    });
    let user_line = serde_json::to_string(&user_entry).unwrap_or_default();
    let initial_jsonl = format!("{header_line}\n{user_line}");

    let write_url = format!("{temper_api_url}/tdata/Files('{session_file_id}')/$value");
    let write_headers = vec![
        ("content-type".to_string(), "text/plain".to_string()),
        ("x-tenant-id".to_string(), tenant.to_string()),
        ("x-temper-principal-kind".to_string(), "admin".to_string()),
    ];
    match ctx.http_call("PUT", &write_url, &write_headers, &initial_jsonl) {
        Ok(resp) if resp.status >= 200 && resp.status < 300 => {
            ctx.log("info", "sandbox_provisioner: session tree initialized");
        }
        Ok(resp) => {
            ctx.log(
                "warn",
                &format!("Failed to write session file (HTTP {})", resp.status),
            );
        }
        Err(e) => {
            ctx.log("warn", &format!("Failed to write session file: {e}"));
        }
    }

    (session_file_id, session_leaf_id)
}

/// Provision a sandbox via the Tensorlake REST API.
///
/// Requires `tensorlake_api_key` in integration config (stored as a secret).
/// Creates a sandbox with the given image and resources, then returns the
/// sandbox URL and ID. The sandbox URL is the ingress endpoint that the
/// tool_runner will use for file/process operations.
fn provision_tensorlake(ctx: &Context, fields: &Value) -> Result<SandboxResult, String> {
    let api_key = ctx.config.get("tensorlake_api_key").cloned().unwrap_or_default();

    if api_key.is_empty() || api_key.contains("{secret:") {
        return Err(
            "sandbox_provider is \"tensorlake\" but tensorlake_api_key is not set — \
             store tensorlake_api_key secret"
                .to_string(),
        );
    }

    ctx.log("info", "sandbox_provisioner: provisioning via Tensorlake API");

    let api_url = resolved_config(ctx, "tensorlake_api_url")
        .unwrap_or_else(|| "https://api.tensorlake.ai".to_string());

    // Verified against the live API (scripts/tl-probe.sh): CreateSandboxRequest
    // documents `image` as "Optional ... When omitted, Tensorlake uses the
    // default managed environment." There is no platform-wide default image
    // literal to fall back to — an unregistered name (e.g. our earlier guess
    // "tensorlake/ubuntu") would fail sandbox creation. Only send `image` when
    // the caller actually specified one.
    let image = fields
        .get("sandbox_image")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
        .or_else(|| resolved_config(ctx, "tensorlake_image"));

    let entity_id = ctx
        .entity_state
        .get("entity_id")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    // Tensorlake sandbox names must satisfy: start with a lowercase letter,
    // contain only lowercase letters/digits/hyphens, not end with a hyphen,
    // max 63 chars (verified: `tl sbx create --help`). Temper run IDs look
    // like `run_<32 hex>` — the underscore alone fails creation with HTTP
    // 400 ("contains invalid character '_'"), which is what surfaced this.
    let sandbox_name = sanitize_sandbox_name(&format!("temper-agent-{entity_id}"));

    // Verified endpoint: POST /sandboxes (no /v2 prefix — that segment only
    // appears in the docs URL, not the API path). Body per CreateSandboxRequest:
    // `resources` and `network` are nested objects, not flat fields.
    let create_url = format!("{api_url}/sandboxes");
    let headers = vec![
        ("Authorization".to_string(), format!("Bearer {api_key}")),
        ("content-type".to_string(), "application/json".to_string()),
    ];

    let mut body = json!({
        "name": sandbox_name,
        "resources": {
            "cpus": 1,
            "memory_mb": 2048,
        },
        "timeout_secs": 900,
        "network": {
            "allow_internet_access": true,
        },
    });
    if let Some(image) = image {
        body["image"] = json!(image);
    }

    let resp = ctx.http_call("POST", &create_url, &headers, &body.to_string())?;

    if resp.status < 200 || resp.status >= 300 {
        return Err(format!(
            "Tensorlake sandbox creation failed (HTTP {}): {}",
            resp.status,
            &resp.body[..resp.body.len().min(500)]
        ));
    }

    let parsed: Value = serde_json::from_str(&resp.body)
        .map_err(|e| format!("failed to parse Tensorlake response: {e}"))?;

    let sandbox_id = parsed
        .get("sandbox_id")
        .or_else(|| parsed.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    // Return an addressable derived proxy URL immediately. Readiness polling
    // happens after `SandboxAllocated` has durably persisted this ID.
    let sandbox_url = parsed
        .get("sandbox_url")
        .and_then(|value| value.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| format!("https://{sandbox_id}.sandbox.tensorlake.ai"));

    ctx.log(
        "info",
        &format!(
            "sandbox_provisioner: Tensorlake sandbox created: id={sandbox_id}, url={sandbox_url}"
        ),
    );

    Ok(SandboxResult {
        sandbox_url,
        sandbox_id,
        sandbox_provider: "tensorlake".to_string(),
    })
}

/// Wait for a previously persisted Tensorlake sandbox to accept proxy work.
///
/// This intentionally runs only during `SandboxAllocated` bootstrap. A
/// readiness error therefore leaves the run with its provider sandbox ID so a
/// later governed delete can perform teardown instead of orphaning compute.
fn wait_for_tensorlake_ready(ctx: &Context, sandbox_id: &str) -> Result<String, String> {
    let api_key = resolved_config(ctx, "tensorlake_api_key")
        .ok_or_else(|| "tensorlake_api_key is not set — cannot poll sandbox readiness".to_string())?;
    let api_url = resolved_config(ctx, "tensorlake_api_url")
        .unwrap_or_else(|| "https://api.tensorlake.ai".to_string());
    let get_url = format!("{api_url}/sandboxes/{sandbox_id}");
    let headers = vec![("Authorization".to_string(), format!("Bearer {api_key}"))];
    let poll_start_ms = Context::get_time_millis();
    const MAX_POLL_MS: i64 = 60_000;
    const MAX_POLL_ATTEMPTS: u32 = 120;
    let mut last_status = String::new();

    for attempt in 0..MAX_POLL_ATTEMPTS {
        let response = ctx.http_call("GET", &get_url, &headers, "")?;
        if response.status >= 200 && response.status < 300 {
            if let Ok(info) = serde_json::from_str::<Value>(&response.body) {
                last_status = info
                    .get("status")
                    .and_then(|value| value.as_str())
                    .unwrap_or("")
                    .to_string();
                if last_status == "running" {
                    return Ok(info
                        .get("sandbox_url")
                        .and_then(|value| value.as_str())
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("https://{sandbox_id}.sandbox.tensorlake.ai")));
                }
            }
        }
        if Context::get_time_millis() - poll_start_ms > MAX_POLL_MS {
            break;
        }
        // Pace the poll: each HTTP round trip provides natural latency, but
        // a fast-responding API can exhaust 120 attempts in seconds. Issue
        // a lightweight HEAD request to the sandbox proxy endpoint to add
        // a bounded network delay between polls (typically 100-500 ms per
        // round trip). This is a stopgap until the WASM host supports a
        // proper sleep primitive.
        if attempt + 1 < MAX_POLL_ATTEMPTS {
            let _ = ctx.http_call(
                "GET",
                &format!("https://{sandbox_id}.sandbox.tensorlake.ai/api/v1/health"),
                &[],
                "",
            );
        }
    }

    Err(format!(
        "Tensorlake sandbox {sandbox_id} did not reach running within {MAX_POLL_MS}ms (last status: {last_status:?})"
    ))
}

/// Clone a git repo into the sandbox.
///
/// Returns `Ok(Some(path))` when the clone landed at a different absolute
/// path than the entity's configured `workdir` (always true for Tensorlake —
/// see `clone_repo_tensorlake`), so the caller can update `workdir` and keep
/// later tool_runner file operations pointed at the real location. Returns
/// `Ok(None)` when the clone landed exactly at the configured `workdir`
/// (E2B / local — unchanged behavior).
fn clone_repo_into_sandbox(
    ctx: &Context,
    sandbox_url: &str,
    repo_url: &str,
    repo_ref: &str,
    fields: &Value,
) -> Result<Option<String>, String> {
    let is_e2b = sandbox_url.contains("e2b.app") || sandbox_url.contains("e2b.dev");
    let is_tensorlake = sandbox_url.contains("tensorlake.ai");

    if is_tensorlake {
        let github_token = resolved_config(ctx, "github_token").unwrap_or_default();
        return clone_repo_tensorlake(ctx, sandbox_url, repo_url, repo_ref, &github_token)
            .map(Some);
    }

    let workdir = fields
        .get("workdir")
        .and_then(|v| v.as_str())
        .unwrap_or("/workspace");

    let github_token = resolved_config(ctx, "github_token").unwrap_or_default();

    // Inject token for private repos.
    let clone_url = if !github_token.is_empty() && repo_url.starts_with("https://github.com") {
        repo_url.replacen("https://", &format!("https://x-access-token:{github_token}@"), 1)
    } else {
        repo_url.to_string()
    };

    ctx.log(
        "info",
        &format!("sandbox_provisioner: cloning {repo_url} (ref={repo_ref}) into {workdir}"),
    );

    // `|| true` must scope ONLY to the checkout, not the whole chain. Written
    // as `A && B && C || true`, bash's left-to-right &&/|| precedence makes
    // this `(A && B && C) || true` — a clone failure would be swallowed by
    // `|| true` just as much as a missing ref, and the exit-code check below
    // would never see it. Parenthesizing the checkout keeps `git clone`'s
    // exit status as the command's real exit status.
    let clone_cmd = format!(
        "mkdir -p {workdir} && cd {workdir} && git clone {clone_url} repo && cd repo && (git checkout {repo_ref} 2>/dev/null || true)"
    );

    if is_e2b {
        let url = format!("{sandbox_url}/process.Process/Start");
        let body = serde_json::to_string(&json!({
            "command": clone_cmd,
            "envs": {},
            "cwd": workdir,
        }))
        .unwrap_or_default();
        let resp = ctx.http_call("POST", &url, &[], &body)?;
        if resp.status < 200 || resp.status >= 300 {
            return Err(format!("git clone failed (HTTP {}): {}", resp.status, &resp.body[..resp.body.len().min(200)]));
        }
    } else {
        let url = format!("{sandbox_url}/v1/processes/run");
        let body = serde_json::to_string(&json!({
            "command": clone_cmd,
            "workdir": workdir,
        }))
        .unwrap_or_default();
        let headers = vec![("content-type".to_string(), "application/json".to_string())];
        let resp = ctx.http_call("POST", &url, &headers, &body)?;
        if resp.status < 200 || resp.status >= 300 {
            return Err(format!("git clone failed (HTTP {}): {}", resp.status, &resp.body[..resp.body.len().min(200)]));
        }
        let parsed: Value = serde_json::from_str(&resp.body)
            .map_err(|e| format!("failed to parse clone response: {e}"))?;
        let exit_code = parsed.get("exit_code").and_then(|v| v.as_i64()).unwrap_or(0);
        if exit_code != 0 {
            let stderr = parsed.get("stderr").and_then(|v| v.as_str()).unwrap_or("");
            return Err(format!("git clone failed (exit {exit_code}): {stderr}"));
        }
    }

    Ok(None)
}

/// Clone into a Tensorlake sandbox's home directory and return the resolved
/// absolute clone path (e.g. `/root/repo`).
///
/// Two things this deliberately does NOT do, both proven not to work by
/// direct testing rather than assumed:
///   - Does not clone into `/workspace`. The sandbox's default managed-image
///     user cannot write to arbitrary root-level paths; `mkdir -p /workspace`
///     fails for that user. `$HOME` is always writable by the user that owns
///     it, so cloning there is the one location that's guaranteed to work
///     without knowing anything else about the image.
///   - Does not embed the token as a literal in the command string or the
///     JSON body. The token is delivered via the process `env` field
///     (verified working: scripts/tl-probe.sh step 3b) and referenced in the
///     shell command only as `${GITHUB_TOKEN}`, substituted by the sandbox's
///     own shell — so the raw value never appears in anything Temper sends
///     or logs. This does not hide the token from `env`/`printenv` inside
///     the sandbox for the duration of this one process's execution; no
///     Tensorlake mechanism achieves that today (confirmed directly by
///     Tensorlake's own team, not inferred from doc gaps).
///
/// `x-access-token` is used as the git username scheme. The current
/// fine-grained credential rejects the `oauth2` username but succeeds with
/// `x-access-token` (verified by an authenticated local clone of the fixture).
/// Keep the token itself in the command-scoped process environment.
///
/// `$HOME`'s actual value is not documented and must not be assumed (it
/// depends on which user the managed image runs as) — it's read back from
/// the same command that does the clone via a leading `echo`, and returned
/// to the caller so it can become the entity's new `workdir`.
fn clone_repo_tensorlake(
    ctx: &Context,
    sandbox_url: &str,
    repo_url: &str,
    repo_ref: &str,
    github_token: &str,
) -> Result<String, String> {
    let repo_host_path = repo_url
        .strip_prefix("https://")
        .ok_or_else(|| format!("repo_url must start with https:// (got: {repo_url})"))?;

    ctx.log(
        "info",
        &format!("sandbox_provisioner: cloning {repo_url} (ref={repo_ref}) into $HOME/repo"),
    );

    // rm -rf first: a stale ~/repo left over from an earlier attempt on the
    // same sandbox (e.g. after a checkpoint/restore) must not silently mask
    // a fresh clone failure by leaving the directory present after `git
    // clone` itself failed.
    let (clone_cmd, process_env) = if github_token.is_empty() {
        // Public repositories do not need a token. Avoid injecting an empty
        // Git credential so Git can use the original unauthenticated URL.
        (
            format!(
                "echo \"TEMPER_HOME=$HOME\" && echo \"TEMPER_GITHUB_TOKEN_BYTES=0\" && cd ~ && rm -rf repo && git clone {repo_url} repo && cd repo && (git checkout {repo_ref} 2>/dev/null || true)"
            ),
            json!({}),
        )
    } else {
        // Do not expand the token into a Basic-auth URL: that would put it in
        // git's argv and, for this fine-grained PAT, fails in Tensorlake even
        // though the same credential works locally. GIT_ASKPASS supplies the
        // password from the command-scoped environment and the temporary
        // helper is removed when this shell process exits.
        (
            format!(
                "echo \"TEMPER_HOME=$HOME\" && echo \"TEMPER_GITHUB_TOKEN_BYTES=$(printf %s \\\"$GITHUB_TOKEN\\\" | wc -c | tr -d ' ')\" && cd ~ && rm -rf repo && askpass=$(mktemp) && trap 'rm -f \"$askpass\"' EXIT && printf '%s\\n' '#!/bin/sh' 'printf %s \"$GITHUB_TOKEN\"' > \"$askpass\" && chmod 700 \"$askpass\" && GIT_ASKPASS=\"$askpass\" GIT_ASKPASS_REQUIRE=force GIT_TERMINAL_PROMPT=0 git clone https://x-access-token@{repo_host_path} repo && cd repo && (git checkout {repo_ref} 2>/dev/null || true)"
            ),
            json!({"GITHUB_TOKEN": github_token}),
        )
    };

    let api_key = resolved_config(ctx, "tensorlake_api_key").unwrap_or_default();
    let url = format!("{sandbox_url}/api/v1/processes/run");
    let body = serde_json::to_string(&json!({
        "command": "bash",
        "args": ["-c", clone_cmd],
        "working_dir": "/",
        "env": process_env,
    }))
    .unwrap_or_default();
    let headers = vec![
        ("authorization".to_string(), format!("Bearer {api_key}")),
        ("content-type".to_string(), "application/json".to_string()),
    ];
    let resp = ctx.http_call("POST", &url, &headers, &body)?;
    if resp.status < 200 || resp.status >= 300 {
        return Err(format!(
            "git clone failed (HTTP {}): {}",
            resp.status,
            &resp.body[..resp.body.len().min(300)]
        ));
    }

    let (stdout, stderr, exit_code) = parse_tensorlake_sse(&resp.body);
    let token_bytes = stdout
        .lines()
        .find_map(|line| line.strip_prefix("TEMPER_GITHUB_TOKEN_BYTES="))
        .unwrap_or("unavailable");
    ctx.log(
        "info",
        &format!("sandbox_provisioner: Tensorlake clone process received github token bytes={token_bytes}"),
    );
    if exit_code != 0 {
        return Err(format!("git clone failed (exit {exit_code}): {stderr}"));
    }

    let home_dir = stdout
        .lines()
        .find_map(|line| line.strip_prefix("TEMPER_HOME="))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            format!("git clone succeeded but could not determine $HOME from output: {stdout}")
        })?;

    Ok(format!("{home_dir}/repo"))
}

/// Parse a Tensorlake sandbox `/api/v1/processes/run` Server-Sent Events
/// response into `(stdout, stderr, exit_code)`.
///
/// Verified frame shapes (scripts/tl-probe.sh):
///   data: {"handle":1,"pid":536,"started_at":...}            -- ignored
///   data: {"line":"...","timestamp":...,"stream":"stdout"}   -- appended
///   data: {"line":"...","timestamp":...,"stream":"stderr"}   -- appended
///   data: {"exit_code":0}                                     -- terminal frame
///
/// If no exit frame is observed the exit code is reported as `-1` (unknown),
/// matching the "parse failed" convention used elsewhere for local/E2B exec.
fn parse_tensorlake_sse(body: &str) -> (String, String, i64) {
    let mut stdout = String::new();
    let mut stderr = String::new();
    let mut exit_code: i64 = -1;

    for raw_line in body.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        // Two different shapes can reach here depending on a layer this
        // module has no control over: Temper's own WASM host
        // (crates/temper-wasm/src/host_trait.rs) auto-detects SSE responses
        // by Content-Type and, when it does, already strips the `data: `
        // prefix and blank-line event framing before handing the body to
        // this module — leaving bare JSON objects one per line. If that
        // detection doesn't fire, the raw wire format (`data: {...}`, as
        // seen directly via curl in scripts/tl-probe.sh, which never goes
        // through that host layer) comes through unchanged. Assuming only
        // one of these previously produced a *silent* empty result (exit
        // code -1, no stdout/stderr) instead of a clear parse error — every
        // line failed strip_prefix, nothing was ever extracted, and nothing
        // said so.
        let json_part = line.strip_prefix("data:").map(str::trim).unwrap_or(line);
        if json_part.is_empty() {
            continue;
        }
        let Ok(frame) = serde_json::from_str::<Value>(json_part) else {
            continue;
        };

        if let Some(code) = frame.get("exit_code") {
            exit_code = code.as_i64().unwrap_or(-1);
            continue;
        }
        if let Some(text) = frame.get("line").and_then(|v| v.as_str()) {
            let stream = frame.get("stream").and_then(|v| v.as_str()).unwrap_or("stdout");
            let target = if stream == "stderr" { &mut stderr } else { &mut stdout };
            if !target.is_empty() {
                target.push('\n');
            }
            target.push_str(text);
        }
        // "handle"/"pid"/"started_at" start-of-process frames: ignored.
    }

    (stdout, stderr, exit_code)
}

/// Read an integration config value, treating an unresolved `{secret:NAME}`
/// template as absent.
///
/// `resolve_secret_templates` leaves the literal pattern in place when a secret
/// is missing (see `temper-server/src/secrets/template.rs`), so `config.get()`
/// returns `Some("{secret:...}")` rather than `None`. Without this filter a
/// `unwrap_or_else` default never fires and the raw template reaches the wire.
fn resolved_config(ctx: &Context, key: &str) -> Option<String> {
    ctx.config
        .get(key)
        .map(String::as_str)
        .filter(|value| !value.trim().is_empty() && !value.contains("{secret:"))
        .map(str::to_string)
}

/// Sanitize a candidate sandbox name to satisfy Tensorlake's naming rule
/// (verified via `tl sbx create --help`): must start with a lowercase
/// letter, contain only lowercase letters, digits, and hyphens, not end
/// with a hyphen, max 63 chars, and not be exactly 21 lowercase
/// alphanumeric characters (ambiguous with a sandbox ID). Temper run IDs
/// look like `run_<32 hex chars>` — the underscore alone fails creation
/// with HTTP 400 ("contains invalid character '_'"), which is what
/// surfaced this in the first place.
fn sanitize_sandbox_name(raw: &str) -> String {
    let mut out: String = raw
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() {
                c
            } else {
                '-'
            }
        })
        .collect();

    out.truncate(63);
    while out.ends_with('-') {
        out.pop();
    }

    if !out.chars().next().is_some_and(|c| c.is_ascii_lowercase()) {
        out = format!("s-{out}");
        out.truncate(63);
        while out.ends_with('-') {
            out.pop();
        }
    }

    if out.len() == 21 && out.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()) {
        out.push('x');
    }

    if out.is_empty() {
        out = "sandbox".to_string();
    }

    out
}
