use temper_wasm_sdk::prelude::*;
use wasm_helpers::resolve_temper_api_url;

#[unsafe(no_mangle)]
pub extern "C" fn run(_ctx_ptr: i32, _ctx_len: i32) -> i32 {
    let result = (|| -> Result<(), String> {
        let ctx = Context::from_host()?;
        let fields = ctx.entity_state.get("fields").cloned().unwrap_or_else(|| json!({}));
        let interval_seconds = fields
            .get("scan_interval_seconds")
            .and_then(|v| v.as_str())
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(30)
            .clamp(1, 300);
        let base_url = resolve_temper_api_url(&ctx, &fields);
        let headers = vec![
            ("x-tenant-id".to_string(), ctx.tenant.clone()),
            ("x-temper-principal-kind".to_string(), "admin".to_string()),
            ("accept".to_string(), "application/json".to_string()),
            ("content-type".to_string(), "application/json".to_string()),
        ];

        let wait_url = format!(
            "{base_url}/observe/entities/{}/{}/wait?statuses=__never__&timeout_ms={}&poll_ms=250",
            ctx.entity_type,
            ctx.entity_id,
            interval_seconds * 1000
        );
        let _ = ctx.http_call("GET", &wait_url, &headers, "")?;

        let action_url = format!(
            "{base_url}/tdata/HeartbeatMonitors('{}')/Temper.Agent.HeartbeatMonitor.ScheduledScan",
            ctx.entity_id
        );
        let _ = ctx.http_call("POST", &action_url, &headers, "{}")?;

        set_success_result("ScheduleFailed", &json!({
            "error_message": "",
        }));
        Ok(())
    })();

    if let Err(error) = result {
        set_error_result(&error);
    }
    0
}

