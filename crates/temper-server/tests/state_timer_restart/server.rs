//! The restart tests' world and servers: a real store and a simulation clock
//! that outlive each server process, and the helpers that drive and read
//! records.
//!
//! Each server runs in its own single-threaded, paused-clock Tokio runtime;
//! dropping the runtime kills its in-memory timers as a process exit would.
//! `sim_now()` reads that paused clock, carried across restarts with the time
//! the server was down ([`ProcessClock`]), so the timer deadlines production
//! code computes and the timer sleeps it waits on share one virtual time. The
//! clock moves only when no task can run, so how fast the machine is decides
//! nothing.

use std::cell::RefCell;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use temper_authz::{PrincipalKind, SecurityContext};
use temper_runtime::persistence::StateTimerRecord;
use temper_runtime::scheduler::{
    DeterministicIdGen, DeterministicRng, SimContextGuard, install_sim_context, sim_now,
};
use temper_runtime::{ActorSystem, tenant::TenantId};
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::state::BootTimers;
use temper_server::storage::BoxedEventStore;
use temper_server::{ServerState, StorageStack};
use temper_store_postgres::{PostgresEventStore, PostgresSchema};
use temper_store_turso::TursoEventStore;
use tokio::sync::Notify;

use super::process_clock::ProcessClock;
use super::{
    COUNTING_POLLER_IOA, CSDL, PERMIT_ALL, POLLER_IOA, PROBE_IOA, RELAY_IOA, TALLY_IOA, TICKER_IOA,
};

/// Simulation time for work already under way to end, such as a boot's
/// background walk or a timer that fell due. That work waits on no timer of
/// its own, so it ends at the instant it started; the wait also covers a store
/// backoff of a few tens of milliseconds.
pub(crate) const SETTLE: Duration = Duration::from_millis(100);

/// Connections the PostgreSQL pool opens before its server starts. It never
/// opens another (see [`held_postgres_pool`]).
const POSTGRES_CONNECTIONS: u32 = 4;

/// Where the servers keep their journal and timer rows.
#[derive(Clone)]
pub(crate) enum Backend {
    Turso(String),
    Postgres(String, PostgresSchema),
}

impl Backend {
    /// Open the store. Call inside the server's runtime.
    async fn open(&self) -> StorageStack {
        match self {
            Self::Turso(url) => {
                StorageStack::from_turso(TursoEventStore::new(url, None).await.unwrap())
            }
            Self::Postgres(url, schema) => {
                let pool = held_postgres_pool(url).await;
                StorageStack::from_postgres(PostgresEventStore::with_schema(pool, schema.clone()))
            }
        }
    }
}

/// A PostgreSQL pool whose round trips hold the paused clock.
///
/// Turso reads and writes its file inside the poll that asks, so a server on
/// Turso is idle only when every task waits on a timer. sqlx waits on a socket
/// instead: with a query in flight and nothing else to run, Tokio would move
/// the clock to the next timer, and a state timer could fire before the query
/// meant to come first. So one task stays runnable while any connection is out
/// of the pool. The pool opens all its connections first, under the same
/// hold, because a connect waits on a socket before any acquire hook runs.
async fn held_postgres_pool(url: &str) -> PgPool {
    let acquired = Arc::new(Notify::new());
    let on_acquire = acquired.clone();
    let pool = PgPoolOptions::new()
        .max_connections(POSTGRES_CONNECTIONS)
        .min_connections(0)
        .idle_timeout(None)
        .max_lifetime(None)
        .test_before_acquire(false)
        .before_acquire(move |_, _| {
            let acquired = on_acquire.clone();
            Box::pin(async move {
                acquired.notify_one();
                Ok(true)
            })
        })
        .connect_lazy(url)
        .unwrap();
    let opening = tokio::spawn({
        let pool = pool.clone();
        async move {
            let mut open = Vec::new();
            for _ in 0..POSTGRES_CONNECTIONS {
                open.push(pool.acquire().await.unwrap());
            }
        }
    });
    while !opening.is_finished() {
        tokio::task::yield_now().await;
    }
    opening.await.unwrap();
    tokio::spawn(hold_clock_while_querying(pool.clone(), acquired));
    pool
}

/// Stay runnable while any connection is out of `pool` (acquired, or on its
/// way back), so the paused clock cannot move under a query in flight.
async fn hold_clock_while_querying(pool: PgPool, acquired: Arc<Notify>) {
    loop {
        if pool.size() as usize > pool.num_idle() {
            tokio::task::yield_now().await;
        } else {
            acquired.notified().await;
        }
    }
}

