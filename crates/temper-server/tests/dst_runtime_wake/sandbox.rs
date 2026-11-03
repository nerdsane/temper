//! The sandbox, the requests that reach it and its control plane, run against
//! the process.
//!
//! The sandbox is awake from each request until exactly [`SUSPEND_AFTER`]
//! plus up to [`JITTER`] seconds after it. While awake, the process's clock
//! runs and its timers fire on their own. Suspended, nothing of the process
//! runs and only the wall clock moves on, until the next request: one from
//! the workload, or the control plane's wake for the due time it holds. As
//! katagami-control's `Wake` does, the control plane sends it [`WAKE_LEAD`]
//! seconds before that time, and it reaches the process up to [`MOST_LAG`]
//! seconds after it is sent. The control plane holds the newest report it
//! took, by `(reported_at, seq)`, and wakes the process once for it.
//! Half its wakes are `GET /healthz`, which `temper serve` answers outside
//! the kernel's routes: they resume the sandbox and keep it awake, but the
//! process does not see them as inbound.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::json;
use temper_runtime::ActorSystem;
use temper_runtime::scheduler::{
    DeterministicIdGen, DeterministicRng, install_sim_context, sim_now,
};
use temper_server::registry::SpecRegistry;
use temper_server::{ServerState, StorageStack};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;

use super::checks::{self, Rec, Start};
use super::world::{Clock, Reports, TimerRows};
use super::{AWAKE, CSDL, JITTER, JOB, SUSPEND_AFTER, TENANT, act, job, settle};

/// How long a run lasts on the wall clock.
const RUN: i64 = 8 * 3600;

/// How long before the due time it holds the control plane sends its wake:
/// katagami-control's `Wake` asks a second early.
const WAKE_LEAD: i64 = 1;

/// The most a wake takes to reach the process after the control plane sends
/// it: it lands from [`WAKE_LEAD`] s before the due time to 3 s after.
const MOST_LAG: u64 = 4;

/// How the control plane orders what it is told: `(reported_at, seq)`.
type Order = (DateTime<Utc>, u64);

/// What one run is told and does.
pub(crate) struct Setup {
    /// How long the process is told it stays awake; None: not told.
    pub(crate) awake: Option<Duration>,
    pub(crate) kind: Kind,
}

pub(crate) enum Kind {
    /// Requests now and then, each making a record or only keeping the
    /// sandbox awake.
    Mixed,
    /// One record whose `Pulse` fires every minute; when `busy`, a request
    /// every 5 to 30 s besides.
    Chain { busy: bool },
    /// Two records due 30 s apart about five hours on, and a request a
    /// minute before the first that resumes the sandbox: the control plane
    /// wakes it for the first, and the second, now inside the window and
    /// not reported, must fire on its own timer, armed before the
    /// suspension.
    Resumed,
}

/// A request that arrives at `at`, making a record that starts so, or none.
struct Arrival {
    at: DateTime<Utc>,
    start: Option<Start>,
}

fn secs(n: u64) -> chrono::Duration {
    chrono::Duration::seconds(n as i64)
}

fn arrivals(kind: &Kind, rng: &mut DeterministicRng, from: DateTime<Utc>) -> Vec<Arrival> {
    let end = from + secs(RUN as u64);
    let mut out = Vec::new();
    let mut at = from;
    match kind {
        Kind::Mixed => {
            while at < end {
                let start = match rng.next_bound(10) {
                    0..=3 => Some(Start::Plan {
                        due: at + secs(30 + rng.next_bound(6 * 3600) as u64),
                    }),
                    4 | 5 => Some(Start::Wait),
                    6 => Some(Start::Activate),
                    _ => None,
                };
                out.push(Arrival { at, start });
                // Mostly close together, sometimes past the sandbox's
                // window, sometimes hours apart.
                at += secs(match rng.next_bound(20) {
                    0..=11 => rng.next_bound(300),
                    12..=16 => 900 + rng.next_bound(2700),
                    _ => 3600 + rng.next_bound(4 * 3600),
                } as u64);
            }
        }
        Kind::Chain { busy } => {
            out.push(Arrival {
                at,
                start: Some(Start::Hum),
            });
            while *busy && at < end {
                at += secs(5 + rng.next_bound(26) as u64);
                out.push(Arrival { at, start: None });
            }
        }
        Kind::Resumed => {
            let first = at + secs(5 * 3600 - 30 - rng.next_bound(60) as u64);
            for due in [first, first + secs(30)] {
                out.push(Arrival {
                    at,
                    start: Some(Start::Plan { due }),
                });
            }
            out.push(Arrival {
                at: first - secs(30),
                start: None,
            });
        }
    }
    out
}

