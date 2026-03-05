//! Schedule ticker: evaluates cron expressions for Active schedules.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use temper_sdk::TemperClient;
use tracing::{info, warn};

use crate::agent_type::resolve_agent_type_model;

/// Default model used when no AgentType is configured.
const DEFAULT_MODEL: &str = "claude-sonnet-4-6";

/// Periodically evaluates Active schedules and fires due ones.
///
/// Runs on a 60-second interval. For each Active Schedule entity, parses the
/// cron expression and fires `Schedule.Fire` if the cron is due. The Fire action's
/// spawn effect creates an Agent entity, which the SSE event loop picks up.
pub async fn run_schedule_ticker(
    temper_url: &str,
    tenant: &str,
    shutting_down: &Arc<AtomicBool>,
) {
    use chrono::Utc; // determinism-ok: executor process, not simulation-visible
    use cron::Schedule;
    use std::str::FromStr;

    let client = TemperClient::new(temper_url, tenant);
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));

    loop {
        interval.tick().await;
        if shutting_down.load(Ordering::Relaxed) {
            break;
        }

        // Query Active schedules.
        let schedules = match client
            .list_filtered("Schedules", "status eq 'Active'")
            .await
        {
            Ok(s) => s,
            Err(e) => {
                warn!("Failed to query schedules: {e}");
                continue;
            }
        };

        let now = Utc::now(); // determinism-ok: executor process

        for sched in schedules {
            let field = |key: &str| -> &str {
                sched
                    .get(key)
                    .or_else(|| {
                        sched
                            .get("fields")
                            .and_then(|f: &serde_json::Value| f.get(key))
                    })
                    .and_then(|v: &serde_json::Value| v.as_str())
                    .unwrap_or_default()
            };
            let sched_id = field("id");
            let cron_expr = field("cron_expr");
            let last_run = field("last_run");
            let run_count: u64 = field("run_count").parse().unwrap_or(0);
            let max_runs: u64 = field("max_runs").parse().unwrap_or(0);

            // Check max_runs (0 = unlimited).
            if max_runs > 0 && run_count >= max_runs {
                // Auto-complete the schedule.
                if let Err(e) = client
                    .action("Schedules", sched_id, "Complete", serde_json::json!({}))
                    .await
                {
                    warn!(schedule_id = %sched_id, "Failed to complete schedule: {e}");
                }
                continue;
            }

            // Parse cron expression.
            let schedule = match Schedule::from_str(cron_expr) {
                Ok(s) => s,
                Err(e) => {
                    warn!(schedule_id = %sched_id, cron = %cron_expr, "Invalid cron expression: {e}");
                    continue;
                }
            };

            // Check if due: find next occurrence after last_run (or epoch if never run).
            let last = if last_run.is_empty() {
                chrono::DateTime::<Utc>::MIN_UTC
            } else {
                last_run
                    .parse::<chrono::DateTime<Utc>>()
                    .unwrap_or(chrono::DateTime::<Utc>::MIN_UTC)
            };

            let next = schedule.after(&last).next();
            let is_due = next.is_some_and(|n| n <= now);

            if !is_due {
                continue;
            }

            // Resolve agent params from the schedule entity.
            let resolve = |key: &str| -> String {
                sched
                    .get(key)
                    .or_else(|| sched.get("fields").and_then(|f| f.get(key)))
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string()
            };

            let agent_role = resolve("agent_role");
            let goal_template = resolve("goal_template");
            let agent_type_id = resolve("agent_type_id");
            let now_str = now.to_rfc3339();

            // Resolve model from AgentType instead of hardcoding.
            let model =
                resolve_agent_type_model(&client, &agent_type_id, DEFAULT_MODEL).await;

            info!(
                schedule_id = %sched_id,
                role = %agent_role,
                model = %model,
                "Firing schedule"
            );

            if let Err(e) = client
                .action(
                    "Schedules",
                    sched_id,
                    "Fire",
                    serde_json::json!({
                        "last_run": now_str,
                        "role": agent_role,
                        "goal": goal_template,
                        "model": model,
                        "agent_type_id": agent_type_id,
                    }),
                )
                .await
            {
                warn!(schedule_id = %sched_id, "Failed to fire schedule: {e}");
            }
        }
    }
}
