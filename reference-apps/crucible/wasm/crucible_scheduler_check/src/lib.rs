//! Crucible Scheduler Check — queries active SessionSchedules and
//! fires due ones by dispatching their Trigger action.
//!
//! Fired by `CrucibleScheduler.ScheduledCheck`. For each active
//! schedule, POSTs the `Trigger` bound action if the schedule is due.
//!
//! Due = schedule is Active (the OData filter handles this).
//! Actual cron expression evaluation is deferred — the scheduler
//! fires ALL active schedules on each check cycle and lets the
//! `crucible_cron_trigger` module decide whether to skip based on
//! session status. A future version can add server-side cron
//! parsing to only fire truly due schedules.

use temper_wasm_sdk::prelude::*;

temper_module! {
    fn run(ctx: Context) -> Result<Value> {
        let temper_url = ctx.config.get("temper_api_url")
            .cloned().unwrap_or_else(|| "http://127.0.0.1:3000".into());
        let tenant = &ctx.tenant;
        let headers = vec![("X-Tenant-Id".to_string(), tenant.to_string())];
        let post_headers = vec![
            ("X-Tenant-Id".to_string(), tenant.to_string()),
            ("Content-Type".to_string(), "application/json".to_string()),
        ];

        // Query all active SessionSchedules.
        let resp = ctx.http_call("GET",
            &format!("{temper_url}/tdata/SessionSchedules?$filter=Status%20eq%20'Active'"),
            &headers, "")?;
        if resp.status != 200 {
            return Err(format!("GET SessionSchedules failed: HTTP {}", resp.status));
        }

        let body: Value = serde_json::from_str(&resp.body)
            .map_err(|e| format!("parse schedules: {e}"))?;
        let schedules = body.get("value")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let mut triggered = 0i64;
        for sched in &schedules {
            let sched_id = sched.get("entity_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if sched_id.is_empty() { continue; }

            // Fire the Trigger action.
            let url = format!(
                "{temper_url}/tdata/SessionSchedules('{sched_id}')/Temper.Crucible.Trigger"
            );
            let resp = ctx.http_call("POST", &url, &post_headers, "{}");
            match resp {
                Ok(r) if r.status < 400 => {
                    triggered += 1;
                    ctx.log("info", &format!("Triggered schedule {sched_id}"));
                }
                Ok(r) => {
                    ctx.log("warn", &format!(
                        "Trigger {sched_id} failed: HTTP {} {}",
                        r.status, &r.body[..r.body.len().min(200)]
                    ));
                }
                Err(e) => {
                    ctx.log("warn", &format!("Trigger {sched_id} error: {e}"));
                }
            }
        }

        ctx.log("info", &format!(
            "Check complete: {}/{} schedules triggered",
            triggered, schedules.len()
        ));

        Ok(json!({"schedules_triggered": triggered}))
    }
}
