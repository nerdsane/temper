//! Walks that make every record of a timed type follow its state's timer.
//!
//! An install that changes a type's spec, or gives a type its first timeout,
//! can leave records in a timed state with the timer of another state or
//! none. Its walk reconciles every record of the type
//! ([`ServerState::reconcile_state_timers`]), so every record in a timed state
//! ends with its timer armed, or waits for a caller (see
//! [`TimerFollow::WaitsForCaller`]).
//!
//! At boot, after the rows are re-armed, a walk makes each record that had a
//! row follow the state it replays into: a row armed under an older spec, on
//! a record whose state now declares another timer, gets the new timer at
//! once, not when the stale one fires. A record in a timed state with no row
//! (its process stopped between the event and the row) is armed too, from
//! when it entered its state, through the boot's fire gate; only one whose
//! timeout binds a caller nobody saved waits for its next action. The walk
//! counts those, and ends with one log line of counts per tenant and type.
//!
//! A record in memory is reconciled with its actor's state. One that is not is
//! replayed from the store without starting an actor, so a walk over many
//! records does not leave them all resident. A record that cannot be read
//! does not hold its type back: the walk finishes the type, reports the
//! record, and tries it again without its type until it reads.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;

use temper_runtime::tenant::TenantId;

use super::state_timer_reconcile::{Arming, Follow, TimerFollow};
use super::state_timer_settled::BootRows;
use crate::entity_actor::{EntityState, recover_entity_state_for_walk};
use crate::request_context::AgentContext;
use crate::state::ServerState;
use crate::state::projection_backfill::transition_table_for;

/// The wait before retrying a type whose records the store failed to list,
/// or one record or timer row it failed to read. It doubles up to
/// [`FOLLOW_RETRY_MAX`].
pub(super) const FOLLOW_RETRY_FIRST: Duration = Duration::from_secs(1);
/// The longest wait between two tries. The reason for the ceiling is the
/// store: a store that stays down, or a journal that never replays, gets one
/// read a minute per type or record, not a tight loop of them. The cost is
/// time: a record whose read failed only briefly gets its timer within a
/// minute of the store recovering. The retries never stop, because the walk
/// cannot tell a passing store error from a journal that will never replay,
/// and a record with no timer row has nothing else that would arm its timer.
pub(super) const FOLLOW_RETRY_MAX: Duration = Duration::from_secs(60);

/// Who armed the timer of a record whose row the boot dropped (its state no
/// longer declares a timeout), so the record's new timer fires as them.
pub(super) type BootCallers = BTreeMap<(TenantId, String, String), AgentContext>;

/// A record a walk could not read, with whoever armed its dropped row.
type UnreadableRecord = ((TenantId, String, String), Option<AgentContext>);

/// What the boot did with one timed type's timers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BootTimerCounts {
    /// Rows armed again.
    pub rearmed: usize,
    /// Rows removed: their state no longer declares a timeout.
    pub dropped: usize,
    /// Records in a timed state whose timeout binds a caller, with no row
    /// to say who: not armed, they wait for their next action.
    pub waiting: usize,
    /// Scheduled actions (`schedule`, `schedule_at`) armed again from their
    /// rows.
    pub scheduled: usize,
    /// Records the boot walk replayed from their journals: those whose timer
    /// it could change, and those the catalog could not vouch for.
    pub replayed: usize,
}

/// What one walk of a type did: records that wait for their next action,
/// and records it replayed.
#[derive(Clone, Copy, Debug, Default)]
struct Walked {
    waiting: usize,
    replayed: usize,
}

/// The boot's [`BootTimerCounts`] by tenant and entity type.
pub type BootTimers = BTreeMap<(TenantId, String), BootTimerCounts>;

/// While held, one tenant's changed types wait out of every walk (see
/// [`ServerState::hold_timer_follow`]). Dropping it, on success or failure,
/// makes them ready for the next walk.
pub struct TimerFollowHold {
    server: ServerState,
    tenant: TenantId,
}

