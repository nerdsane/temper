//! DST: a timer acts with the standing its setter has when it fires.
//!
//! A timer fires as whoever set it. Its move is the app's own, so Cedar is
//! not asked about the move: it is asked whether the setter may read the
//! record, when they set the timer and when it fires. A timer an agent set
//! also stops when the token the agent's identity came from would be refused
//! now: its grant was revoked, or the user it acts for signed out everywhere. The tenant's Cedar here
//! names its people, as an app's does: they may read its records, `Start` a
//! `Job` and `Hold` it. It also names outsiders, who may `Start` and `Hold` a
//! `Job` but never read one. Nobody is permitted the timers' own moves,
//! `Expire`, `Take` and `Lapse`.
//!
//! Workload, per seed: records are set going at seeded times, each by one
//! setter, with a state timer (`Start` enters `Waiting`, whose timeout makes
//! `Expire`), a scheduled action (`Hold` schedules `Take`), or a state timer
//! from creation (a `Pass` is created into `Open`, whose timeout makes
//! `Lapse`). The setters:
//! - a user who stays in the tenant's Cedar, by themselves or through an
//!   agent acting for them;
//! - a user who leaves it after setting the timer and before it fires, by
//!   themselves or through an agent;
//! - an outsider, by themselves or through an agent, who could never read the
//!   record;
//! - an agent whose grant is revoked after it set the timer and before it
//!   fires;
//! - a user who signs out everywhere after setting the timer and before it
//!   fires, by themselves or through an agent;
//! - an agent whose identity kept no credential (a context saved before
//!   contexts kept one), whose user signs out and whose grant is revoked;
//! - the app's own service, which Cedar names nowhere;
//! - an internal caller with no security context.
//!
//! The tenant's Cedar outlives a process, as stored policies do. Some seeds
//! restart the server before the timers fire, with or without them falling
//! due while it is down; every seed restarts once more after they fired.
//!
//! Invariants, long after every timer was due, and again after the last
//! restart:
//! - a timer whose setter may still read the record, could not read it when
//!   they set it, or acts for no one, makes its move exactly once, and so
//!   does a person's after they sign out everywhere and an agent's that kept
//!   no credential;
//! - a timer whose setter could read the record when they set it and can no
//!   longer, or an agent's whose token would now be refused, never makes its
//!   move, and says so in a log line naming the setter;
//! - such a state timer keeps its row, armed as its setter, so no later
//!   arming fires it as anyone else; such a scheduled action leaves no row
//!   and no live timer.
//!
//! Each server runs in its own single-threaded, paused-clock Tokio runtime,
//! and `sim_now()` reads that clock.

#[path = "dst_timer_setter_standing/install_recovery.rs"]
mod install_recovery;
#[path = "dst_timer_setter_standing/invariants.rs"]
mod invariants;
#[path = "dst_timer_setter_standing/logged.rs"]
mod logged;
#[path = "dst_state_timers_follow_state/process_clock.rs"]
mod process_clock;
#[path = "dst_timer_setter_standing/recovery.rs"]
mod recovery;
#[path = "dst_timer_setter_standing/revocation.rs"]
mod revocation;
#[path = "dst_timer_setter_standing/server.rs"]
mod server;
#[path = "dst_timer_setter_standing/specs.rs"]
mod specs;
#[path = "dst_state_timers_follow_state/world.rs"]
mod world;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use invariants::{check, check_logged};
use logged::Withheld;
use process_clock::ProcessClock;
use revocation::{bump_generation, grant};
use serde_json::json;
use server::{Process, World, pass, settle};
use temper_authz::{AuthenticatedRequestContext, IssuedCredential, PrincipalKind, SecurityContext};
use temper_runtime::scheduler::{
    DeterministicIdGen, DeterministicRng, install_sim_context, sim_now,
};
use temper_runtime::tenant::TenantId;
use temper_server::build_router;
use temper_server::request_context::AgentContext;
use temper_store_sim::SimEventStore;
use tower::ServiceExt;
use world::SimTimerRows;

const SEEDS: u64 = 16;
const TENANT: &str = "alpha";
/// Both timers' wait: every record is set going, and every leaver leaves,
/// well within it.
const WAIT: Duration = Duration::from_secs(600);
/// How long the run goes on after the timers fell due.
const HORIZON: Duration = Duration::from_secs(4 * 3600);