/// One seed's run. Err: the invariant it broke, with its trace.
pub(crate) fn run(seed: u64, setup: Setup) -> Result<(), String> {
    let clock = Arc::new(Clock::new());
    let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
    let events = SimEventStore::no_faults(seed);
    let rows = Arc::new(TimerRows::default());
    let reports = Arc::new(Reports::new(seed));
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
        let mut state = ServerState::from_registry(ActorSystem::new("wake"), registry);
        state.set_storage_stack(StorageStack::from_sim(events.clone(), None));
        state.state_timer_rows = Some(rows);
        state.set_wake_sink(reports.clone());
        if let Some(awake) = setup.awake {
            state.set_wake_awake(awake);
        }
        state.start_runtime().await.expect("start").await.unwrap();
        let mut rng = DeterministicRng::new(seed);
        let arrivals = arrivals(&setup.kind, &mut rng, sim_now());
        let mut sim = Sim {
            state,
            events,
            reports,
            clock: clock.clone(),
            rng,
            awake_until: sim_now(),
            plan: None,
            held: None,
            woke_for: None,
            lag: 0,
            wakes: 0,
            recs: Vec::new(),
            trace: Vec::new(),
        };
        sim.go(arrivals, &setup.kind).await
    });
    drop(runtime);
    result.map_err(|failure| format!("seed {seed}: {failure}"))
}

struct Sim {
    state: ServerState,
    events: SimEventStore,
    reports: Arc<Reports>,
    clock: Arc<Clock>,
    rng: DeterministicRng,
    /// When the sandbox is suspended, unless a request comes first.
    awake_until: DateTime<Utc>,
    /// When the control plane will wake the process next.
    plan: Option<DateTime<Utc>>,
    /// The `(reported_at, seq)` of what the control plane holds.
    held: Option<Order>,
    /// What the control plane held when it woke the process last.
    woke_for: Option<Order>,
    /// How long the next wake takes to reach the process, in seconds.
    lag: u64,
    /// How many times the control plane woke the process at a time it held.
    wakes: u64,
    recs: Vec<Rec>,
    trace: Vec<String>,
}

impl Sim {
    fn fail(&self, (invariant, detail): (&str, String)) -> String {
        let from = self.trace.len().saturating_sub(120);
        format!(
            "{invariant}: {detail}\ntrace:\n  {}",
            self.trace[from..].join("\n  ")
        )
    }

    /// A request reached the sandbox, which stays awake from now. The process
    /// notes it as inbound when it `reached_routes`: the kernel's routes.
    fn request(&mut self, reached_routes: bool) {
        if reached_routes {
            self.state.note_inbound();
        }
        let after = chrono::Duration::from_std(SUSPEND_AFTER).unwrap();
        self.awake_until = sim_now() + after + secs(self.rng.next_bound(JITTER as usize) as u64);
    }

    /// The control plane holds what it was told if it is newer, by
    /// `(reported_at, seq)`, than what it holds.
    fn hold(&mut self, key: Order, next_due_at: Option<DateTime<Utc>>) {
        if self.held.is_none_or(|held| key > held) {
            self.held = Some(key);
            self.plan = next_due_at;
        }
    }

    /// The control plane takes the newest report delivered to it.
    fn take_reports(&mut self) {
        if let Some(told) = self.reports.told() {
            self.hold((told.reported_at, told.seq), told.next_due_at);
        }
    }

