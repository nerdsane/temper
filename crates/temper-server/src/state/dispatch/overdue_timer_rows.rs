//! A gauge of timer rows past their due time, per tenant: state timers and
//! scheduled actions. It reads the rows, not the timers armed in this process,
//! so a row that no process armed, or whose fire never landed, still counts.
//! Without it the kernel saw an overdue timer only when something touched its
//! record: 20 release records sat in Queued for two days unseen.
//!
//! A repeating timer whose fire changes nothing writes nothing, so its row stays
//! at the time of its first look and this process holds when it looks next. Such
//! a row is due when that timer was armed for, the later of the two: a poll
//! waiting for its next look is not late, and one whose look does not land is,
//! from when that look was due. A fire stuck retrying keeps the time it was
//! armed for (`DueTimes::armed_for`), so it counts too.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use temper_runtime::persistence::{OverdueTimerRow, StateTimerRecord};
use temper_runtime::scheduler::sim_now;

use crate::request_context::ScheduledFire;
use crate::state::ServerState;
use crate::storage::StateTimerStore;
use crate::wake::TimerRef;

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
                    let state_timer = |row: &OverdueTimerRow| TimerRef::State {
                        tenant: tenant.clone(),
                        entity_type: row.entity_type.clone(),
                        entity_id: row.entity_id.clone(),
                    };
                    let scheduled = |row: &OverdueTimerRow| {
                        TimerRef::Scheduled(ScheduledFire {
                            tenant: tenant.clone(),
                            entity_type: row.entity_type.clone(),
                            entity_id: row.entity_id.clone(),
                            arm_id: row.arm_id.clone(),
                        })
                    };
                    let (state_rows, state_late) =
                        self.past_due(&rows.state_timers, state_timer, now);
                    crate::runtime_metrics::record_overdue_timer_rows(
                        &tenant,
                        "state_timeout",
                        state_rows,
                        state_late,
                    );
                    let (scheduled_rows, scheduled_late) =
                        self.past_due(&rows.scheduled_actions, scheduled, now);
                    crate::runtime_metrics::record_overdue_timer_rows(
                        &tenant,
                        "scheduled_action",
                        scheduled_rows,
                        scheduled_late,
                    );
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

    /// How many of `rows` are late at `now`, and how many seconds late the
    /// oldest is. A row whose due time cannot be read counts, with no lateness.
    fn past_due(
        &self,
        rows: &[OverdueTimerRow],
        timer: impl Fn(&OverdueTimerRow) -> TimerRef,
        now: DateTime<Utc>,
    ) -> (u64, u64) {
        let mut count = 0;
        let mut oldest_seconds = 0;
        for row in rows {
            let Ok(row_due) = DateTime::parse_from_rfc3339(&row.due_at) else {
                count += 1;
                continue;
            };
            let row_due = row_due.with_timezone(&Utc);
            let due = self
                .due_times
                .armed_for(&timer(row))
                .map_or(row_due, |armed_for| armed_for.max(row_due));
            let Ok(late) = (now - due).to_std() else {
                continue;
            };
            count += 1;
            oldest_seconds = oldest_seconds.max(late.as_secs());
        }
        (count, oldest_seconds)
    }
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