impl Drop for TimerFollowHold {
    fn drop(&mut self) {
        if let Ok(mut registry) = self.server.registry.write() {
            registry.finish_install(&self.tenant);
        }
    }
}

impl ServerState {
    /// Make the timers of every record of each type whose spec an install
    /// changed follow the state the record is in now, including a type that
    /// had no timeout before. Failed listings and reads remain pending until
    /// the store recovers. Startup installs defer to `start_runtime`'s
    /// boot walk, preserving saved callers and its shared fire gate.
    pub async fn follow_spec_changes(&self) {
        let changed = match self.registry.write() {
            Ok(mut registry) => {
                // Startup installs precede start_runtime. The boot walk owns
                // these changes: only it has the saved callers and fire gate.
                if !self
                    .timer_recovery_complete
                    .load(std::sync::atomic::Ordering::Acquire)
                {
                    return;
                }
                registry.take_changed_types()
            }
            Err(_) => return,
        };
        self.follow_types(changed, &BootCallers::new(), None, Arming::Any, None)
            .await;
    }

    /// Hold `tenant`'s changed types back from every walk, any tenant's,
    /// until the returned hold is dropped: while an installation into it
    /// registers specs ahead of the policies and modules its timers need.
    pub fn hold_timer_follow(&self, tenant: &TenantId) -> TimerFollowHold {
        self.registry
            .write()
            .expect("registry lock poisoned")
            .begin_install(tenant);
        TimerFollowHold {
            server: self.clone(),
            tenant: tenant.clone(),
        }
    }

