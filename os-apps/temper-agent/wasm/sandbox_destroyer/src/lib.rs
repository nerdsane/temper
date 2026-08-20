//! Sandbox Destroyer — WASM module for cancelling or deleting sandboxes.
//!
//! Reads sandbox_id and sandbox_provider from entity state and destroys
//! the sandbox via the appropriate backend API:
//!   - local: no-op (local sandbox is shared, not per-run)
//!   - E2B: DELETE https://api.e2b.dev/sandboxes/{sandbox_id}
//!   - Tensorlake: DELETE {api_url}/sandboxes/{sandbox_id}
//!
//! Cancellation remains best-effort because the run is already terminal. A
//! deletion request is stricter: failure transitions the run to
//! `DeletionFailed`, retaining a retryable record rather than claiming the
//! run was deleted while provider compute remains allocated.
//!
//! Build: `cargo build --target wasm32-unknown-unknown --release`

use temper_wasm_sdk::prelude::*;

/// Entry point.
#[unsafe(no_mangle)]
pub extern "C" fn run(_ctx_ptr: i32, _ctx_len: i32) -> i32 {
    let ctx = match Context::from_host() {
        Ok(ctx) => ctx,
        Err(_) => return 1,
    };
    ctx.log("info", "sandbox_destroyer: starting");

    let fields = ctx.entity_state.get("fields").cloned().unwrap_or(json!({}));
    let deletion_requested = ctx
        .entity_state
        .get("status")
        .and_then(|value| value.as_str())
        == Some("Deleting");
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
        "agent.deletion_requested": deletion_requested,
    }));

    let teardown = match teardown_target(sandbox_provider, sandbox_id, sandbox_url) {
        TeardownTarget::Noop => {
            ctx.log("info", "sandbox_destroyer: local/static sandbox — no teardown needed");
            Ok(())
        }
        TeardownTarget::MissingRemoteId => Err(format!(
            "remote sandbox provider {sandbox_provider:?} has no persisted sandbox_id; refusing successful teardown"
        )),
        TeardownTarget::Tensorlake | TeardownTarget::E2b => {
            ctx.log(
                "info",
                &format!(
                    "sandbox_destroyer: destroying sandbox {sandbox_id} (provider={sandbox_provider})"
                ),
            );
            match teardown_target(sandbox_provider, sandbox_id, sandbox_url) {
                TeardownTarget::Tensorlake => destroy_tensorlake(&ctx, &fields, sandbox_id),
                TeardownTarget::E2b => destroy_e2b(&ctx, sandbox_id),
                _ => unreachable!("teardown target changed after classification"),
            }
        }
        TeardownTarget::UnsupportedRemote => Err(format!(
            "unsupported remote sandbox provider {sandbox_provider:?} for sandbox {sandbox_id:?}"
        )),
    };

    match teardown {
        Ok(()) => {
            ctx.log("info", "sandbox_destroyer: teardown complete");
            if deletion_requested {
                set_success_result("DeletionTeardownSucceeded", &json!({}));
            } else {
                // Cancellation is best-effort: the run is already Cancelled,
                // so emit an empty callback rather than an invalid second
                // Cancel transition from a terminal state.
                set_success_result("", &json!({}));
            }
        }
        Err(error) if deletion_requested => {
            let error_message: String = error.chars().take(300).collect();
            ctx.log(
                "warn",
                &format!("sandbox_destroyer: deletion teardown failed: {error_message}"),
            );
            set_success_result(
                "DeletionTeardownFailed",
                &json!({ "error_message": error_message }),
            );
        }
        Err(error) => {
            // Cancellation is best-effort: the run is already Cancelled, so
            // preserve its existing semantics and report the teardown failure
            // through telemetry without blocking the terminal transition.
            ctx.log(
                "warn",
                &format!("sandbox_destroyer: cancellation teardown failed (non-fatal): {error}"),
            );
            set_success_result("", &json!({}));
        }
    }
    0
}

/// Provider operation selected from persisted allocation identity.
#[derive(Debug, PartialEq, Eq)]
enum TeardownTarget {
    /// An explicit local/static sandbox never owns per-run provider compute.
    Noop,
    /// A remote provider might own compute but its allocation ID was lost.
    MissingRemoteId,
    /// Delete through Tensorlake's control-plane API.
    Tensorlake,
    /// Delete through E2B's control-plane API.
    E2b,
    /// A non-local provider has no supported teardown route.
    UnsupportedRemote,
}

