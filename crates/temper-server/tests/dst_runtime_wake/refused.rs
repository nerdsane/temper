//! DST: a report the control plane refuses on purpose is not sent again.
//!
//! The control plane answers 403 to a pass that is bad or revoked, as when the
//! app was deleted or its pass replaced. Retrying it would leave a dead
//! process knocking for good, so the process sends each refused report once.
//!
//! Workload, per seed: with the control plane answering 403 to everything,
//! the process boots, arms a record due a day away and one due sooner, and
//! lets the sooner one fall due, with hours of quiet after each. Then its pass
//! comes back and it arms one more.
//!
//! Invariants:
//! - each change that brings an earlier due time sends exactly one report
//!   naming it, and so does the refused time passing;
//! - in the quiet after a refused report, nothing is sent: it counts as told
//!   and is not retried;
//! - once the pass comes back, the next change's report is delivered.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::json;
use temper_runtime::ActorSystem;
use temper_runtime::scheduler::{
    DeterministicIdGen, DeterministicRng, install_sim_context, sim_now,
};
use temper_runtime::tenant::TenantId;
use temper_server::registry::SpecRegistry;
use temper_server::wake::{SendError, WakeReport, WakeSink};
use temper_server::{ServerState, StorageStack};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;

use super::world::{Clock, TimerRows};
use super::{CSDL, JOB, TENANT, act, settle};

/// Longer than many retries would take, waiting 1 s and doubling.
const QUIET: Duration = Duration::from_secs(3 * 3600);

/// A control plane that answers 403 to every report while `refusing`, and
/// takes them otherwise.
struct Gate {
    sent: Mutex<Vec<(WakeReport, bool)>>,
    refusing: AtomicBool,
}

#[async_trait::async_trait]
impl WakeSink for Gate {
    async fn send(&self, report: &WakeReport, _traceparent: Option<&str>) -> Result<(), SendError> {
        let refusing = self.refusing.load(Ordering::SeqCst);
        self.sent.lock().unwrap().push((report.clone(), !refusing));
        if refusing {
            return Err(SendError::Refused(403));
        }
        Ok(())
    }
}

/// Since `from` sends, the sends after it must be exactly one report naming
/// `earliest`, delivered or not as `delivered` says.
fn one_report(
    gate: &Gate,
    from: usize,
    earliest: Option<DateTime<Utc>>,
    delivered: bool,
    step: &str,
) -> Result<usize, String> {
    let sent = gate.sent.lock().unwrap();
    let new = &sent[from..];
    match new {
        [(report, took)] if report.next_due_at == earliest && *took == delivered => Ok(sent.len()),
        _ => Err(format!(
            "{step}: each change sends exactly one report naming the earliest due time \
             {earliest:?}; sent {new:?}"
        )),
    }
}

/// After `QUIET`, nothing more was sent since `from`.
async fn quiet(gate: &Gate, from: usize, step: &str) -> Result<(), String> {
    tokio::time::sleep(QUIET).await;
    settle().await;
    let sent = gate.sent.lock().unwrap();
    if sent.len() != from {
        return Err(format!(
            "{step}: a refused report is not sent again; sent {:?}",
            &sent[from..]
        ));
    }
    Ok(())
}

async fn plan(state: &ServerState, id: &str, due: DateTime<Utc>) -> Result<(), String> {
    state
        .get_or_create_tenant_entity(&TenantId::new(TENANT), "Job", id, json!({}))
        .await
        .unwrap();
    if !act(state, id, "Plan", json!({ "due": due.to_rfc3339() })).await {
        return Err(format!("{id} Plan refused"));
    }
    settle().await;
    Ok(())
}

fn run_seed(seed: u64) -> Result<(), String> {
    let clock = Arc::new(Clock::new());
    let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
    let mut rng = DeterministicRng::new(seed);
    let events = SimEventStore::no_faults(seed);
    let gate = Arc::new(Gate {
        sent: Mutex::new(Vec::new()),
        refusing: AtomicBool::new(true),
    });
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .unwrap();
    let result = runtime.block_on(async {
        clock.begin();
        let mut registry = SpecRegistry::new();
        let csdl = parse_csdl(CSDL).unwrap();
        registry.register_tenant(TENANT, csdl, CSDL.to_string(), &[("Job", JOB)]);
        let mut state = ServerState::from_registry(ActorSystem::new("refused"), registry);
        state.set_storage_stack(StorageStack::from_sim(events.clone(), None));
        state.state_timer_rows = Some(Arc::new(TimerRows::default()));
        state.set_wake_sink(gate.clone());
        state.start_runtime().await.expect("start").await.unwrap();
        settle().await;
        let sent = one_report(&gate, 0, None, false, "boot")?;
        quiet(&gate, sent, "after boot").await?;

        let far = sim_now() + chrono::Duration::seconds(24 * 3600 + rng.next_bound(3600) as i64);
        plan(&state, &format!("job-{seed}-far"), far).await?;
        let sent = one_report(&gate, sent, Some(far), false, "first arm")?;
        quiet(&gate, sent, "after the first arm").await?;

        // Due after the quiet that follows it, before the first.
        let near = sim_now() + chrono::Duration::seconds(4 * 3600 + rng.next_bound(3600) as i64);
        plan(&state, &format!("job-{seed}-near"), near).await?;
        let sent = one_report(&gate, sent, Some(near), false, "second arm")?;
        quiet(&gate, sent, "after the second arm").await?;

        // The refused time passes: the control plane, had it taken it, would
        // hold nothing after it, so the next one is sent, once.
        tokio::time::sleep((near - sim_now()).to_std().unwrap() + Duration::from_secs(2)).await;
        settle().await;
        let sent = one_report(&gate, sent, Some(far), false, "the refused time passing")?;
        quiet(&gate, sent, "after the refused time passed").await?;

        // The pass comes back.
        gate.refusing.store(false, Ordering::SeqCst);
        let next = sim_now() + chrono::Duration::seconds(60 + rng.next_bound(600) as i64);
        plan(&state, &format!("job-{seed}-next"), next).await?;
        one_report(&gate, sent, Some(next), true, "arm once the pass is back")?;
        Ok::<(), String>(())
    });
    drop(runtime);
    result.map_err(|failure| format!("seed {seed}: {failure}"))
}

#[test]
fn dst_a_refused_report_is_sent_once_per_change() {
    let failures: Vec<String> = (0..16).filter_map(|seed| run_seed(seed).err()).collect();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
