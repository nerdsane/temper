//! DST: a write the kernel owes lands once the store takes writes again.
//!
//! When a move commits, the kernel owes the writes that follow from it: a
//! module's answer, run in the background or inline; the move a state timer
//! fires; a reaction on another record; a child the move spawns. Nobody will
//! ask for them again, so a store that refuses one must not lose it.
//!
//! Workload, per case and seed: a `Job` takes the move that sets one owed
//! write off, and the store stops taking writes the moment that move's
//! commit lands (`SimEventStore::stop_writes_after_next_append`), so the owed
//! write meets a store that takes no writes. The store stays down for a
//! seeded stretch of a second to an hour (a timer falls due inside it), then
//! takes writes again in the same server, with no restart. The first write
//! to the owed write's journal after that meets a fault drawn from the seed
//! (`dst_commit_outcome/faults.rs`): it fails once more, conflicts, lands
//! late, lands and answers an error, or lands and answers an error the
//! journal cannot be read to explain (outcome unknown), so a retry that does
//! not know the first attempt landed applies it twice. Half the callers send
//! an `Idempotency-Key`.
//!
//! A store fails in other ways too (`Outage`), and the kernel keeps the owed
//! write through each: the owed write's first append hangs while the store
//! takes no writes, so its caller times out rather than being refused; the
//! owing record's journal cannot be read by a server that has not loaded
//! the record since it started; or the blob store takes nothing, so an
//! answer too large to keep inline cannot be stored.
//!
//! Invariants, per seed:
//! - the move that sets the write off lands exactly once;
//! - within [`RECOVERY_WINDOW`] of the store taking writes again, the owed
//!   write has landed exactly once: never lost, never applied twice;
//! - the owing record's state in memory is the state its journal gives;
//! - a timer that fired leaves no row behind, which the next boot would fire
//!   again.
//!
//! So that a run means something: a control run, with the store taking every
//! write, must land the owed write exactly once, so a write lost elsewhere
//! was lost to the outage; each seed checks that the owed write did not
//! land while the store failed, that a store taking no writes refused an
//! append to its journal then (so it landed on a later try, not its first),
//! that a hang met it, and that the fault drawn for the first write after
//! met an append to that journal; and each case must meet an outcome-unknown
//! write from a caller with a key and from one without.
//!
//! The server runs in one single-threaded, paused-clock Tokio runtime, and
//! `sim_now()` reads that clock. The module is real WASM, run by the
//! server's engine; its only host call returns its answer.

#[path = "dst_commit_outcome/faults.rs"]
mod faults;
#[path = "dst_state_timers_follow_state/process_clock.rs"]
mod process_clock;
#[path = "dst_owed_writes_land_after_refusal/specs.rs"]
mod specs;
#[path = "dst_state_timers_follow_state/world.rs"]
mod world;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use faults::Fault;
use process_clock::ProcessClock;
use serde_json::json;
use temper_runtime::ActorSystem;
use temper_runtime::scheduler::{
    DeterministicIdGen, DeterministicRng, install_sim_context, sim_now,
};
use temper_runtime::tenant::TenantId;
use temper_server::entity_actor::effects::DEFAULT_FIELD_INLINE_MAX;
use temper_server::idempotency::IdempotencyKey;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::state::DispatchExtOptions;
use temper_server::{ServerState, StorageStack};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;
use world::SimTimerRows;

const SEEDS: u64 = 48;
const TENANT: &str = "default";
/// The record that takes the move, and the id of its `Ledger`.
const JOB: &str = "job";
/// The `Child` that `Spawn` creates.
const CHILD: &str = "child";
/// The longest the store stays down, before a timer's wait is added.
const LONGEST_STRETCH_SECS: usize = 3_600;
/// The longest wait between two tries of an owed write: the kernel's
/// `state::dispatch::owed_writes::LAST_RETRY_WAIT`, which a test cannot read.
const LAST_RETRY_WAIT: Duration = Duration::from_secs(3_600);
/// How soon after the store takes writes again an owed write has landed. The
/// first try after the store is back comes within one capped wait and meets
/// the seeded fault, so the write may land only on the try after it, up to
/// two capped waits after the store is back. The minute on top covers the
/// work of those tries in the process (the fault's late landing and the
/// actor's conflict retries take milliseconds).
const RECOVERY_WINDOW: Duration = LAST_RETRY_WAIT
    .saturating_mul(2)
    .saturating_add(Duration::from_secs(60));

