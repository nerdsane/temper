//! Recover the successor rows of a scheduled fire whose answer was lost.

use chrono::{DateTime, Utc};
use temper_runtime::persistence::ScheduledActionRecord;
use temper_runtime::tenant::TenantId;

use super::super::owed_writes::{FIRST_RETRY_WAIT, LAST_RETRY_WAIT};
use super::ScheduledTimer;
use crate::request_context::ScheduledFire;
use crate::request_context::armed_by::{call_started_in, decode_armed_by};
use crate::request_context::calls::called_integration;
use crate::state::ServerState;

impl ServerState {
    /// Cancelled rows only need garbage collection. Rows owned by this
    /// process still have a live call (or an owed settlement), even when a
    /// boot listing is retried after new requests have started.
    pub(super) fn skip_call_recovery(
        &self,
        tenant: &TenantId,
        row: &ScheduledActionRecord,
    ) -> bool {
        if row.action.is_empty() {
            let server = self.clone();
            let call = ScheduledFire {
                tenant: tenant.as_str().to_string(),
                entity_type: row.entity_type.clone(),
                entity_id: row.entity_id.clone(),
                arm_id: row.arm_id.clone(),
            };
            tokio::spawn(async move { server.end_call_row(&call).await }); // determinism-ok: durable cancellation cleanup
            return true;
        }
        called_integration(&row.arm_id).is_some()
            && call_started_in(&row.armed_by, self.call_process_id)
    }

    /// Arm the rows `timer`'s fire committed, after a retried fire. A fire
    /// whose answer was lost commits its successors' rows, but the duplicate
    /// that answers its retry hands back no schedules. Only the fire's own
    /// rows are armed: any other row of the record belongs to a dispatch that
    /// arms it itself, after its integrations. Err: the journal or the rows
    /// could not be read, and nothing was armed. Ok: whether the fire already
    /// committed, including when it scheduled no successors.
    async fn arm_fired_rows(&self, timer: &ScheduledTimer, key: &str) -> Result<bool, String> {
        let Some((store, _)) = self.event_journal() else {
            return Ok(false);
        };
        // The fire's event comes after the event that armed it.
        let armed_seq = timer
            .arm_id
            .split_once('.')
            .and_then(|(seq, _)| seq.parse::<u64>().ok())
            .unwrap_or(0);
        let persistence_id = format!("{}:{}:{}", timer.tenant, timer.entity_type, timer.entity_id);
        let events = store
            .read_events(&persistence_id, armed_seq.saturating_sub(1))
            .await
            .map_err(|error| error.to_string())?;
        let Some(fire_seq) = events
            .iter()
            .find(|event| event.payload["idempotency_key"].as_str() == Some(key))
            .map(|event| event.sequence_nr)
        else {
            return Ok(false);
        };
        let rows = store
            .list_scheduled_actions(timer.tenant.as_str())
            .await
            .map_err(|error| error.to_string())?;
        let prefix = format!("{fire_seq}.");
        // A call deadline the fire's move wrote waits for the call, which
        // answers in this process; only a boot arms it.
        let fired = rows.iter().filter(|row| {
            row.entity_type == timer.entity_type
                && row.entity_id == timer.entity_id
                && row.arm_id.starts_with(&prefix)
                && called_integration(&row.arm_id).is_none()
        });
        for row in fired {
            if let Ok(waiting) = scheduled_timer(&timer.tenant, row) {
                self.spawn_scheduled_action(waiting, None);
            }
        }
        Ok(true)
    }

    /// [`Self::arm_fired_rows`] until the rows can be read, waiting longer
    /// after each failure: until then the fire's successors have no timer.
    pub(super) async fn arm_fired_rows_retrying(&self, timer: &ScheduledTimer, key: &str) -> bool {
        let mut wait = FIRST_RETRY_WAIT;
        loop {
            match self.arm_fired_rows(timer, key).await {
                Ok(committed) => return committed,
                Err(error) => {
                    tracing::error!(tenant = %timer.tenant, entity_type = %timer.entity_type,
                        entity_id = %timer.entity_id, arm_id = %timer.arm_id, %error,
                        retry_in_ms = wait.as_millis() as u64,
                        "rows of a retried scheduled fire not readable; retrying");
                    tokio::time::sleep(wait).await; // determinism-ok: retry backoff
                    wait = (wait * 2).min(LAST_RETRY_WAIT);
                }
            }
        }
    }
}

/// The timer a row arms.
pub(super) fn scheduled_timer(
    tenant: &TenantId,
    row: &ScheduledActionRecord,
) -> Result<ScheduledTimer, String> {
    let due_at = DateTime::parse_from_rfc3339(&row.due_at)
        .map_err(|error| format!("due_at: {error}"))?
        .with_timezone(&Utc);
    let agent_ctx = decode_armed_by(&row.armed_by).map_err(|error| format!("armed_by: {error}"))?;
    let timer = ScheduledTimer {
        tenant: tenant.clone(),
        entity_type: row.entity_type.clone(),
        entity_id: row.entity_id.clone(),
        arm_id: row.arm_id.clone(),
        action: row.action.clone(),
        due_at,
        agent_ctx,
    };
    Ok(timer)
}

#[cfg(test)]
#[path = "call_recovery_test.rs"]
mod tests;