/// What outlives a server process: the store, and the simulation clock with
/// the time the server was down. Installs the seed's simulation context on
/// this thread until it is dropped.
pub(crate) struct World {
    /// The seed: its ids, and the times the scenario draws.
    pub(crate) seed: u64,
    backend: Backend,
    clock: Arc<ProcessClock>,
    _sim: SimContextGuard,
}

impl World {
    /// A world on `backend` whose ids and draws come from `seed`.
    pub(crate) fn new(seed: u64, backend: Backend) -> Self {
        let clock = Arc::new(ProcessClock::new());
        let sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
        Self {
            seed,
            backend,
            clock,
            _sim: sim,
        }
    }

    /// The scenario's draws for this seed.
    pub(crate) fn rng(&self) -> DeterministicRng {
        DeterministicRng::new(self.seed ^ 0x7e57_a127)
    }
}

/// A draw in `[from, to)` milliseconds.
pub(crate) fn millis_between(rng: &mut DeterministicRng, from: u64, to: u64) -> Duration {
    assert!(from < to, "an empty range of times");
    Duration::from_millis(from + rng.next_bound((to - from) as usize) as u64)
}

/// One server process.
pub(crate) struct Server {
    runtime: tokio::runtime::Runtime,
    state: ServerState,
    /// The journal the server writes, read back by [`Self::journal`].
    journal: BoxedEventStore,
    /// The boot's walk of every timed type, until [`Self::boot_counts`] waits for it.
    boot: RefCell<Option<tokio::task::JoinHandle<BootTimers>>>,
}

impl Server {
    /// Start a server in `world`, the way `temper serve` boots: specs,
    /// policies, then the boot re-arm of timers left waiting.
    pub(crate) fn start(world: &World, name: &str) -> Self {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .unwrap();
        let (state, journal, boot) = runtime.block_on(async {
            world.clock.begin();
            let mut registry = SpecRegistry::new();
            registry.register_tenant(
                "default",
                temper_spec::csdl::parse_csdl(CSDL).unwrap(),
                CSDL.to_string(),
                &[
                    ("Probe", PROBE_IOA),
                    ("Ticker", TICKER_IOA),
                    ("Poller", POLLER_IOA),
                    ("CountingPoller", COUNTING_POLLER_IOA),
                    ("Relay", RELAY_IOA),
                    ("Tally", TALLY_IOA),
                ],
            );
            let mut state = ServerState::from_registry(ActorSystem::new(name), registry);
            let storage = world.backend.open().await;
            let journal = storage.events.clone();
            state.set_storage_stack(storage);
            state
                .authz
                .reload_tenant_policies("default", PERMIT_ALL)
                .unwrap();
            state.rebuild_reaction_dispatcher();
            let boot = state.start_runtime().await;
            (state, journal, boot)
        });
        Self {
            runtime,
            state,
            journal,
            boot: RefCell::new(boot),
        }
    }

    /// Kill the server the way a process exit would, every task stopping,
    /// and keep it down until `until` on the simulation clock.
    pub(crate) fn kill(self, world: &World, until: DateTime<Utc>) {
        let Self {
            runtime,
            state,
            journal,
            ..
        } = self;
        runtime.block_on(async {
            let down = (until - sim_now())
                .to_std()
                .expect("the server comes back later, not earlier");
            world.clock.end(down);
        });
        drop(journal);
        drop(state);
        drop(runtime);
    }

    /// What the boot did with each tenant's and type's timers, once its walk
    /// of every timed type has ended.
    pub(crate) fn boot_counts(&self) -> BootTimers {
        let walk = self.boot.borrow_mut().take().expect("the boot walk, once");
        self.runtime.block_on(walk).expect("the boot walk ends")
    }

    /// Now, on the simulation clock.
    pub(crate) fn now(&self) -> DateTime<Utc> {
        self.runtime.block_on(async { sim_now() })
    }

    /// Run until `at` on the simulation clock. The paused clock jumps from
    /// one timer to the next only when no task can run, so each timer fires
    /// at its own instant, and what it starts ends before the clock moves on.
    pub(crate) fn run_until(&self, at: DateTime<Utc>) {
        self.runtime.block_on(async {
            let wait = (at - sim_now()).to_std().expect("a time still ahead");
            tokio::time::sleep(wait).await;
        });
    }

    /// Run for `by` on the simulation clock (see [`Self::run_until`]).
    pub(crate) fn run_for(&self, by: Duration) {
        // The sleep is made inside the runtime, so it reads the paused clock.
        self.runtime
            .block_on(async move { tokio::time::sleep(by).await });
    }

    /// Create the record, if it does not exist yet.
    pub(crate) fn create(&self, entity_type: &str, id: &str) {
        self.runtime.block_on(async {
            self.state
                .get_or_create_tenant_entity(&TenantId::default(), entity_type, id, json!({}))
                .await
                .unwrap();
        });
    }