/// The agent a user may act through.
const AGENT: &str = "katagami-mcp";

/// Which timer a record is set going with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Timer {
    /// `Start`: `Waiting`'s state timer makes `Expire`.
    State,
    /// `Hold`: a scheduled `Take`.
    Scheduled,
    /// Creating a `Pass`: `Open`'s state timer makes `Lapse`.
    Created,
}

impl Timer {
    /// The move that sets it, the move it makes, the place the record waits
    /// in and the place its move takes the record to.
    fn moves(self) -> (&'static str, &'static str, &'static str, &'static str) {
        match self {
            Self::State => ("Start", "Expire", "Waiting", "Done"),
            Self::Scheduled => ("Hold", "Take", "Held", "Taken"),
            Self::Created => ("create", "Lapse", "Open", "Lapsed"),
        }
    }

    /// The type of the record it is set on.
    fn entity_type(self) -> &'static str {
        match self {
            Self::State | Self::Scheduled => "Job",
            Self::Created => "Pass",
        }
    }
}

/// Whether a user sets a timer themselves or through an agent acting for them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum By {
    Themselves,
    Agent,
}

/// Who sets a record's timer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Setter {
    /// A user who stays in the tenant's Cedar.
    Stays(By),
    /// A user who leaves the tenant's Cedar before the timer fires.
    Leaves(By),
    /// A user the tenant's Cedar lets set a `Job`'s timers but never read it.
    Outsider(By),
    /// An agent acting for a user who stays in the tenant's Cedar, whose
    /// grant is revoked before the timer fires.
    Revoked,
    /// A user who stays in the tenant's Cedar and signs out everywhere before
    /// the timer fires.
    SignsOut(By),
    /// An agent whose identity kept no credential, as one saved before
    /// identities kept it, whose user signs out everywhere and whose grant is
    /// revoked before the timer fires.
    NoCredential,
    /// The app's own service, which the Cedar names nowhere.
    App,
    /// An internal caller with no security context.
    Internal,
}

impl Setter {
    /// The identity the record's user `user` sets the timer as. An agent's
    /// keeps what its token was issued under, as the identity boundary keeps
    /// it: the user, their sign-out generation then (0) and its grant.
    fn caller(self, user: &str) -> AgentContext {
        let verified = |id: &str, kind, agent_type, acting_for| {
            SecurityContext::from_verified_jwt(id, kind, agent_type, acting_for, None, None)
        };
        let agent = verified(AGENT, PrincipalKind::Agent, Some("contributor"), Some(user));
        let security_ctx = match self {
            Self::Stays(By::Themselves)
            | Self::Leaves(By::Themselves)
            | Self::Outsider(By::Themselves)
            | Self::SignsOut(By::Themselves) => verified(user, PrincipalKind::Customer, None, None),
            Self::NoCredential => agent,
            Self::Stays(By::Agent)
            | Self::Leaves(By::Agent)
            | Self::Outsider(By::Agent)
            | Self::Revoked
            | Self::SignsOut(By::Agent) => agent.with_credential(Some(IssuedCredential {
                subject: Some(user.to_string()),
                auth_generation: 0,
                grant_id: Some(grant(user)),
            })),
            Self::App => return AgentContext::for_service("app"),
            Self::Internal => return AgentContext::default(),
        };
        AgentContext {
            security_ctx: Some(security_ctx),
            ..Default::default()
        }
    }

    /// The principal a withheld fire names as its setter.
    fn principal(self, user: &str) -> String {
        match self {
            Self::Stays(By::Agent)
            | Self::Leaves(By::Agent)
            | Self::Outsider(By::Agent)
            | Self::Revoked
            | Self::SignsOut(By::Agent)
            | Self::NoCredential => AGENT.to_string(),
            _ => user.to_string(),
        }
    }

    fn leaves(self) -> bool {
        matches!(self, Self::Leaves(_))
    }

