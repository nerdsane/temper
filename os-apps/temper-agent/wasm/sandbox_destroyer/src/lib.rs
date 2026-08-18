//! Sandbox Destroyer — WASM module for tearing down sandboxes on Cancel.
//!
//! Reads sandbox_id and sandbox_provider from entity state and destroys
//! the sandbox via the appropriate backend API:
//!   - local: no-op (local sandbox is shared, not per-run)
//!   - E2B: DELETE https://api.e2b.dev/sandboxes/{sandbox_id}
//!   - Tensorlake: DELETE {api_url}/v2/sandboxes/{sandbox_id}
//!
//! On failure, logs a warning but does not fail the Cancel transition
//! (on_failure = "Log" in the IOA spec). The run is already Cancelled;
//! a teardown failure should not block the terminal state.
//!
//! Build: `cargo build --target wasm32-unknown-unknown --release`

use temper_wasm_sdk::prelude::*;

/// Entry point.
#[unsafe(no_mangle)]
pub extern "C" fn run(_ctx_ptr: i32, _ctx_len: i32) -> i32 {
    let result = (|| -> Result<(), String> {
        let ctx = Context::from_host()?;
        ctx.log("info", "sandbox_destroyer: starting");

        let fields = ctx.entity_state.get("fields").cloned().unwrap_or(json!({}));

        let sandbox_id = fields
            .get("sandbox_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let sandbox_url = fields
            .get("sandbox_url")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let sandbox_provider = fields
            .get("sandbox_provider")
            .and_then(|v| v.as_str())
            .unwrap_or("local");

        // Emit structured telemetry for sandbox teardown.
        let _ = ctx.log_structured("info", "sandbox.destroy", &json!({
            "agent.run_id": ctx.entity_state.get("entity_id").and_then(|v| v.as_str()).unwrap_or(""),
            "agent.sandbox_id": sandbox_id,
            "agent.sandbox_provider": sandbox_provider,
        }));

        if sandbox_id.is_empty() || sandbox_id == "static-sandbox" {
            ctx.log(
                "info",
                "sandbox_destroyer: no sandbox to destroy (local or not provisioned)",
            );
            // Return success — Cancel should proceed even if no sandbox exists.
            set_success_result("Cancel", &json!({}));
            return Ok(());
        }

        ctx.log(
            "info",
            &format!(
                "sandbox_destroyer: destroying sandbox {sandbox_id} (provider={sandbox_provider})"
            ),
        );

        match sandbox_provider {
            "tensorlake" => destroy_tensorlake(&ctx, &fields, sandbox_id),
            _ => {
                // Local sandbox: no-op (shared sandbox, not per-run).
                // E2B: attempt DELETE if URL contains e2b.
                if sandbox_url.contains("e2b.app") || sandbox_url.contains("e2b.dev") {
                    destroy_e2b(&ctx, sandbox_id)
                } else {
                    ctx.log(
                        "info",
                        "sandbox_destroyer: local sandbox — no teardown needed",
                    );
                    Ok(())
                }
            }
        }?;

        ctx.log("info", "sandbox_destroyer: teardown complete");

        // Return success so the Cancel transition completes.
        set_success_result("Cancel", &json!({}));
        Ok(())
    })();

    if let Err(e) = result {
        // Log the error but still return success — Cancel must not be blocked
        // by a teardown failure. The run is already transitioning to Cancelled.
        let ctx = Context::from_host();
        if let Ok(ctx) = ctx {
            ctx.log(
                "warn",
                &format!("sandbox_destroyer: teardown failed (non-fatal): {e}"),
            );
        }
        // Still set success so Cancel proceeds.
        set_success_result("Cancel", &json!({}));
    }
    0
}

/// Destroy a Tensorlake sandbox via DELETE /v2/sandboxes/:id.
fn destroy_tensorlake(
    ctx: &Context,
    _fields: &Value,
    sandbox_id: &str,
) -> Result<(), String> {
    let api_key = ctx.config.get("tensorlake_api_key").cloned().unwrap_or_default();

    if api_key.is_empty() || api_key.contains("{secret:") {
        ctx.log(
            "warn",
            "sandbox_destroyer: tensorlake_api_key not set — cannot destroy sandbox",
        );
        return Ok(()); // Non-fatal
    }

    let api_url = ctx
        .config
        .get("tensorlake_api_url")
        .cloned()
        .unwrap_or_else(|| "https://api.tensorlake.ai".to_string());

    let url = format!("{api_url}/v2/sandboxes/{sandbox_id}");
    let headers = vec![
        ("Authorization".to_string(), format!("Bearer {api_key}")),
    ];

    let resp = ctx.http_call("DELETE", &url, &headers, "")?;

    if resp.status >= 200 && resp.status < 300 {
        ctx.log("info", "sandbox_destroyer: Tensorlake sandbox destroyed");
        Ok(())
    } else {
        Err(format!(
            "Tensorlake destroy failed (HTTP {}): {}",
            resp.status,
            &resp.body[..resp.body.len().min(200)]
        ))
    }
}

/// Destroy an E2B sandbox via DELETE /sandboxes/:id.
fn destroy_e2b(ctx: &Context, sandbox_id: &str) -> Result<(), String> {
    let api_key = ctx.config.get("e2b_api_key").cloned().unwrap_or_default();

    if api_key.is_empty() || api_key.contains("{secret:") {
        ctx.log(
            "warn",
            "sandbox_destroyer: e2b_api_key not set — cannot destroy sandbox",
        );
        return Ok(()); // Non-fatal
    }

    let api_url = ctx
        .config
        .get("e2b_api_url")
        .cloned()
        .unwrap_or_else(|| "https://api.e2b.dev".to_string());

    let url = format!("{api_url}/sandboxes/{sandbox_id}");
    let headers = vec![
        ("x-api-key".to_string(), api_key),
    ];

    let resp = ctx.http_call("DELETE", &url, &headers, "")?;

    if resp.status >= 200 && resp.status < 300 {
        ctx.log("info", "sandbox_destroyer: E2B sandbox destroyed");
        Ok(())
    } else {
        Err(format!(
            "E2B destroy failed (HTTP {}): {}",
            resp.status,
            &resp.body[..resp.body.len().min(200)]
        ))
    }
}
