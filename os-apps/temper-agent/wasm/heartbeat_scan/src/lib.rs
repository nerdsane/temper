//! Heartbeat Scanner — WASM module for detecting stale agents.
//!
//! Queries TemperAgent entities in non-terminal states, checks heartbeat freshness,
//! and fires TimeoutFail on stale ones.

use temper_wasm_sdk::prelude::*;
use wasm_helpers::{entity_field_str, parse_iso8601_to_epoch_secs, resolve_temper_api_url};

#[unsafe(no_mangle)]
pub extern "C" fn run(_ctx_ptr: i32, _ctx_len: i32) -> i32 {
    let result = (|| -> Result<(), String> {
        let ctx = Context::from_host()?;
        ctx.log("info", "heartbeat_scan: starting");

        let fields = ctx.entity_state.get("fields").cloned().unwrap_or(json!({}));
        let temper_api_url = resolve_temper_api_url(&ctx, &fields);
        let tenant = &ctx.tenant;

        // Get scanner's reference timestamp for "now"
        let scan_started_at = fields
            .get("last_scan_at")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let now_secs = parse_iso8601_to_epoch_secs(scan_started_at).unwrap_or(0);

        let headers = vec![
            ("x-tenant-id".to_string(), tenant.to_string()),
            ("x-temper-principal-kind".to_string(), "admin".to_string()),
            ("accept".to_string(), "application/json".to_string()),
        ];

        // Query agents in non-terminal states
        let filter = "$filter=Status ne 'Completed' and Status ne 'Failed' and Status ne 'Cancelled' and Status ne 'Created'";
        let url = format!("{temper_api_url}/tdata/TemperAgents?{filter}");
        let resp = ctx.http_call("GET", &url, &headers, "")?;

        let mut stale_count: i64 = 0;

        if resp.status == 200 {
            let parsed: Value = serde_json::from_str(&resp.body).unwrap_or(json!({"value": []}));
            let agents = parsed.get("value").and_then(|v| v.as_array()).cloned().unwrap_or_default();

            ctx.log("info", &format!("heartbeat_scan: checking {} active agents", agents.len()));

            for agent in &agents {
                let agent_id = agent
                    .get("entity_id")
                    .and_then(|v| v.as_str())
                    .or_else(|| entity_field_str(agent, &["Id"]))
                    .unwrap_or("");
                let last_heartbeat =
                    entity_field_str(agent, &["LastHeartbeatAt"]).unwrap_or("");
                let timeout_secs: u64 = entity_field_str(agent, &["HeartbeatTimeoutSeconds"])
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(300);

                // Skip agents without heartbeat monitoring configured.
                if timeout_secs == 0 {
                    continue;
                }

                let is_stale = if last_heartbeat.is_empty() {
                    // No heartbeat ever observed — stale
                    true
                } else if now_secs > 0 {
                    // Compare heartbeat timestamp against current time
                    match parse_iso8601_to_epoch_secs(last_heartbeat) {
                        Some(hb_secs) => now_secs.saturating_sub(hb_secs) > timeout_secs,
                        None => {
                            ctx.log("warn", &format!(
                                "heartbeat_scan: agent {} has unparseable heartbeat timestamp '{}'",
                                agent_id, last_heartbeat
                            ));
                            false
                        }
                    }
                } else {
                    // No reference time available; only flag agents with no heartbeat at all
                    ctx.log("info", &format!(
                        "heartbeat_scan: agent {} has heartbeat '{}' but no scan reference time, skipping comparison",
                        agent_id, last_heartbeat
                    ));
                    false
                };

                if is_stale {
                    let fail_url = format!(
                        "{temper_api_url}/tdata/TemperAgents('{agent_id}')/Temper.Agent.TemperAgent.TimeoutFail"
                    );
                    let elapsed_msg = if last_heartbeat.is_empty() {
                        "no heartbeat observed".to_string()
                    } else {
                        let hb_secs = parse_iso8601_to_epoch_secs(last_heartbeat).unwrap_or(0);
                        format!("last heartbeat {}s ago", now_secs.saturating_sub(hb_secs))
                    };
                    let fail_body = json!({
                        "error_message": format!(
                            "heartbeat timeout: {} (timeout: {}s)",
                            elapsed_msg, timeout_secs
                        )
                    });
                    match ctx.http_call("POST", &fail_url, &headers, &fail_body.to_string()) {
                        Ok(resp) if resp.status >= 200 && resp.status < 300 => {
                            stale_count += 1;
                            ctx.log(
                                "warn",
                                &format!("heartbeat_scan: failed stale agent {}", agent_id),
                            );
                        }
                        Ok(resp) => ctx.log(
                            "warn",
                            &format!(
                                "heartbeat_scan: TimeoutFail failed for {} (HTTP {})",
                                agent_id, resp.status
                            ),
                        ),
                        Err(error) => ctx.log(
                            "warn",
                            &format!(
                                "heartbeat_scan: TimeoutFail failed for {}: {}",
                                agent_id, error
                            ),
                        ),
                    }
                } else {
                    ctx.log(
                        "info",
                        &format!(
                            "heartbeat_scan: agent {} heartbeat marker='{}' timeout={}s — alive",
                            agent_id, last_heartbeat, timeout_secs
                        ),
                    );
                }
            }
        }

        // Return scan complete
        set_success_result("ScanComplete", &json!({
            "last_scan_at": "scan-complete",
            "stale_agents_found": stale_count,
        }));

        Ok(())
    })();

    if let Err(e) = result {
        set_error_result(&e);
    }
    0
}
