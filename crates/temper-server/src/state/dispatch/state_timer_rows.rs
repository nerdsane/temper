//! Durable rows for armed state timers, so a restart arms them again (ADR-0056).
//!
//! A timer writes its row when it is armed and removes it when it ends, so the
//! rows are the timers still waiting. At boot [`ServerState::rearm_state_timers`]
//! reads each tenant's rows, retrying failed listings, without loading any
//! entity, and arms every timer again from when its state's clock started:
//! for the rest of its budget,
//! or at once if it fell due while the server was down. It fires once, as the
//! identity that armed it; ticks missed while the server was down are not
//! replayed. A row armed for no one (the state-timer service) is left to the
//! walk that follows, which arms it as the principal the record's journal
//! names, when it names one (`state_timer_setter`).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use temper_runtime::persistence::{PersistenceError, StateTimerRecord};
use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;
use tokio::sync::Semaphore;
use tracing::instrument;

use super::state_timeouts::{ArmedTimer, remaining_budget};
use super::state_timer_follow::{BootCallers, BootTimers};
use super::state_timer_setter::is_state_timer_service;
use super::state_timer_settled::BootRows;
use crate::entity_actor::EntityState;
use crate::request_context::AgentContext;
use crate::request_context::armed_by::{decode_armed_by, encode_armed_by};
use crate::state::ServerState;
use crate::storage::{MetadataTimerRows, StateTimerStore};

/// How many timers armed at boot fire at once. Every timer that fell due while
/// the server was down is due at boot; firing them all together is a burst of
/// concurrent writes, and on 2026-10-16 1,610 at once on a loaded machine left
/// 861 events unwritten (`failed to persist event`) and 1,013 projections
/// failed, so those timers did not re-arm. The rest wait their turn.
const BOOT_FIRES_AT_ONCE: usize = 8;

/// A tenant's declared timeouts: `(entity type, state)` to
/// `(after_seconds, on_timeout)`.
type DeclaredTimeouts = BTreeMap<(String, String), (u64, String)>;

impl ServerState {
    /// Where `tenant`'s timer rows are kept: [`Self::state_timer_rows`] when
    /// set, else the tenant's metadata store. None: rows are not kept.
    async fn timer_rows_for_tenant(&self, tenant: &str) -> Option<Arc<dyn StateTimerStore>> {
        if let Some(rows) = self.state_timer_rows.clone() {
            return Some(rows);
        }
        let store = self.metadata_store_for_tenant(tenant).await?;
        Some(Arc::new(MetadataTimerRows(store)))
    }

