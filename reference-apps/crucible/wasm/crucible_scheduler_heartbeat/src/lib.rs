//! Crucible Scheduler Heartbeat — waits N seconds then re-triggers
//! ScheduledCheck on the CrucibleScheduler entity.
//!
//! This creates the self-perpetuating loop:
//!   ScheduledCheck → check WASM → CheckComplete
//!   → heartbeat WASM (wait) → ScheduledCheck → ...
//!
//! The wait uses the Temper observe long-poll endpoint with an
//! impossible status filter, so it blocks for exactly the configured
//! interval.

use temper_wasm_sdk::prelude::*;

temper_module! {
    fn run(ctx: Context) -> Result<Value> {
        let fields = ctx.entity_state.get("fields").cloned().unwrap_or(json!({}));
        let interval = fields.get("HeartbeatIntervalSeconds")
            .and_then(|v| v.as_i64())
            .unwrap_or(30)
            .clamp(5, 300);

        let temper_url = ctx.config.get("temper_api_url")
            .cloned().unwrap_or_else(|| "http://127.0.0.1:3000".into());
        let tenant = &ctx.tenant;
        let entity_id = &ctx.entity_id;
        let headers = vec![("X-Tenant-Id".to_string(), tenant.to_string())];

        let interval_ms = interval * 1000;

        // Long-poll wait — blocks for `interval` seconds.
        // Uses an impossible status filter so it always times out after
        // the specified duration, implementing a sleep.
        let wait_url = format!(
            "{temper_url}/observe/entities/CrucibleScheduler/{entity_id}/wait?statuses=__never__&timeout_ms={interval_ms}&poll_ms=250"
        );
        let _ = ctx.http_call("GET", &wait_url, &headers, "");

        // Re-trigger ScheduledCheck on ourselves.
        let post_headers = vec![
            ("X-Tenant-Id".to_string(), tenant.to_string()),
            ("Content-Type".to_string(), "application/json".to_string()),
        ];
        let check_url = format!(
            "{temper_url}/tdata/CrucibleSchedulers('{entity_id}')/Temper.Crucible.ScheduledCheck"
        );
        let _ = ctx.http_call("POST", &check_url, &post_headers, "{}");

        Ok(json!({}))
    }
}