/// The write a move sets off, which the kernel owes once the move commits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Owed {
    /// `Ask` runs the module in the background: its `Answer`.
    BackgroundAnswer,
    /// `Ask` waits for the module (`await_integration`): its `Answer`.
    InlineAnswer,
    /// `Hold` enters `Waiting`: the `Expire` its timer fires.
    TimerFire,
    /// `Note` counts itself on the record's `Ledger`: the `Count`.
    Reaction,
    /// `Spawn` creates a `Child`: its creation and its `Init`.
    ChildSpawn,
}

impl Owed {
    /// The `Job` move that sets the write off, and its parameters.
    fn trigger(self) -> (&'static str, serde_json::Value) {
        match self {
            Self::BackgroundAnswer | Self::InlineAnswer => ("Ask", json!({})),
            Self::TimerFire => ("Hold", json!({})),
            Self::Reaction => ("Note", json!({})),
            Self::ChildSpawn => ("Spawn", json!({ "child_id": CHILD })),
        }
    }

    /// The record that owes the write, and the events its journal must hold
    /// exactly once.
    fn owing(self) -> (Record, &'static [&'static str]) {
        let job = Record {
            entity_type: "Job",
            id: JOB,
        };
        match self {
            Self::BackgroundAnswer | Self::InlineAnswer => (job, &["Answer"]),
            Self::TimerFire => (job, &["Expire"]),
            Self::Reaction => (
                Record {
                    entity_type: "Ledger",
                    id: JOB,
                },
                &["Count"],
            ),
            Self::ChildSpawn => (
                Record {
                    entity_type: "Child",
                    id: CHILD,
                },
                &["Created", "Init"],
            ),
        }
    }
}

/// How the store fails the owed write, from the move that sets it off until
/// the stretch ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outage {
    /// It takes no writes from the moment the move commits.
    Refuses,
    /// The same, and the owed write's first append hangs until the stretch
    /// ends, so its caller times out instead of being refused.
    Hangs,
    /// The owing record's journal cannot be read, and the server has not
    /// loaded the record since it started. Writes go on.
    CannotRead,
    /// The blob store takes nothing, so a field too large to keep inline
    /// cannot be stored. The event store goes on.
    RefusesBlobs,
}

impl Outage {
    /// What the module answers `Ask` with: when blobs are refused, a value
    /// too large to keep inline, so the answer's write stores a blob first.
    fn answer(self) -> serde_json::Value {
        match self {
            Self::RefusesBlobs => json!("a".repeat(DEFAULT_FIELD_INLINE_MAX)),
            Self::Refuses | Self::Hangs | Self::CannotRead => json!(42),
        }
    }

    /// Fail the store this way before the move.
    fn begin(self, store: &SimEventStore, state: &ServerState, job: Record, owing: Record) {
        match self {
            Self::Refuses => store.stop_writes_after_next_append(&job.journal()),
            Self::Hangs => {
                store.stop_writes_after_next_append(&job.journal());
                store.hang_next_append(&owing.journal());
            }
            Self::CannotRead => store.fail_next_reads(&owing.journal(), usize::MAX),
            // A file where the blob directory goes, so no blob can be stored.
            Self::RefusesBlobs => std::fs::write(state.data_dir.join("blobs"), b"")
                .expect("the blob directory is blocked"),
        }
    }

    /// Let the store work again.
    fn end(self, store: &SimEventStore, state: &ServerState, owing: Record) {
        match self {
            Self::Refuses | Self::Hangs => store.take_writes_again(),
            Self::CannotRead => store.fail_next_reads(&owing.journal(), 0),
            Self::RefusesBlobs => std::fs::remove_file(state.data_dir.join("blobs"))
                .expect("the blob directory is unblocked"),
        }
    }

    /// Whether the store takes no writes, so the owed write's tries are
    /// refused and counted (`SimEventStore::refused_appends`). A hung append
    /// is let go while it still takes none.
    fn stops_writes(self) -> bool {
        matches!(self, Self::Refuses | Self::Hangs)
    }