    /// The sign-out generations bumped before the timer fires: the user's
    /// own (signing out everywhere) and their agent's grant (revoking it).
    fn bumps(self, user: &str) -> Vec<String> {
        match self {
            Self::Revoked => vec![grant(user)],
            Self::SignsOut(_) => vec![user.to_string()],
            Self::NoCredential => vec![user.to_string(), grant(user)],
            _ => Vec::new(),
        }
    }

    /// Whether the setter's timer must not make its move: they left the
    /// tenant's Cedar, or they are an agent whose token would be refused now.
    fn loses_standing(self) -> bool {
        matches!(
            self,
            Self::Leaves(_) | Self::Revoked | Self::SignsOut(By::Agent)
        )
    }
}

fn fail(seed: u64, trace: &[String], invariant: &str, detail: String) -> String {
    format!(
        "seed {seed}: {invariant}: {detail}\ntrace:\n  {}",
        trace.join("\n  ")
    )
}

/// One record: its user, its setter and its timer.
struct Record {
    id: String,
    user: String,
    setter: Setter,
    timer: Timer,
}

/// One seed's run.
fn run_seed(seed: u64, withheld: &Withheld) -> Result<(), String> {
    let clock = Arc::new(ProcessClock::new());
    let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
    let mut rng = DeterministicRng::new(seed ^ 0x57a_d1e);
    let mut trace = Vec::new();
    withheld.clear();

    // Every setter with every timer, in a seeded order.
    let mut cases = Vec::new();
    for timer in [Timer::State, Timer::Scheduled, Timer::Created] {
        for setter in [
            Setter::Stays(By::Themselves),
            Setter::Stays(By::Agent),
            Setter::Leaves(By::Themselves),
            Setter::Leaves(By::Agent),
            Setter::Outsider(By::Themselves),
            Setter::Outsider(By::Agent),
            Setter::Revoked,
            Setter::SignsOut(By::Themselves),
            Setter::SignsOut(By::Agent),
            Setter::NoCredential,
            Setter::App,
            Setter::Internal,
        ] {
            // A record is created through the server's route by someone
            // signed in; an internal caller has no way to.
            if !(timer == Timer::Created && setter == Setter::Internal) {
                cases.push((setter, timer));
            }
        }
    }
    for i in (1..cases.len()).rev() {
        cases.swap(i, rng.next_bound(i + 1));
    }
    let records: Vec<Record> = cases
        .into_iter()
        .enumerate()
        .map(|(i, (setter, timer))| Record {
            id: format!("{}-{i}", timer.entity_type().to_lowercase()),
            user: format!("user-{i}"),
            setter,
            timer,
        })
        .collect();

    let (outsiders, people): (Vec<&Record>, Vec<&Record>) = records
        .iter()
        .partition(|record| matches!(record.setter, Setter::Outsider(_)));
    let world = World {
        events: SimEventStore::no_faults(seed),
        rows: Arc::new(SimTimerRows::new(seed)),
        clock,
        people: Mutex::new(people.iter().map(|record| record.user.clone()).collect()),
        outsiders: outsiders.iter().map(|record| record.user.clone()).collect(),
    };
    let tenant = TenantId::new(TENANT);

    let process = Process::start(&world, &format!("standing-{seed}"));
    let set_going = process.runtime.block_on(async {
        for record in &records {
            // Every record is set going within a quarter of the wait.
            pass(Duration::from_secs(rng.next_bound(6) as u64)).await;
            let (sets, ..) = record.timer.moves();
            let caller = record.setter.caller(&record.user);
            let refused = if record.timer == Timer::Created {
                // Through the server's create route, as the setter.
                let who = caller.security_ctx.clone().expect("a creator signs in");
                let mut request = Request::post("/tdata/Passes")
                    .header("Content-Type", "application/json")
                    .body(Body::from(json!({ "Id": record.id }).to_string()))
                    .unwrap();
                request
                    .extensions_mut()
                    .insert(AuthenticatedRequestContext::new(tenant.clone(), who));
                let response = build_router(process.state.clone())
                    .oneshot(request)
                    .await
                    .expect("the router answers");
                let status = response.status();
                let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap_or_default();
                (status != StatusCode::CREATED)
                    .then(|| format!("{status} {}", String::from_utf8_lossy(&body)))
            } else {
                process
                    .state
                    .get_or_create_tenant_entity(&tenant, "Job", &record.id, json!({}))
                    .await
                    .map_err(|error| {
                        fail(seed, &trace, "records are created", error.to_string())
                    })?;
                match process
                    .state
                    .dispatch_tenant_action(&tenant, "Job", &record.id, sets, json!({}), &caller)
                    .await
                {
                    Ok(response) if response.success => None,
                    Ok(response) => Some(response.error.unwrap_or_default()),
                    Err(error) => Some(error),
                }
            };
            if let Some(why) = refused {
                return Err(fail(
                    seed,
                    &trace,
                    "records are set going",
                    format!("{} {:?}: {why}", record.id, record.setter),
                ));
            }
            trace.push(format!(
                "{}: {:?} {sets} at {}",
                record.id,
                record.setter,
                sim_now()
            ));
        }
        settle().await;
        Ok(())
    });
    if let Err(report) = set_going {
        process.kill(&world, Duration::ZERO);
        return Err(report);
    }

    // The leavers leave, well before any timer is due.
    {
        let mut people = world.people.lock().unwrap();
        for record in records.iter().filter(|record| record.setter.leaves()) {
            people.remove(&record.user);
        }
    }
    process
        .state
        .authz
        .reload_tenant_policies(TENANT, &world.cedar())
        .expect("the tenant's Cedar parses");
    trace.push(format!("leavers left at {}", sim_now()));

    // Grants are revoked and users sign out everywhere, as the platform does:
    // by bumping the generation keyed by the grant or the user.
    let revoked = process.runtime.block_on(async {
        for record in &records {
            for key in record.setter.bumps(&record.user) {
                bump_generation(&process.state, &tenant, &key)
                    .await
                    .map_err(|error| fail(seed, &trace, "generations are bumped", error))?;
            }
        }
        settle().await;
        Ok(())
    });
    if let Err(report) = revoked {
        process.kill(&world, Duration::ZERO);
        return Err(report);
    }
    trace.push(format!(
        "grants revoked and users signed out at {}",
        sim_now()
    ));

    // Restart before the timers fire, or while they fall due, or not at all.
    let process = match seed % 3 {
        0 => process,
        restart => {
            let downtime = if restart == 1 {
                Duration::ZERO
            } else {
                WAIT * 2
            };
            trace.push(format!("restart, down {}s", downtime.as_secs()));
            process.kill(&world, downtime);
            Process::start(&world, &format!("standing-{seed}-back"))
        }
    };
    let after_fires = process.runtime.block_on(async {
        pass(WAIT + HORIZON).await;
        check(
            seed,
            &trace,
            &world,
            &process.state,
            &records,
            withheld,
            "after the fires",
        )
        .await
    });
    process.kill(&world, Duration::ZERO);
    after_fires?;
    check_logged(seed, &trace, &records, withheld)?;

    // Every later boot arms the kept rows again, as their setters.
    trace.push("restart after the fires".to_string());
    let process = Process::start(&world, &format!("standing-{seed}-again"));
    let after_restart = process.runtime.block_on(async {
        pass(HORIZON).await;
        check(
            seed,
            &trace,
            &world,
            &process.state,
            &records,
            withheld,
            "after the last restart",
        )
        .await
    });
    process.kill(&world, Duration::ZERO);
    after_restart
}

#[test]
fn dst_a_timer_acts_with_its_setters_current_standing() {
    let withheld = Withheld::install();
    // Seeds that broke each invariant, and the first such seed's report.
    let mut broken: BTreeMap<String, (Vec<u64>, String)> = BTreeMap::new();
    for seed in 0..SEEDS {
        if let Err(report) = run_seed(seed, &withheld) {
            let invariant = report.split(": ").nth(1).unwrap_or("unknown").to_string();
            let entry = broken
                .entry(invariant)
                .or_insert_with(|| (Vec::new(), report));
            entry.0.push(seed);
        }
    }
    let summary: Vec<String> = broken
        .iter()
        .map(|(invariant, (seeds, first))| format!("{invariant}: seeds {seeds:?}\nfirst: {first}"))
        .collect();
    assert!(
        broken.is_empty(),
        "invariants broken over {SEEDS} seeds:\n\n{}",
        summary.join("\n\n")
    );
}
