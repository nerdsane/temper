//! A simulated server process over a store that outlives it, and how the
//! tests move and read.
//!
//! Each process runs on a single-threaded runtime with a paused clock, so its
//! timers and tasks run in one order for a seed. A seed's clock and journal
//! outlive each process, and dropping a process's runtime stops all its tasks,
//! as an exit would (the timer restart harness, `state_timer_restart`).

use super::*;

/// How long, on the simulated clock, a test waits for what it expects.
pub(super) const WITHIN: Duration = Duration::from_secs(20);

/// How many seeds each test runs. A seed sets the simulation's ids and the
/// times the tests draw: when the first move lands and how long a stopped
/// server stays down.
pub(super) const SEEDS: u64 = 8;

/// The log line the kernel leaves each time it stops a timer.
pub(super) const STOPPED: &str = "state timer stopped: nothing names its caller";

#[derive(Clone, Default)]
pub(super) struct CapturedLog(Arc<Mutex<Vec<u8>>>);

impl Write for CapturedLog {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl CapturedLog {
    /// Capture what this thread logs until the guard drops. Every process of
    /// a simulation runs its tasks on the test's own thread, so this sees all
    /// of them, and tests that share a process (plain `cargo test`) each see
    /// only their own.
    fn install() -> (Self, tracing::subscriber::DefaultGuard) {
        let logs = Self::default();
        let writer = logs.clone();
        let guard = tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .json()
                .without_time()
                .with_writer(move || writer.clone())
                .finish(),
        );
        (logs, guard)
    }

    /// The times a timer of record `id` was stopped.
    pub(super) fn stops(&self, id: &str) -> usize {
        String::from_utf8_lossy(&self.0.lock().unwrap())
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|event| {
                event["fields"]["message"] == STOPPED && event["fields"]["entity_id"] == id
            })
            .count()
    }
}

/// Timer rows committed with the journal's events, as on Turso.
pub(super) struct CommittedRows(SimEventStore);

#[async_trait::async_trait]
impl StateTimerStore for CommittedRows {
    async fn upsert_state_timer(&self, row: &StateTimerRecord) -> Result<(), PersistenceError> {
        self.0.upsert_state_timer(row);
        Ok(())
    }

    async fn delete_state_timer(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        arm_id: &str,
    ) -> Result<(), PersistenceError> {
        self.0
            .delete_state_timer(tenant, entity_type, entity_id, arm_id);
        Ok(())
    }

    async fn list_state_timers(
        &self,
        tenant: &str,
    ) -> Result<Vec<StateTimerRecord>, PersistenceError> {
        Ok(self.0.list_state_timers(tenant))
    }
}

/// One seed's simulation: its clock, ids, draws, store and log, across the
/// server processes it runs.
pub(super) struct Sim {
    pub(super) seed: u64,
    pub(super) store: SimEventStore,
    pub(super) logs: CapturedLog,
    clock: Arc<ProcessClock>,
    rng: DeterministicRng,
    _log: tracing::subscriber::DefaultGuard,
    _context: SimContextGuard,
}

impl Sim {
    fn new(seed: u64) -> Self {
        let clock = Arc::new(ProcessClock::new());
        let context = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
        let (logs, log) = CapturedLog::install();
        Self {
            seed,
            store: SimEventStore::no_faults(seed),
            logs,
            clock,
            rng: DeterministicRng::new(seed ^ 0x7173_e0ff),
            _log: log,
            _context: context,
        }
    }

    /// Run `test` once for each of [`SEEDS`] seeds, each a simulation of its
    /// own.
    pub(super) fn each_seed(test: impl Fn(&mut Sim)) {
        for seed in 0..SEEDS {
            test(&mut Sim::new(seed));
        }
    }

    /// A drawn time under `bound_ms` milliseconds.
    pub(super) fn draw(&mut self, bound_ms: u64) -> Duration {
        Duration::from_millis(self.rng.next_bound(bound_ms as usize) as u64)
    }

    /// A started server process, with Shipment's timer at `after` seconds.
    pub(super) fn start(&self, after: u32) -> Process {
        self.start_over(after, None)
    }

    /// [`Self::start`], reading and writing the journal through `events`
    /// when given.
    pub(super) fn start_over(
        &self,
        after: u32,
        events: Option<temper_server::storage::BoxedEventStore>,
    ) -> Process {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .expect("runtime");
        let state = runtime.block_on(async {
            self.clock.begin();
            server_over(&format!("seed-{}", self.seed), &self.store, after, events).await
        });
        Process { runtime, state }
    }

    /// Stop `process` as an exit would, timers and all, and leave the store
    /// with no server for a drawn time, up to three seconds, which can leave
    /// timers overdue at the next start.
    pub(super) fn stop(&mut self, process: Process) {
        let downtime = self.draw(3000);
        let Process { state, runtime } = process;
        runtime.block_on(async { self.clock.end(downtime) });
        drop(state);
        drop(runtime);
    }
}

