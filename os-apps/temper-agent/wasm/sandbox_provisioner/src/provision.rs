//! Sandbox connect / create for the provisioner guest.

use temper_wasm_sdk::prelude::*;

use crate::named_sandbox;

/// Result of a successful sandbox connect or create.
pub struct SandboxResult {
    /// HTTP URL of the sandbox.
    pub sandbox_url: String,
    /// Id recorded on SandboxReady.
    pub sandbox_id: String,
}

/// Resolve Temper API base URL from entity fields or integration config.
pub fn resolve_temper_api_url(ctx: &Context, fields: &Value) -> String {
    fields
        .get("temper_api_url")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(
            || match ctx.config.get("temper_api_url").map(String::as_str) {
                Some(value) if !value.trim().is_empty() && !value.contains("{secret:") => {
                    Some(value.to_string())
                }
                _ => None,
            },
        )
        .unwrap_or_else(|| "http://127.0.0.1:3000".to_string())
}

/// Provision a sandbox. Priority order:
/// 1. usable sandbox_url from entity state, integration config, or trigger params
/// 2. named sandbox (TEMPER_SANDBOX_URL); name-only fails closed
/// 3. E2B REST API (requires e2b_api_key in integration config)
pub fn provision_sandbox(ctx: &Context) -> Result<SandboxResult, String> {
    let fields = ctx.entity_state.get("fields").cloned().unwrap_or(json!({}));

    // Priority 1: sandbox_url from entity state (set at Configure time) or config.
    // Unresolved `{secret:...}` templates are unset, not used as a URL.
    let static_url = named_sandbox::first_usable([
        fields.get("sandbox_url").and_then(|v| v.as_str()),
        ctx.config.get("sandbox_url").map(String::as_str),
        ctx.trigger_params
            .get("sandbox_url")
            .and_then(|v| v.as_str()),
    ]);
    if let Some(url) = static_url {
        ctx.log(
            "info",
            &format!("sandbox_provisioner: using static sandbox_url: {url}"),
        );
        return Ok(SandboxResult {
            sandbox_url: url.to_string(),
            sandbox_id: "static-sandbox".to_string(),
        });
    }

    // Priority 2: gated named sandbox. Empty name+URL keeps the E2B path.
    // Name without URL fails closed so dsf is never silently replaced by E2B.
    let named = named_sandbox::NamedSandboxDecision::from_name_and_url(
        named_sandbox::first_usable([
            fields.get("temper_sandbox_name").and_then(|v| v.as_str()),
            ctx.config.get("temper_sandbox_name").map(String::as_str),
            ctx.trigger_params
                .get("temper_sandbox_name")
                .and_then(|v| v.as_str()),
        ]),
        named_sandbox::first_usable([
            fields.get("temper_sandbox_url").and_then(|v| v.as_str()),
            ctx.config.get("temper_sandbox_url").map(String::as_str),
            ctx.trigger_params
                .get("temper_sandbox_url")
                .and_then(|v| v.as_str()),
        ]),
    );
    match named {
        named_sandbox::NamedSandboxDecision::Connect { url, sandbox_id } => {
            ctx.log(
                "info",
                &format!("sandbox_provisioner: using named sandbox id={sandbox_id} url={url}"),
            );
            return Ok(SandboxResult {
                sandbox_url: url,
                sandbox_id,
            });
        }
        named_sandbox::NamedSandboxDecision::FailClosed { name } => {
            return Err(named_sandbox::NamedSandboxDecision::fail_closed_message(
                &name,
            ));
        }
        named_sandbox::NamedSandboxDecision::Unset => {}
    }

    create_e2b_sandbox(ctx)
}

fn create_e2b_sandbox(ctx: &Context) -> Result<SandboxResult, String> {
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