    /// The actions the record's journal holds, oldest first.
    pub(crate) fn journal(&self, entity_type: &str, id: &str) -> Vec<String> {
        self.runtime.block_on(async {
            self.journal
                .read_events(&format!("default:{entity_type}:{id}"), 0)
                .await
                .unwrap()
                .into_iter()
                .map(|event| event.event_type)
                .collect()
        })
    }

    /// Send `action` to the record as `who`, creating the record first.
    pub(crate) fn act(&self, entity_type: &str, id: &str, action: &str, who: &AgentContext) {
        self.runtime.block_on(async {
            let tenant = TenantId::default();
            self.state
                .get_or_create_tenant_entity(&tenant, entity_type, id, json!({}))
                .await
                .unwrap();
            let response = self
                .state
                .dispatch_tenant_action(&tenant, entity_type, id, action, json!({}), who)
                .await
                .unwrap();
            assert!(response.success, "{action} on {id}: {:?}", response.error);
        });
    }

    /// Arm many records at once: each is created and sent `Arm`
    /// concurrently.
    pub(crate) fn arm_all(&self, entity_type: &str, ids: &[String], who: &AgentContext) {
        self.runtime.block_on(async {
            let mut arming = tokio::task::JoinSet::new();
            for id in ids {
                let (state, id, who) = (self.state.clone(), id.clone(), who.clone());
                let entity_type = entity_type.to_string();
                arming.spawn(async move {
                    let tenant = TenantId::default();
                    state
                        .get_or_create_tenant_entity(&tenant, &entity_type, &id, json!({}))
                        .await
                        .unwrap();
                    let response = state
                        .dispatch_tenant_action(&tenant, &entity_type, &id, "Arm", json!({}), &who)
                        .await
                        .unwrap();
                    assert!(response.success, "Arm on {id}: {:?}", response.error);
                });
            }
            while let Some(armed) = arming.join_next().await {
                armed.unwrap();
            }
        });
    }

    /// `(counter, fired_by)` for one entity.
    pub(crate) fn read(&self, entity_type: &str, id: &str, counter: &str) -> (usize, Value) {
        self.runtime.block_on(async {
            let entity = self
                .state
                .get_tenant_entity_state(&TenantId::default(), entity_type, id)
                .await
                .unwrap();
            let count = entity.state.counters.get(counter).copied().unwrap_or(0);
            (count, entity.state.fields["fired_by"].clone())
        })
    }

    /// Remove every timer row, as for timers armed before rows were kept.
    pub(crate) fn forget_rows(&self) {
        for row in self.rows() {
            self.runtime.block_on(async {
                let store = self
                    .state
                    .metadata_store_for_tenant("default")
                    .await
                    .unwrap();
                store
                    .delete_state_timer("default", &row.entity_type, &row.entity_id, &row.arm_id)
                    .await
                    .unwrap();
            });
        }
    }

    /// Remove `id`'s timer rows, as for a record whose process stopped
    /// between its event and its row.
    pub(crate) fn forget_row(&self, id: &str) {
        for row in self.rows().into_iter().filter(|row| row.entity_id == id) {
            self.runtime.block_on(async {
                let store = self
                    .state
                    .metadata_store_for_tenant("default")
                    .await
                    .unwrap();
                store
                    .delete_state_timer("default", &row.entity_type, &row.entity_id, &row.arm_id)
                    .await
                    .unwrap();
            });
        }
    }

    /// Every timer row the store holds.
    pub(crate) fn rows(&self) -> Vec<StateTimerRecord> {
        self.runtime.block_on(async {
            let store = self
                .state
                .metadata_store_for_tenant("default")
                .await
                .unwrap();
            store.list_state_timers("default").await.unwrap()
        })
    }

    /// Whether the store holds a timer row for `id`.
    pub(crate) fn has_row(&self, id: &str) -> bool {
        self.rows().iter().any(|row| row.entity_id == id)
    }

    /// Write a row back, as one whose delete never happened.
    pub(crate) fn restore_row(&self, row: &StateTimerRecord) {
        self.runtime.block_on(async {
            let store = self
                .state
                .metadata_store_for_tenant("default")
                .await
                .unwrap();
            store.upsert_state_timer(row).await.unwrap();
        });
    }
}

/// A caller with a verified identity.
pub(crate) fn person(id: &str) -> AgentContext {
    AgentContext {
        security_ctx: Some(SecurityContext::from_verified_jwt(
            id,
            PrincipalKind::Agent,
            Some("test"),
            None,
            None,
            None,
        )),
        ..Default::default()
    }
}

/// A fresh Turso database in a temporary directory.
pub(crate) fn turso() -> (tempfile::TempDir, Backend) {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("file:{}", dir.path().join("events.db").display());
    (dir, Backend::Turso(url))
}