/// One simulated server process.
pub(super) struct Process {
    // The server goes before the runtime its tasks run on.
    pub(super) state: ServerState,
    runtime: tokio::runtime::Runtime,
}

impl Process {
    /// Run `work` on this process until it ends.
    pub(super) fn run<T>(&self, work: impl Future<Output = T>) -> T {
        self.runtime.block_on(work)
    }
}

/// A started server over `store`, with Shipment's timer at `after` seconds,
/// reading and writing its journal through `events` when given.
async fn server_over(
    name: &str,
    store: &SimEventStore,
    after: u32,
    events: Option<temper_server::storage::BoxedEventStore>,
) -> ServerState {
    let shipment = SHIPMENT_IOA.replace("AFTER", &after.to_string());
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        parse_csdl(CSDL).expect("CSDL should parse"),
        CSDL.to_string(),
        &[
            ("Line", LINE_IOA),
            ("Shipment", shipment.as_str()),
            ("Check", CHECK_IOA),
            ("Poller", POLLER_IOA),
            ("Asker", ASKER_IOA),
            ("Hopper", HOPPER_IOA),
        ],
    );
    let mut state = ServerState::from_registry(ActorSystem::new(name), registry);
    let mut storage = StorageStack::from_sim(store.clone(), None);
    if let Some(events) = events {
        storage.events = events;
    }
    state.set_storage_stack(storage);
    state.state_timer_rows = Some(Arc::new(CommittedRows(store.clone())));
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .expect("policy should load");
    state.rebuild_reaction_dispatcher();
    let hash = state
        .wasm_engine
        .compile_and_cache(ECHO_WASM)
        .expect("the module compiles");
    state
        .wasm_module_registry
        .write()
        .expect("wasm registry lock") // ci-ok: infallible lock
        .register(&TenantId::default(), "trigger_principal_echo", &hash);
    state
        .start_runtime()
        .await
        .expect("server starts")
        .await
        .expect("boot walk completes");
    state
}

pub(super) fn as_who(security_ctx: SecurityContext) -> AgentContext {
    AgentContext {
        security_ctx: Some(security_ctx),
        ..AgentContext::default()
    }
}

pub(super) fn person(id: &str) -> AgentContext {
    as_who(SecurityContext::from_verified_jwt(
        id,
        PrincipalKind::Customer,
        None,
        None,
        None,
        None,
    ))
}

pub(super) fn agent_for(person: &str) -> AgentContext {
    as_who(SecurityContext::from_verified_jwt(
        "agent-1",
        PrincipalKind::Agent,
        Some("claude-code"),
        Some(person),
        Some("contributor"),
        None,
    ))
}

/// Make `action` on `entity_type` `id` as `by`, creating the record first.
pub(super) async fn act(
    state: &ServerState,
    entity_type: &str,
    id: &str,
    action: &str,
    by: &AgentContext,
) {
    let tenant = TenantId::default();
    state
        .get_or_create_tenant_entity(&tenant, entity_type, id, json!({}))
        .await
        .expect("the record is created");
    let response = state
        .dispatch_tenant_action(&tenant, entity_type, id, action, json!({}), by)
        .await
        .expect("the move is dispatched");
    assert!(
        response.success,
        "{action} on {entity_type} {id}: {:?}",
        response.error
    );
}

/// The record as an ordinary read returns it.
pub(super) async fn read(state: &ServerState, set: &str, id: &str) -> Value {
    let mut request = Request::builder()
        .method("GET")
        .uri(format!("/tdata/{set}('{id}')"))
        .body(Body::empty())
        .unwrap();
    request
        .extensions_mut()
        .insert(AuthenticatedRequestContext::new(
            TenantId::default(),
            SecurityContext::from_resolved_identity("reader", "person", None),
        ));
    let response = build_router(state.clone()).oneshot(request).await.unwrap();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

/// Let all work due at this simulated instant finish.
pub(super) async fn settle() {
    for _ in 0..600 {
        tokio::task::yield_now().await;
    }
}

/// Wait on the simulated clock, `for_` long, letting the work it brings run.
pub(super) async fn wait(for_: Duration) {
    tokio::time::sleep(for_).await;
    settle().await;
}

/// Wait until `holds` holds of the record, or fail saying what it read.
pub(super) async fn until(
    state: &ServerState,
    set: &str,
    id: &str,
    what: &str,
    holds: fn(&Value) -> bool,
) -> Value {
    let deadline = tokio::time::Instant::now() + WITHIN;
    loop {
        settle().await;
        let record = read(state, set, id).await;
        if holds(&record) {
            return record;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{set} {id} never {what}: {record}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

pub(super) fn is_stopped(record: &Value) -> bool {
    record["timer_needs_person"].is_string()
}
