//! Crucible Cron Trigger — posts a user.message to a Session.
//!
//! Fired by the `SessionSchedule.Trigger` action. Reads the schedule's
//! `SessionId` and `MessageTemplate`, performs template substitution,
//! checks the session status, enforces a minimum interval between
//! triggers, and POSTs a `user.message` event.

use temper_wasm_sdk::prelude::*;

/// Minimum seconds between triggers. Prevents rapid-fire when the
/// heartbeat loop runs faster than intended. Set to match your
/// desired trigger interval (default 4s for sub-minute scheduling,
/// or 55s for standard cron-minute granularity).
const MIN_INTERVAL_SECS: i64 = 4;

temper_module! {
    fn run(ctx: Context) -> Result<Value> {
        let fields = ctx.entity_state.get("fields").cloned().unwrap_or(json!({}));
        let counters = ctx.entity_state.get("counters").cloned().unwrap_or(json!({}));

        let session_id = fields.get("SessionId")
            .and_then(|v| v.as_str())
            .ok_or("SessionSchedule.SessionId is not set")?;
        let template = fields.get("MessageTemplate")
            .and_then(|v| v.as_str())
            .unwrap_or("Scheduled trigger");
        let run_count = counters.get("run_count")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let last_result = fields.get("LastResult")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let last_triggered = fields.get("LastTriggeredAt")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let temper_url = ctx.config.get("temper_api_url")
            .cloned().unwrap_or_else(|| "http://127.0.0.1:3000".into());
        let tenant = &ctx.tenant;
        let headers = vec![("X-Tenant-Id".to_string(), tenant.to_string())];

        let now_ms = Context::get_time_millis();
        let now_secs = now_ms / 1000;
        let now = format_iso(now_ms);

        // ── Rate limit: skip if fired too recently ──────────────
        if !last_triggered.is_empty() {
            if let Some(last_secs) = parse_iso_to_epoch(last_triggered) {
                let elapsed = now_secs - last_secs;
                if elapsed < MIN_INTERVAL_SECS {
                    return Ok(json!({
                        "last_result": format!("skipped: {}s since last trigger (min {}s)", elapsed, MIN_INTERVAL_SECS)
                    }));
                }
            }
        }

        // ── Template substitution ───────────────────────────────
        let message = template
            .replace("{{now}}", &now)
            .replace("{{run_count}}", &run_count.to_string())
            .replace("{{last_result}}", last_result);

        // ── Check session status ────────────────────────────────
        let resp = ctx.http_call("GET",
            &format!("{temper_url}/tdata/Sessions('{session_id}')"),
            &headers, "")?;
        if resp.status != 200 {
            return Err(format!("Session {session_id} not found: HTTP {}", resp.status));
        }
        let session: Value = serde_json::from_str(&resp.body)
            .map_err(|e| format!("parse session: {e}"))?;
        let status = session.get("status").and_then(|v| v.as_str()).unwrap_or("");
        if status == "Running" || status == "Terminated" || status == "Archived" {
            ctx.log("info", &format!("Skipping: session {session_id} is {status}"));
            return Ok(json!({"last_result": format!("skipped: session {status}")}));
        }

        // ── Get max sequence ────────────────────────────────────
        let resp = ctx.http_call("GET",
            &format!("{temper_url}/tdata/SessionEvents?$filter=SessionId%20eq%20'{session_id}'&$orderby=Sequence%20desc&$top=1"),
            &headers, "")?;
        let events: Value = serde_json::from_str(&resp.body).unwrap_or(json!({}));
        let max_seq = events.get("value")
            .and_then(|v| v.get(0))
            .and_then(|e| e.get("fields"))
            .and_then(|f| f.get("Sequence"))
            .and_then(|v| v.as_i64())
            .unwrap_or(-1);
        let next_seq = max_seq + 1;

        // ── POST user.message ───────────────────────────────────
        let event_body = json!({
            "id": format!("ev-cron-{session_id}-{next_seq}"),
            "SessionId": session_id,
            "Sequence": next_seq,
            "Kind": "user.message",
            "Content": json!({"blocks": [{"type": "text", "text": message}]}).to_string(),
            "CreatedAt": &now,
            "ProcessedAt": &now,
        });
        let post_headers = vec![
            ("X-Tenant-Id".to_string(), tenant.to_string()),
            ("Content-Type".to_string(), "application/json".to_string()),
        ];
        let resp = ctx.http_call("POST",
            &format!("{temper_url}/tdata/SessionEvents"),
            &post_headers, &event_body.to_string())?;
        if resp.status >= 400 {
            return Err(format!("POST user.message failed: HTTP {}", resp.status));
        }

        // ── PATCH schedule: update LastTriggeredAt ──────────────
        let _ = ctx.http_call("PATCH",
            &format!("{temper_url}/tdata/SessionSchedules('{}')", ctx.entity_id),
            &post_headers,
            &json!({"LastTriggeredAt": &now, "UpdatedAt": &now}).to_string());

        ctx.log("info", &format!("Cron: posted user.message to {session_id} (seq={next_seq})"));
        Ok(json!({"last_result": format!("triggered at {now}")}))
    }
}

/// Parse a subset of ISO-8601 timestamps to epoch seconds.
/// Handles `2026-04-12T17:36:37.188Z` and `2026-04-12T17:36:37Z`.
fn parse_iso_to_epoch(s: &str) -> Option<i64> {
    // Minimal parser: YYYY-MM-DDThh:mm:ss
    if s.len() < 19 { return None; }
    let y: i64 = s[0..4].parse().ok()?;
    let mo: u32 = s[5..7].parse().ok()?;
    let d: u32 = s[8..10].parse().ok()?;
    let h: u32 = s[11..13].parse().ok()?;
    let mi: u32 = s[14..16].parse().ok()?;
    let sec: u32 = s[17..19].parse().ok()?;

    // Days from civil date (inverse of epoch_to_civil).
    let y_adj = if mo <= 2 { y - 1 } else { y };
    let era = if y_adj >= 0 { y_adj } else { y_adj - 399 } / 400;
    let yoe = (y_adj - era * 400) as u32;
    let mo_adj = if mo > 2 { mo - 3 } else { mo + 9 };
    let doy = (153 * mo_adj + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe as i64 - 719_468;

    Some(days * 86_400 + h as i64 * 3600 + mi as i64 * 60 + sec as i64)
}

fn format_iso(millis: i64) -> String {
    let secs = millis / 1000;
    let ms = millis % 1000;
    let (y, mo, d, h, mi, s) = epoch_to_civil(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{ms:03}Z")
}

fn epoch_to_civil(epoch_secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let s = epoch_secs.rem_euclid(86_400) as u32;
    let h = s / 3600;
    let m = (s % 3600) / 60;
    let sec = s % 60;
    let z = epoch_secs.div_euclid(86_400) + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    (y, mo, d, h, m, sec)
}
