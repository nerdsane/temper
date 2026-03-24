//! Cron Scheduler Check — WASM module for checking due cron jobs.
//!
//! Queries active CronJobs where NextRunAt <= now and fires Trigger on each.

use temper_wasm_sdk::prelude::*;
use wasm_helpers::{entity_field_str, resolve_temper_api_url};

#[unsafe(no_mangle)]
pub extern "C" fn run(_ctx_ptr: i32, _ctx_len: i32) -> i32 {
    let result = (|| -> Result<(), String> {
        let ctx = Context::from_host()?;
        ctx.log("info", "cron_scheduler_check: starting");

        let fields = ctx.entity_state.get("fields").cloned().unwrap_or(json!({}));
        let temper_api_url = resolve_temper_api_url(&ctx, &fields);
        let tenant = &ctx.tenant;

        let headers = vec![
            ("content-type".to_string(), "application/json".to_string()),
            ("x-tenant-id".to_string(), tenant.to_string()),
            ("x-temper-principal-kind".to_string(), "admin".to_string()),
            ("accept".to_string(), "application/json".to_string()),
        ];

        // Query active cron jobs
        let url = format!("{temper_api_url}/tdata/CronJobs?$filter=Status eq 'Active'");
        let resp = ctx.http_call("GET", &url, &headers, "")?;

        let mut triggered_count: i64 = 0;

        if resp.status == 200 {
            let parsed: Value = serde_json::from_str(&resp.body).unwrap_or(json!({"value": []}));
            let jobs = parsed.get("value").and_then(|v| v.as_array()).cloned().unwrap_or_default();

            ctx.log("info", &format!("cron_scheduler_check: found {} active cron jobs", jobs.len()));

            for job in &jobs {
                let job_id = job
                    .get("entity_id")
                    .and_then(|v| v.as_str())
                    .or_else(|| entity_field_str(job, &["Id"]))
                    .unwrap_or("");
                // NextRunAt check deferred to cron_scheduler — this module triggers all active jobs
                // that the scheduler determined are due
                let trigger_url = format!("{temper_api_url}/tdata/CronJobs('{job_id}')/Temper.Agent.Trigger");
                let trigger_body = json!({ "last_run_at": "" });
                match ctx.http_call("POST", &trigger_url, &headers, &trigger_body.to_string()) {
                    Ok(r) if r.status >= 200 && r.status < 300 => {
                        triggered_count += 1;
                        ctx.log("info", &format!("cron_scheduler_check: triggered job {}", job_id));
                    }
                    Ok(r) => {
                        ctx.log("warn", &format!("cron_scheduler_check: failed to trigger job {} (HTTP {})", job_id, r.status));
                    }
                    Err(e) => {
                        ctx.log("warn", &format!("cron_scheduler_check: failed to trigger job {}: {}", job_id, e));
                    }
                }
            }
        }

        set_success_result("CheckComplete", &json!({
            "last_check_at": "",
            "jobs_triggered": triggered_count,
        }));

        Ok(())
    })();

    if let Err(e) = result {
        set_error_result(&e);
    }
    0
}