    /// What the store did not do while it failed.
    fn refused(self) -> &'static str {
        match self {
            Self::Refuses | Self::Hangs => "the store took no writes",
            Self::CannotRead => "its journal could not be read",
            Self::RefusesBlobs => "the store took no blobs",
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Record {
    entity_type: &'static str,
    id: &'static str,
}

impl Record {
    fn journal(self) -> String {
        format!("{TENANT}:{}:{}", self.entity_type, self.id)
    }
}

/// A record's journal, read back.
struct Journal {
    /// Each event's type, in order.
    events: Vec<String>,
    /// The newest sequence.
    sequence: u64,
    /// The status the journal leaves the record in.
    status: String,
}

impl Journal {
    fn count(&self, event_type: &str) -> usize {
        self.events
            .iter()
            .filter(|event| *event == event_type)
            .count()
    }
}

/// Read past any read failure the store is set to inject.
fn read_journal(store: &SimEventStore, record: Record) -> Journal {
    let events = store.dump_journal(&record.journal());
    let status = events
        .iter()
        .rev()
        .find_map(|event| {
            event.payload["to_status"]
                .as_str()
                .filter(|status| !status.is_empty())
        })
        .unwrap_or_default()
        .to_string();
    Journal {
        events: events
            .iter()
            .map(|event| event.event_type.clone())
            .collect(),
        sequence: events.last().map_or(0, |event| event.sequence_nr),
        status,
    }
}

struct Run {
    seed: u64,
    trace: Vec<String>,
}

impl Run {
    fn fail(&self, invariant: &str, detail: String) -> String {
        format!(
            "seed {}: {invariant}: {detail}\ntrace:\n  {}",
            self.seed,
            self.trace.join("\n  ")
        )
    }
}

/// Let every task that can run, run.
async fn settle() {
    for _ in 0..400 {
        tokio::task::yield_now().await;
    }
}

/// A server on `store`, whose module answers as `outage` needs, and which
/// keeps object blobs in `blobs` when it is given.
fn server(
    store: &SimEventStore,
    rows: &Arc<SimTimerRows>,
    seed: u64,
    outage: Outage,
    blobs: Option<&Path>,
) -> ServerState {
    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(specs::CSDL).expect("CSDL parses");
    registry.register_tenant(
        TENANT,
        csdl,
        specs::CSDL.to_string(),
        &[
            ("Job", specs::JOB),
            ("Ledger", specs::LEDGER),
            ("Child", specs::CHILD),
        ],
    );
    let mut state = ServerState::from_registry(ActorSystem::new(format!("owed-{seed}")), registry);
    state.set_storage_stack(StorageStack::from_sim(store.clone(), None));
    state.state_timer_rows = Some(rows.clone());
    if let Some(blobs) = blobs {
        // A field too large to keep inline is stored here, and its event
        // holds a reference to it.
        state.data_dir = blobs.to_path_buf();
    }
    // A reaction and a callback are authorized as moves of their own: with no
    // tenant policy, Cedar refuses them and nothing would be owed.
    state
        .authz
        .reload_tenant_policies(TENANT, "permit(principal, action, resource);")
        .expect("the tenant policy parses");
    state.rebuild_reaction_dispatcher();
    let hash = state
        .wasm_engine
        .compile_and_cache(specs::oracle_module(&outage.answer()).as_bytes())
        .expect("the module compiles");
    state.register_wasm_module(&TenantId::new(TENANT), "oracle", &hash);
    state
}

/// Dispatch the move that sets `owed` off, and say how its caller was
/// answered.
async fn set_off(state: &ServerState, owed: Owed, caller: &AgentContext) -> String {
    let (action, params) = owed.trigger();
    let options = DispatchExtOptions {
        agent_ctx: caller,
        await_integration: owed == Owed::InlineAnswer,
        await_reactions: true,
    };
    let answer = state
        .dispatch_tenant_action_ext(&TenantId::new(TENANT), "Job", JOB, action, params, options)
        .await;
    match answer {
        Ok(response) => format!("success {}, error {:?}", response.success, response.error),
        Err(error) => format!("dispatch error {error}"),
    }
}

/// What a seed drew.
struct Drawn {
    /// What the first write to the owed write's journal met once the store
    /// took writes again.
    fault: Fault,
    /// Whether the caller sent an `Idempotency-Key`.
    keyed: bool,
}

/// One seed of `owed` meeting `outage`, and the first broken invariant. With
/// `store_fails` false the store works throughout and nothing is faulted:
/// the control, in which the owed write must land too.
fn run_seed(
    owed: Owed,
    outage: Outage,
    seed: u64,
    store_fails: bool,
) -> (Drawn, Result<(), String>) {
    // Its own directory for object blobs, which the outage can block.
    let blob_dir = (outage == Outage::RefusesBlobs)
        .then(|| tempfile::tempdir().expect("a blob directory is made"));
    let blobs = blob_dir.as_ref().map(tempfile::TempDir::path);
    let clock = Arc::new(ProcessClock::new());
    let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .expect("runtime builds");
    let store = SimEventStore::no_faults(seed);
    let rows = Arc::new(SimTimerRows::new(seed));
    let mut rng = DeterministicRng::new(seed ^ 0x0_ed_57_0e);
    // Xorshift from a small seed: its first outputs move together, and so
    // would the fault, the stretch and the key.
    for _ in 0..8 {
        rng.next_u64();
    }
    let drawn = Fault::draw(&mut rng);
    let fault = if store_fails { drawn } else { Fault::None };
    let mut stretch = Duration::from_secs(1 + rng.next_bound(LONGEST_STRETCH_SECS) as u64);
    if owed == Owed::TimerFire {
        // The timer falls due while the store takes no writes.
        stretch += Duration::from_secs(specs::TIMER_SECS);
    }
    let keyed = rng.next_bound(2) == 0;
    let caller = AgentContext {
        idempotency_key: keyed.then(|| IdempotencyKey::Request(format!("caller-{seed}"))),
        ..AgentContext::default()
    };
    let (owing, owed_events) = owed.owing();
    let job = Record {
        entity_type: "Job",
        id: JOB,
    };
    let mut run = Run {
        seed,
        trace: Vec::new(),
    };
    let ended = runtime.block_on(async {
        clock.begin();
        let mut state = server(&store, &rows, seed, outage, blobs);
        let tenant = TenantId::new(TENANT);
        let mut records = vec![job];
        if owed == Owed::Reaction {
            records.push(owing);
        }
        for record in records {
            state
                .get_or_create_tenant_entity(
                    &tenant,
                    record.entity_type,
                    record.id,
                    json!({ "Id": record.id }),
                )
                .await
                .map_err(|error| run.fail("records are created", error.to_string()))?;
        }
        if outage == Outage::CannotRead {
            // The server starts again, so the owing record's actor is not
            // loaded and must read its journal to start.
            state = server(&store, &rows, seed, outage, blobs);
        }

        if store_fails {
            outage.begin(&store, &state, job, owing);
        }
        let (action, _) = owed.trigger();
        let sent_at = sim_now();
        let answer = set_off(&state, owed, &caller).await;
        run.trace.push(format!(
            "{action} (keyed: {keyed}, outage: {outage:?}, the store fails: {store_fails}): \
             {answer} after {}s",
            (sim_now() - sent_at).num_seconds()
        ));
        tokio::time::sleep(stretch).await;
        settle().await;
        if store_fails && outage == Outage::Hangs {
            // Let the hung append go while the store still takes no writes and
            // long after its caller gave up (the move awaits its reactions):
            // it and the caller's tries queued behind it are refused. They
            // run before the second passes, as the paused clock moves only
            // once every task is idle.
            if !store.let_hung_appends_go() {
                return Err(run.fail(
                    "the owed write's first append hangs (harness)",
                    format!("no append to {} hung", owing.journal()),
                ));
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
            settle().await;
        }
        let during = read_journal(&store, owing);
        let refused = store.refused_appends(&owing.journal());
        run.trace.push(format!(
            "{}s later {} holds {:?}, and the store refused {refused} appends to it",
            stretch.as_secs(),
            owing.journal(),
            during.events
        ));
        let early = owed_events.iter().find(|event| during.count(event) > 0);
        if let Some(early) = early.filter(|_| store_fails) {
            return Err(run.fail(
                "the owed write does not land during the outage (harness)",
                format!("{early} landed while {}", outage.refused()),
            ));
        }
        // A write the store never refused was never tried while it was down,
        // or writes a journal other than the one this case reads.
        if store_fails && outage.stops_writes() && refused == 0 {
            return Err(run.fail(
                "the owed write met the refusal (harness)",
                format!(
                    "no append to {} was refused while the store took no writes",
                    owing.journal()
                ),
            ));
        }

        if store_fails {
            outage.end(&store, &state, owing);
        }
        fault.apply(&store, &owing.journal());
        tokio::time::sleep(RECOVERY_WINDOW).await;
        settle().await;
        let fault_met = !store.append_faults_pending(&owing.journal());
        Fault::clear(&store, &owing.journal());
        let after = read_journal(&store, owing);
        run.trace.push(format!(
            "the store works again, the first write to {} {fault:?} (met: {fault_met}); {}s later it holds {:?}",
            owing.journal(),
            RECOVERY_WINDOW.as_secs(),
            after.events
        ));

        let moves = read_journal(&store, job).count(action);
        if moves != 1 {
            return Err(run.fail(
                "the move that sets the write off lands exactly once",
                format!("{action} landed {moves} times"),
            ));
        }
        for event in owed_events {
            let landed = after.count(event);
            if landed != 1 {
                return Err(run.fail(
                    "an owed write lands exactly once after the store takes writes again",
                    format!("{event} landed {landed} times"),
                ));
            }
        }
        let memory = state
            .get_tenant_entity_state(&tenant, owing.entity_type, owing.id)
            .await
            .map_err(|error| run.fail("the owing record reads", error))?;
        if memory.state.sequence_nr != after.sequence || memory.state.status != after.status {
            return Err(run.fail(
                "the record's state in memory is the state its journal gives",
                format!(
                    "memory seq {} {}, journal seq {} {}",
                    memory.state.sequence_nr, memory.state.status, after.sequence, after.status
                ),
            ));
        }
        if owed == Owed::TimerFire
            && let Some(row) = rows.row(TENANT, JOB)
        {
            return Err(run.fail(
                "a timer that fired leaves no row behind",
                format!("{row:?}"),
            ));
        }
        // Checked last, so that a write that never lands fails on that.
        if !fault_met {
            return Err(run.fail(
                "the owed write met the fault drawn for it (harness)",
                format!("the {fault:?} set on {} met no append", owing.journal()),
            ));
        }
        clock.end(Duration::ZERO);
        Ok::<(), String>(())
    });
    (Drawn { fault, keyed }, ended)
}

/// Run `owed` meeting `outage` over every seed and fail with each broken
/// invariant, the seeds that broke it and the first report.
fn check_over_seeds(owed: Owed, outage: Outage) {
    // With the store working, the move sets its write off and the write
    // lands: a write lost in a seed whose store failed was lost to the outage.
    if let (_, Err(report)) = run_seed(owed, outage, 0, false) {
        panic!(
            "{owed:?}, {outage:?}: the owed write lands while the store works (harness): {report}"
        );
    }
    let mut broken: BTreeMap<String, (Vec<u64>, String)> = BTreeMap::new();
    // Seeds whose owed write met an outcome-unknown write, by whether the
    // caller sent a key.
    let mut outcome_unknown: BTreeMap<bool, usize> = BTreeMap::new();
    for seed in 0..SEEDS {
        let (drawn, ended) = run_seed(owed, outage, seed, true);
        if drawn.fault == Fault::LandThenFailUnreadable {
            *outcome_unknown.entry(drawn.keyed).or_default() += 1;
        }
        if let Err(report) = ended {
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
        "{owed:?}, {outage:?}: invariants broken over {SEEDS} seeds:\n\n{}",
        summary.join("\n\n")
    );
    // A harness that never loses the answer to a write that landed cannot
    // catch a retry that applies it twice, with or without a caller's key.
    assert!(
        outcome_unknown.len() == 2,
        "{owed:?}, {outage:?}: over {SEEDS} seeds, outcome-unknown writes met only these callers (keyed: count): {outcome_unknown:?}"
    );
}

/// A module answer that arrives while the store refuses writes.
#[test]
fn dst_a_module_answer_during_a_store_refusal_lands_after_recovery_exactly_once() {
    check_over_seeds(Owed::BackgroundAnswer, Outage::Refuses);
}

/// The same, for a caller that waits for the answer.
#[test]
fn dst_an_inline_module_answer_during_a_store_refusal_lands_after_recovery_exactly_once() {
    check_over_seeds(Owed::InlineAnswer, Outage::Refuses);
}

#[test]
fn dst_a_state_timer_due_during_a_store_refusal_fires_after_recovery_exactly_once() {
    check_over_seeds(Owed::TimerFire, Outage::Refuses);
}

#[test]
fn dst_a_reaction_during_a_store_refusal_lands_after_recovery_exactly_once() {
    check_over_seeds(Owed::Reaction, Outage::Refuses);
}

#[test]
fn dst_a_child_spawned_during_a_store_refusal_is_created_after_recovery_exactly_once() {
    check_over_seeds(Owed::ChildSpawn, Outage::Refuses);
}

/// A reaction whose write hangs in a store outage: its caller times out
/// rather than being refused. Fails until the kernel keeps an owed write
/// whose store did not answer.
#[test]
fn dst_a_reaction_whose_write_hangs_in_a_store_outage_lands_after_recovery_exactly_once() {
    check_over_seeds(Owed::Reaction, Outage::Hangs);
}

/// A reaction on a record whose journal a restarted server cannot read.
/// Fails until the kernel keeps an owed write the store could not read for.
#[test]
fn dst_a_reaction_on_a_record_that_cannot_be_read_lands_after_reads_return_exactly_once() {
    check_over_seeds(Owed::Reaction, Outage::CannotRead);
}

/// A module answer too large to keep inline, whose blob the store does not
/// take. Fails until the kernel keeps an owed write whose blob was refused.
#[test]
fn dst_a_module_answer_whose_blob_is_refused_lands_after_recovery_exactly_once() {
    check_over_seeds(Owed::BackgroundAnswer, Outage::RefusesBlobs);
}
