//! A gauge of timer rows past their due time, per tenant: state timers and
//! scheduled actions. It reads the rows, not the timers armed in this process,
//! so a row that no process armed, or whose fire never landed, still counts.
//! Without it the kernel saw an overdue timer only when something touched its
//! record: 20 Factory Releases sat in Queued for two days unseen.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use temper_runtime::persistence::{OverdueTimers, StateTimerRecord};
use temper_runtime::scheduler::sim_now;

use crate::state::ServerState;
use crate::storage::StateTimerStore;

impl ServerState {
    /// Sample each tenant's timer rows past due into
    /// `temper_timer_rows_overdue` and
    /// `temper_timer_rows_overdue_oldest_seconds`, by `kind`. One read of the
    /// rows' due-time index per tenant, each given `limit` to answer: a slow
    /// store skips that tenant's sample, logged, rather than holding the
    /// loop. The runtime metrics loop calls it every interval.
    pub async fn record_overdue_timer_rows(&self, limit: Duration) {
        let tenants: Vec<String> = {
            let Ok(registry) = self.registry.read() else {
                return;
            };
            registry
                .tenant_ids()
                .into_iter()
                .map(|tenant| tenant.to_string())
                .collect()
        };
        let now = sim_now();
        let at = now.to_rfc3339();
        for tenant in tenants {
            let Some(store) = self.timer_rows_for_tenant(&tenant).await else {
                continue;
            };
            match tokio::time::timeout(limit, store.overdue_timer_rows(&tenant, &at)).await {
                Ok(Ok(rows)) => {
                    record(&tenant, "state_timeout", &rows.state_timers, now);
                    record(&tenant, "scheduled_action", &rows.scheduled_actions, now);
                }
                Ok(Err(error)) => {
                    tracing::warn!(tenant, %error, "timer rows past due not readable");
                }
                Err(_) => {
                    tracing::warn!(
                        tenant,
                        limit_ms = limit.as_millis() as u64,
                        "timer rows past due not read in time"
                    );
                }
            }
        }
    }
}

fn record(tenant: &str, kind: &str, rows: &OverdueTimers, now: DateTime<Utc>) {
    let oldest_seconds = rows
        .oldest_due_at
        .as_deref()
        .and_then(|due_at| DateTime::parse_from_rfc3339(due_at).ok())
        .and_then(|due_at| (now - due_at.with_timezone(&Utc)).to_std().ok())
        .map_or(0, |late| late.as_secs());
    crate::runtime_metrics::record_overdue_timer_rows(tenant, kind, rows.count, oldest_seconds);
}

/// Write the due time on each row an older release wrote without one (or
/// under another budget), so the gauge of timers past due sees it. An update
/// only: a row whose timer fired and removed it meanwhile stays removed.
pub(super) async fn date_timer_rows(
    rows: Vec<(Arc<dyn StateTimerStore>, StateTimerRecord, String)>,
) {
    for (store, row, due_at) in rows {
        if let Err(error) = store
            .date_state_timer(
                &row.tenant,
                &row.entity_type,
                &row.entity_id,
                &row.arm_id,
                &due_at,
            )
            .await
        {
            tracing::warn!(tenant = %row.tenant, entity_type = %row.entity_type,
                entity_id = %row.entity_id, %error,
                "state timer row's due time not written; the next boot writes it");
        }
    }
}