    /// [`Self::follow_spec_changes`] in the background, for an install that
    /// answers before every record is read.
    pub fn spawn_follow_spec_changes(&self) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let server = self.clone();
        runtime.spawn(async move { server.follow_spec_changes().await }); // determinism-ok: install follow-up runs in the background
    }

    /// At boot, after the rows are re-armed: make every record of every type
    /// that declares a timeout follow its state, arming a record left in a
    /// timed state with no row, and count the records that wait for a caller
    /// and those it replayed into `counts`, which it logs. A record the
    /// catalog shows already following its state, by `rows`, is not replayed
    /// (see `state_timer_settled`).
    #[tracing::instrument(skip_all, fields(otel.name = "boot.timed_type_walk"))]
    pub(super) async fn follow_timed_types(
        &self,
        callers: BootCallers,
        gate: Arc<Semaphore>,
        mut counts: BootTimers,
        rows: BootRows,
    ) -> BootTimers {
        let types: Vec<(TenantId, String)> = match self.registry.read() {
            Ok(registry) => registry
                .tenant_ids()
                .into_iter()
                .flat_map(|tenant| {
                    registry
                        .entity_types(tenant)
                        .into_iter()
                        .filter(|entity_type| {
                            registry
                                .get_spec(tenant, entity_type)
                                .is_some_and(|spec| !spec.automaton.state_timeouts.is_empty())
                        })
                        .map(|entity_type| (tenant.clone(), entity_type.to_string()))
                        .collect::<Vec<_>>()
                })
                .collect(),
            Err(_) => return counts,
        };
        let walked = self
            .follow_types(types, &callers, Some(gate), Arming::Any, Some(&rows))
            .await;
        for (key, walked) in walked {
            let counted = counts.entry(key).or_default();
            counted.waiting = walked.waiting;
            counted.replayed = walked.replayed;
        }
        let by_type = counts
            .iter()
            .map(|((tenant, entity_type), counts)| {
                format!(
                    "{tenant}/{entity_type} rearmed={} dropped={} waiting={} scheduled={} replayed={}",
                    counts.rearmed, counts.dropped, counts.waiting, counts.scheduled, counts.replayed
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        // The boot check reads this summary through the WARN+ OTLP log export.
        tracing::warn!(
            rearmed = counts.values().map(|c| c.rearmed).sum::<usize>(),
            dropped = counts.values().map(|c| c.dropped).sum::<usize>(),
            waiting = counts.values().map(|c| c.waiting).sum::<usize>(),
            scheduled = counts.values().map(|c| c.scheduled).sum::<usize>(),
            replayed = counts.values().map(|c| c.replayed).sum::<usize>(),
            by_type,
            "state timers after boot"
        );
        counts
    }

    /// Reconcile every record of each of `types` as `arming` allows, but at
    /// boot, given the `rows` it armed, not one the catalog shows already
    /// following its state; and return, for each type, how many records wait
    /// for their next action and how many were replayed. A type
    /// whose records the store cannot list is tried again after a wait. A
    /// record it cannot read is reported and tried again without its type
    /// ([`Self::retry_unreadable_records`]): an unreadable record may have no
    /// timer at all, so the walk keeps responsibility for it, but one broken
    /// journal must not make the whole type be walked again.
    async fn follow_types(
        &self,
        mut types: Vec<(TenantId, String)>,
        callers: &BootCallers,
        gate: Option<Arc<Semaphore>>,
        arming: Arming,
        rows: Option<&BootRows>,
    ) -> BTreeMap<(TenantId, String), Walked> {
        let mut wait = FOLLOW_RETRY_FIRST;
        let mut walked_by_type = BTreeMap::new();
        loop {
            let mut pending = Vec::new();
            let mut unreadable_records: Vec<UnreadableRecord> = Vec::new();
            for (tenant, entity_type) in types {
                if let Err(error) = self.ensure_type_indexed(&tenant, &entity_type).await {
                    tracing::warn!(tenant = %tenant, entity_type, %error,
                        retry_in_secs = wait.as_secs(),
                        "records of a timed type could not be listed; trying again");
                    pending.push((tenant, entity_type));
                    continue;
                }
                let ids = self.list_entity_ids(&tenant, &entity_type);
                let settled = match rows {
                    Some(rows) => self.settled_records(&tenant, &entity_type, rows).await,
                    None => BTreeSet::new(),
                };
                let (mut for_caller, mut for_row, mut unreadable) = (0usize, 0usize, 0usize);
                let mut replayed = 0usize;
                for entity_id in ids.iter().filter(|id| !settled.contains(*id)) {
                    replayed += 1;
                    let key = (tenant.clone(), entity_type.clone(), entity_id.clone());
                    let follow = Follow {
                        armed_by: callers.get(&key),
                        gate: gate.clone(),
                        arming,
                        woke_for: None,
                        prior_arm: None,
                    };
                    let followed = self
                        .reconcile_stored_record(&tenant, &entity_type, entity_id, follow)
                        .await;
                    for_caller += usize::from(followed == TimerFollow::WaitsForCaller);
                    for_row += usize::from(followed == TimerFollow::WaitsForRow);
                    if followed == TimerFollow::RetryRead {
                        unreadable += 1;
                        crate::runtime_metrics::record_state_timer_record_unreadable(
                            tenant.as_str(),
                            &entity_type,
                        );
                        tracing::warn!(tenant = %tenant, entity_type, entity_id,
                            retry_in_secs = FOLLOW_RETRY_FIRST.as_secs(),
                            "record of a timed type could not be read; trying it again without its type");
                        let armed_by = callers.get(&key).cloned();
                        unreadable_records.push((key, armed_by));
                    }
                }
                tracing::info!(tenant = %tenant, entity_type, records = ids.len(), replayed,
                    waiting_for_caller = for_caller, waiting_for_row = for_row, unreadable,
                    "state timers follow the records' states");
                if for_caller > 0 {
                    tracing::warn!(tenant = %tenant, entity_type, waiting = for_caller,
                        "records in a timed state wait for their next action: the timeout action binds its caller and nobody armed it");
                }
                walked_by_type.insert(
                    (tenant, entity_type),
                    Walked {
                        waiting: for_caller + for_row,
                        replayed,
                    },
                );
            }
            if !unreadable_records.is_empty() {
                self.retry_unreadable_records(unreadable_records, gate.clone(), arming);
            }
            if pending.is_empty() {
                return walked_by_type;
            }
            tokio::time::sleep(wait).await; // determinism-ok: retry wait on the runtime clock
            wait = (wait * 2).min(FOLLOW_RETRY_MAX);
            types = pending;
        }
    }

    /// Try the records one pass of a walk could not read again, in the
    /// background, until each reads, then reconcile each as the walk would
    /// have. One task reads them one at a time, as the walk did, so many
    /// unreadable records do not all read a recovering store at once. The wait
    /// between rounds doubles up to [`FOLLOW_RETRY_MAX`], so a journal that
    /// never replays costs one read a minute, and each failed read is logged
    /// where it fails. A read can fail for a passing store error, and a record
    /// may have no timer to come back to it, so this does not give up.
    fn retry_unreadable_records(
        &self,
        mut records: Vec<UnreadableRecord>,
        gate: Option<Arc<Semaphore>>,
        arming: Arming,
    ) {
        let server = self.clone();
        let retry = async move {
            let mut wait = FOLLOW_RETRY_FIRST;
            let mut failed_reads: u32 = 1;
            while !records.is_empty() {
                tokio::time::sleep(wait).await; // determinism-ok: retry wait on the runtime clock
                wait = (wait * 2).min(FOLLOW_RETRY_MAX);
                let mut still_unreadable = Vec::new();
                for ((tenant, entity_type, entity_id), armed_by) in records {
                    let follow = Follow {
                        armed_by: armed_by.as_ref(),
                        gate: gate.clone(),
                        arming,
                        woke_for: None,
                        prior_arm: None,
                    };
                    let followed = server
                        .reconcile_stored_record(&tenant, &entity_type, &entity_id, follow)
                        .await;
                    if followed == TimerFollow::RetryRead {
                        still_unreadable.push(((tenant, entity_type, entity_id), armed_by));
                    } else {
                        tracing::info!(tenant = %tenant, entity_type, entity_id, failed_reads,
                            "record of a timed type read again; its timer follows its state");
                    }
                }
                records = still_unreadable;
                failed_reads += 1;
            }
        };
        tokio::spawn(retry); // determinism-ok: a walk's read retries run in the background
    }

    /// Reconcile one record: with its actor's state when it is in memory,
    /// else with its state replayed from the store. A record that cannot be
    /// read remains pending, even when it has no timer to check it again.
    pub(super) async fn reconcile_stored_record(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        follow: Follow<'_>,
    ) -> TimerFollow {
        let Some(current) = self
            .current_record_state(tenant, entity_type, entity_id)
            .await
        else {
            return TimerFollow::RetryRead;
        };
        if current.total_event_count == 0 {
            return TimerFollow::Settled;
        }
        self.follow_state_timer(tenant, entity_type, entity_id, &current, follow)
            .await
    }

    /// The record's state now, without starting its actor when it has none.
    pub(in crate::state) async fn current_record_state(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
    ) -> Option<EntityState> {
        let key = format!("{tenant}:{entity_type}:{entity_id}");
        let resident = self
            .actor_registry
            .read()
            .is_ok_and(|actors| actors.get(&key).is_some_and(|actor| !actor.is_closed()));
        let journal = self.event_journal();
        let (Some((store, backend)), false) = (journal, resident) else {
            return match self
                .get_tenant_entity_state(tenant, entity_type, entity_id)
                .await
            {
                Ok(response) => Some(response.state),
                Err(error) => {
                    tracing::warn!(tenant = %tenant, entity_type, entity_id, %error,
                        "record not readable; its timer is not reconciled");
                    None
                }
            };
        };
        let table = transition_table_for(self, tenant, entity_type)?;
        let blob_store = self.blob_store_for_tenant(tenant).ok();
        match recover_entity_state_for_walk(
            tenant.as_str(),
            entity_type,
            entity_id,
            &table,
            &store,
            backend,
            &serde_json::json!({}),
            blob_store.as_ref(),
        )
        .await
        {
            Ok(state) => Some(state),
            Err(error) => {
                tracing::warn!(tenant = %tenant, entity_type, entity_id, %error,
                    "record not replayable; its timer is not reconciled");
                None
            }
        }
    }
}