/// Classify teardown without ever treating ambiguous remote allocation as safe.
fn teardown_target(provider: &str, sandbox_id: &str, sandbox_url: &str) -> TeardownTarget {
    let is_e2b = sandbox_url.contains("e2b.app") || sandbox_url.contains("e2b.dev");
    if sandbox_id == "static-sandbox" {
        TeardownTarget::Noop
    } else if sandbox_id.is_empty() && (provider != "local" || is_e2b) {
        // Historical E2B runs may still say `local`; their URL is the durable
        // provider evidence and must not be discarded by the local fast path.
        TeardownTarget::MissingRemoteId
    } else if sandbox_id.is_empty() {
        TeardownTarget::Noop
    } else if provider == "tensorlake" {
        TeardownTarget::Tensorlake
    } else if is_e2b {
        TeardownTarget::E2b
    } else if provider == "local" {
        TeardownTarget::Noop
    } else {
        TeardownTarget::UnsupportedRemote
    }
}

/// Destroy a Tensorlake sandbox via DELETE /sandboxes/:id (verified: no /v2
/// prefix — that segment only appears in the docs URL, not the API path).
fn destroy_tensorlake(
    ctx: &Context,
    _fields: &Value,
    sandbox_id: &str,
) -> Result<(), String> {
    let api_key = ctx.config.get("tensorlake_api_key").cloned().unwrap_or_default();

    if api_key.is_empty() || api_key.contains("{secret:") {
        return Err("tensorlake_api_key not set — cannot destroy sandbox".to_string());
    }

    let api_url = resolved_config(ctx, "tensorlake_api_url")
        .unwrap_or_else(|| "https://api.tensorlake.ai".to_string());

    let url = format!("{api_url}/sandboxes/{sandbox_id}");
    let headers = vec![
        ("Authorization".to_string(), format!("Bearer {api_key}")),
    ];

    let resp = ctx.http_call("DELETE", &url, &headers, "")?;

    if (resp.status >= 200 && resp.status < 300) || resp.status == 404 {
        ctx.log("info", "sandbox_destroyer: Tensorlake sandbox absent or destroyed");
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
        return Err("e2b_api_key not set — cannot destroy sandbox".to_string());
    }

    let api_url = resolved_config(ctx, "e2b_api_url")
        .unwrap_or_else(|| "https://api.e2b.dev".to_string());

    let url = format!("{api_url}/sandboxes/{sandbox_id}");
    let headers = vec![
        ("x-api-key".to_string(), api_key),
    ];

    let resp = ctx.http_call("DELETE", &url, &headers, "")?;

    if (resp.status >= 200 && resp.status < 300) || resp.status == 404 {
        ctx.log("info", "sandbox_destroyer: E2B sandbox absent or destroyed");
        Ok(())
    } else {
        Err(format!(
            "E2B destroy failed (HTTP {}): {}",
            resp.status,
            &resp.body[..resp.body.len().min(200)]
        ))
    }
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

#[cfg(test)]
mod teardown_target_tests {
    use super::{TeardownTarget, teardown_target};

    #[test]
    fn remote_missing_id_fails_closed() {
        assert_eq!(
            teardown_target("tensorlake", "", ""),
            TeardownTarget::MissingRemoteId
        );
        assert_eq!(
            teardown_target("e2b", "", "https://49983-id.e2b.app"),
            TeardownTarget::MissingRemoteId
        );
    }

    #[test]
    fn explicit_local_and_static_targets_are_safe_noops() {
        assert_eq!(
            teardown_target("local", "", ""),
            TeardownTarget::Noop
        );
        assert_eq!(
            teardown_target("tensorlake", "static-sandbox", ""),
            TeardownTarget::Noop
        );
    }

    #[test]
    fn provider_targets_remain_distinct() {
        assert_eq!(
            teardown_target("tensorlake", "tl-123", "https://tl-123.sandbox.tensorlake.ai"),
            TeardownTarget::Tensorlake
        );
        assert_eq!(
            teardown_target("e2b", "e2b-123", "https://49983-e2b-123.e2b.app"),
            TeardownTarget::E2b
        );
        // Backward compatibility: earlier E2B allocation did not persist
        // provider and therefore retained the default `local` value.
        assert_eq!(
            teardown_target("local", "e2b-legacy", "https://49983-e2b-legacy.e2b.app"),
            TeardownTarget::E2b
        );
    }
}