    /// When the wake for what the control plane holds reaches the process.
    fn wake_due(&self) -> Option<DateTime<Utc>> {
        if self.held == self.woke_for {
            return None;
        }
        Some(self.plan? - chrono::Duration::seconds(WAKE_LEAD) + secs(self.lag))
    }

    async fn go(&mut self, arrivals: Vec<Arrival>, kind: &Kind) -> Result<(), String> {
        let end = sim_now() + secs(RUN as u64);
        let mut next = 0;
        loop {
            let now = sim_now();
            self.take_reports();
            let wake = self.wake_due();
            let arrival = arrivals.get(next).map(|a| a.at);
            if let Some(at) = wake
                && at <= now
            {
                self.wake();
                continue;
            }
            if let Some(at) = arrival
                && at <= now
            {
                let start = arrivals[next].start;
                next += 1;
                self.arrive(next, start).await?;
                continue;
            }
            if now >= end {
                break;
            }
            let resume = [wake, arrival, Some(end)]
                .into_iter()
                .flatten()
                .min()
                .unwrap();
            if self.awake_until > now {
                // Awake: the process runs until the next request or its
                // suspension; a report landing may bring a wake sooner.
                let until = resume.min(self.awake_until);
                tokio::select! {
                    _ = tokio::time::sleep((until - now).to_std().unwrap()) => {}
                    _ = self.reports.changed.notified() => {}
                }
                continue;
            }
            // Suspended until the next request: the wall clock moves on and
            // the process's clock does not.
            settle().await;
            self.trace
                .push(format!("suspended at {now} until {resume}"));
            self.clock.suspend((resume - now).to_std().unwrap());
        }
        settle().await;
        checks::journal(&self.events, &self.recs, sim_now())
            .await
            .map_err(|failure| self.fail(failure))?;
        if let Kind::Chain { busy } = kind {
            let sent: BTreeSet<u64> = self
                .reports
                .sent
                .lock()
                .unwrap()
                .iter()
                .map(|r| r.seq)
                .collect();
            let windows = RUN as u64 / AWAKE.as_secs();
            if sent.len() as u64 > windows + 3 || self.wakes > windows + 3 {
                return Err(self.fail((
                    "a minute's chain costs the control plane about one report and one wake \
                     per window, not one per fire",
                    format!(
                        "{} reports and {} wakes in {windows} windows, busy {busy}",
                        sent.len(),
                        self.wakes
                    ),
                )));
            }
        }
        Ok(())
    }

    async fn arrive(&mut self, n: usize, start: Option<Start>) -> Result<(), String> {
        self.request(true);
        let Some(start) = start else {
            return Ok(());
        };
        let id = format!("job-{n}");
        job(&self.state, &id).await;
        let (action, params) = match start {
            Start::Plan { due } => ("Plan", json!({ "due": due.to_rfc3339() })),
            Start::Wait => ("Wait", json!({})),
            Start::Activate => ("Activate", json!({})),
            Start::Hum => ("Hum", json!({})),
        };
        if !act(&self.state, &id, action, params).await {
            return Err(self.fail(("the workload arms", format!("{id} {action} refused"))));
        }
        self.trace
            .push(format!("{id} {action} at {}: {start:?}", sim_now()));
        self.recs.push(Rec { id, start });
        Ok(())
    }

    /// The control plane wakes the process for the time it holds: a request
    /// that resumes the sandbox. The process sees it as inbound only when it
    /// reaches the kernel's routes, not as `GET /healthz`.
    fn wake(&mut self) {
        self.wakes += 1;
        self.woke_for = self.held;
        self.lag = self.rng.next_bound(MOST_LAG as usize + 1) as u64;
        let reached_routes = self.rng.next_bound(2) == 0;
        let by = if reached_routes {
            "a request"
        } else {
            "GET /healthz"
        };
        self.trace.push(format!("woken at {} by {by}", sim_now()));
        self.request(reached_routes);
    }
}