    /// The record's timer row, which the event store commits with the events
    /// that change it. None: no row. An error is a row that could not be read
    /// (logged): it may hold the committed arm, so it is not "no row". An
    /// ended, withheld task leaves this row as the record of its setter.
    pub(super) async fn stored_state_timer_row(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<Option<StateTimerRecord>, PersistenceError> {
        let Some(store) = self.timer_rows_for_tenant(tenant.as_str()).await else {
            return Ok(None);
        };
        store
            .state_timer_row(tenant.as_str(), entity_type, entity_id)
            .await
            .inspect_err(|error| {
                tracing::warn!(tenant = %tenant, entity_type, entity_id, %error,
                    "state timer row not readable");
            })
    }

    /// Write a newly armed timer's row. A failed write leaves the timer armed
    /// in this process; only a restart before it fires would lose it. The
    /// event that armed the timer usually wrote its row, and this writes only
    /// a row that is missing.
    pub(super) async fn persist_state_timer(&self, timer: &ArmedTimer) {
        let Some(store) = self.timer_rows_for_tenant(timer.tenant.as_str()).await else {
            return;
        };
        let committed = self
            .stored_state_timer_row(&timer.tenant, &timer.entity_type, &timer.entity_id)
            .await;
        if committed.is_ok_and(|row| row.is_some_and(|row| row.arm_id == timer.arm_id)) {
            return;
        }
        let record = match timer_record(timer) {
            Ok(record) => record,
            Err(error) => {
                tracing::error!(tenant = %timer.tenant, entity_type = %timer.entity_type,
                    entity_id = %timer.entity_id, %error, "state timer row not encodable");
                return;
            }
        };
        if let Err(error) = store.upsert_state_timer(&record).await {
            tracing::error!(tenant = %timer.tenant, entity_type = %timer.entity_type,
                entity_id = %timer.entity_id, %error,
                "state timer row not written; a restart before it fires loses it");
        }
    }

    /// Remove an ended timer's row, unless a newer arm has replaced it. The
    /// event that ended the timer usually removed or replaced its row, and
    /// this removes only a row the timer still holds.
    pub(super) async fn forget_state_timer(&self, timer: &ArmedTimer) {
        let Some(store) = self.timer_rows_for_tenant(timer.tenant.as_str()).await else {
            return;
        };
        let held = self
            .stored_state_timer_row(&timer.tenant, &timer.entity_type, &timer.entity_id)
            .await;
        if !held.is_ok_and(|row| row.is_some_and(|row| row.arm_id == timer.arm_id)) {
            return;
        }
        if let Err(error) = store
            .delete_state_timer(
                timer.tenant.as_str(),
                &timer.entity_type,
                &timer.entity_id,
                &timer.arm_id,
            )
            .await
        {
            tracing::warn!(tenant = %timer.tenant, entity_type = %timer.entity_type,
                entity_id = %timer.entity_id, %error,
                "ended state timer row not removed; the next boot finds it stale");
        }
    }

    /// Arm again every state timer that a previous process left waiting
    /// (`ServerState::start_runtime` calls it once at boot).
    ///
    /// Reads one row per waiting timer and loads no entity until its timer
    /// fires, at most [`BOOT_FIRES_AT_ONCE`] firing at once; a row whose state
    /// declares no timeout now is removed. Then, in the background, each
    /// record that had a row follows the state it is in now (as whoever armed
    /// its row), through the same gate. A record in a timed state with no row
    /// is armed from its recorded state clock (unless it needs a caller). The scheduled actions left
    /// waiting are armed again from their rows through the same gate
    /// (`rearm_scheduled_actions`). Returns that background walk, which ends
    /// with the boot's counts per tenant and type, and logs them as
    /// `state timers after boot`.
    pub(crate) async fn rearm_state_timers(&self) -> tokio::task::JoinHandle<BootTimers> {
        let repaired = self.repair_query_projections_at_boot().await;
        let started = Instant::now();
        let gate = Arc::new(Semaphore::new(BOOT_FIRES_AT_ONCE));
        let mut callers = BootCallers::new();
        let mut counts = BootTimers::new();
        let mut rows = BootRows::new();
        let (armed, overdue) = self
            .rearm_state_timer_rows(&gate, &mut callers, &mut counts, &mut rows)
            .await;
        self.follow_repaired_records(repaired, &callers, &gate)
            .await;
        tracing::info!(
            armed,
            overdue,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "re-armed state timers from the store"
        );
        self.rearm_scheduled_actions(&gate, &mut counts).await;
        {
            // Serialize with install walks so none can consume startup changes
            // before saved identities and gates have been restored. The boot
            // walk covers all timed types, including missing rows at startup.
            let mut registry = self.registry.write().expect("registry lock poisoned");
            registry.take_changed_types();
            self.timer_recovery_complete
                .store(true, std::sync::atomic::Ordering::Release);
        }
        let server = self.clone();
        // determinism-ok: boot follow-up runs in the background, as boot timers do.
        tokio::spawn(async move { server.follow_timed_types(callers, gate, counts, rows).await })
    }

    /// Arm again each state timer row the last process left, as
    /// [`Self::rearm_state_timers`] describes, keeping the callers of rows it
    /// drops in `callers`, the state each row it arms was armed for in
    /// `rows`, and counting into `counts`. Returns how many it armed and how
    /// many of those were overdue.
    #[instrument(skip_all, fields(otel.name = "boot.state_timer_rows"))]
    async fn rearm_state_timer_rows(
        &self,
        gate: &Arc<Semaphore>,
        callers: &mut BootCallers,
        counts: &mut BootTimers,
        rows_armed: &mut BootRows,
    ) -> (usize, usize) {
        let mut armed = 0usize;
        let mut overdue = 0usize;
        for (tenant, timeouts) in self.declared_state_timeouts() {
            let Some(store) = self.timer_rows_for_tenant(tenant.as_str()).await else {
                continue;
            };
            // A failed listing does not mean there are no saved rows. Restore
            // their callers before either repaired records or the rowless walk
            // can reconcile timers and overwrite a saved identity.
            let mut wait = Duration::from_secs(1);
            let rows = loop {
                match store.list_state_timers(tenant.as_str()).await {
                    Ok(rows) => break rows,
                    Err(error) => {
                        tracing::error!(tenant = %tenant, %error,
                            retry_in_secs = wait.as_secs(),
                            "state timer rows not readable; boot recovery waits for saved callers");
                        tokio::time::sleep(wait).await; // determinism-ok: boot retry on runtime clock
                        wait = (wait * 2).min(Duration::from_secs(60));
                    }
                }
            };
            let now = sim_now();
            for row in rows {
                let declared = timeouts.get(&(row.entity_type.clone(), row.state.clone()));
                let timer = match declared {
                    Some((after_seconds, action)) => {
                        armed_timer(&tenant, &row, action, *after_seconds)
                    }
                    None => {
                        if let Some(armed_by) = row_armed_by(&row) {
                            let key = (
                                tenant.clone(),
                                row.entity_type.clone(),
                                row.entity_id.clone(),
                            );
                            callers.insert(key, armed_by);
                        }
                        Err("its state no longer declares a timeout".to_string())
                    }
                };
                let counted = counts
                    .entry((tenant.clone(), row.entity_type.clone()))
                    .or_default();
                let mut timer = match timer {
                    Ok(timer) => timer,
                    Err(reason) => {
                        counted.dropped += 1;
                        tracing::warn!(tenant = %tenant, entity_type = %row.entity_type,
                            entity_id = %row.entity_id, state = %row.state, %reason,
                            "state timer row dropped at boot");
                        let _ = store
                            .delete_state_timer(
                                tenant.as_str(),
                                &row.entity_type,
                                &row.entity_id,
                                &row.arm_id,
                            )
                            .await;
                        continue;
                    }
                };
                if is_state_timer_service(&timer.agent_ctx) {
                    // Armed for no one. The walk after this arms it, as the
                    // principal the record's journal names when it names one
                    // (`state_timer_setter`): reading a journal, page by page,
                    // stays off the path the server serves after.
                    continue;
                }
                timer.boot_gate = Some(gate.clone());
                let delay = remaining_budget(timer.started_at, timer.after_seconds, now);
                let label = if delay.is_zero() {
                    "overdue"
                } else {
                    "budgeted"
                };
                overdue += usize::from(delay.is_zero());
                crate::runtime_metrics::record_state_timeout_armed_on_hydration(
                    tenant.as_str(),
                    &timer.entity_type,
                    &timer.state,
                    label,
                );
                let Some(armed_seq) = self.reserve_state_timer(&timer) else {
                    continue;
                };
                counted.rearmed += 1;
                rows_armed.insert(
                    (
                        tenant.clone(),
                        row.entity_type.clone(),
                        row.entity_id.clone(),
                    ),
                    row.state.clone(),
                );
                self.spawn_state_timer(timer, Some(gate.clone()), armed_seq);
                armed += 1;
            }
        }
        (armed, overdue)
    }

    /// Make each record the boot repair rewrote follow its replayed state,
    /// as the caller its row was armed by, through the boot gate.
    #[instrument(skip_all, fields(otel.name = "boot.repaired_follow", records = repaired.len()))]
    async fn follow_repaired_records(
        &self,
        repaired: Vec<(TenantId, EntityState)>,
        callers: &BootCallers,
        gate: &Arc<Semaphore>,
    ) {
        // Restore saved identities first, then reconcile repaired records with
        // their replayed state through the same eight-fire boot gate. A restart
        // during repair is safe: the rowless boot walk also recovers any timer
        // whose projection landed before its timer row.
        for (tenant, current) in repaired {
            let key = (
                tenant.clone(),
                current.entity_type.clone(),
                current.entity_id.clone(),
            );
            self.follow_state_timer(
                &tenant,
                &current.entity_type,
                &current.entity_id,
                &current,
                super::state_timer_reconcile::Follow {
                    armed_by: callers.get(&key),
                    gate: Some(gate.clone()),
                    ..Default::default()
                },
            )
            .await;
        }
    }

    /// Every tenant's declared timeouts. Tenants without any are left out.
    fn declared_state_timeouts(&self) -> Vec<(TenantId, DeclaredTimeouts)> {
        let registry = self.registry.read().expect("registry lock poisoned");
        let mut declared = Vec::new();
        for tenant in registry.tenant_ids() {
            let mut timeouts = BTreeMap::new();
            for entity_type in registry.entity_types(tenant) {
                let Some(spec) = registry.get_spec(tenant, entity_type) else {
                    continue;
                };
                for st in &spec.automaton.state_timeouts {
                    timeouts.insert(
                        (entity_type.to_string(), st.state.clone()),
                        (st.after_seconds, st.on_timeout.clone()),
                    );
                }
            }
            if !timeouts.is_empty() {
                declared.push((tenant.clone(), timeouts));
            }
        }
        declared
    }
}

/// The identity a row's timer fires as, if the row keeps a readable one.
pub(super) fn row_armed_by(row: &StateTimerRecord) -> Option<AgentContext> {
    decode_armed_by(&row.armed_by).ok()
}

fn timer_record(timer: &ArmedTimer) -> Result<StateTimerRecord, serde_json::Error> {
    Ok(StateTimerRecord {
        tenant: timer.tenant.to_string(),
        entity_type: timer.entity_type.clone(),
        entity_id: timer.entity_id.clone(),
        state: timer.state.clone(),
        arm_id: timer.arm_id.clone(),
        sequence_nr: timer.sequence_nr,
        started_at: timer.started_at.to_rfc3339(),
        params: serde_json::to_string(&timer.params)?,
        armed_by: encode_armed_by(&timer.agent_ctx)?,
    })
}

/// Decode a durable arm using its state's current action and budget.
pub(super) fn armed_timer(
    tenant: &TenantId,
    row: &StateTimerRecord,
    action: &str,
    after_seconds: u64,
) -> Result<ArmedTimer, String> {
    let started_at = DateTime::parse_from_rfc3339(&row.started_at)
        .map_err(|error| format!("started_at: {error}"))?
        .with_timezone(&Utc);
    let params = serde_json::from_str(&row.params).map_err(|error| format!("params: {error}"))?;
    let armed_by = decode_armed_by(&row.armed_by).map_err(|error| format!("armed_by: {error}"))?;
    Ok(ArmedTimer {
        boot_gate: None,
        tenant: tenant.clone(),
        entity_type: row.entity_type.clone(),
        entity_id: row.entity_id.clone(),
        state: row.state.clone(),
        action: action.to_string(),
        params,
        after_seconds,
        agent_ctx: armed_by,
        arm_id: row.arm_id.clone(),
        sequence_nr: row.sequence_nr,
        started_at,
        // The row does not keep it: whoever follows the record reads it there.
        brought_forward_at: None,
    })
}
