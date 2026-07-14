//! ARN-203: pending state-timeouts must survive a server restart.
//!
//! `[[state_timeout]]` declarations (ADR-0049) arm in-memory tokio timers at
//! dispatch time, and ADR-0056 added hydration re-arm — but only on the NEXT
//! dispatch to the entity. An entity sitting in a timed state across a
//! restart receives no dispatch by definition (the timeout exists to fire
//! when nothing happens), so its `on_timeout` never fires. These tests drive
//! the same boot sequence `temper serve` runs (fresh `ServerState` over the
//! persisted store + `populate_index_from_store`) and require the pending
//! timeout to fire without any post-restart traffic.

use std::time::Duration;

use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_store_turso::TursoEventStore;

use temper_server::registry::SpecRegistry;
use temper_server::state::ServerState;
use temper_server::storage::StorageStack;
use temper_spec::csdl::parse_csdl;

const CSDL_XML: &str = include_str!("../../../test-fixtures/specs/model.csdl.xml");

/// Ticket spec whose `Open` state times out after 1 second into
/// `AssignAgent` (Open → InProgress).
const TICKET_WITH_TIMEOUT_IOA: &str = r#"
[automaton]
name = "Ticket"
states = ["Open", "InProgress", "WaitingOnCustomer", "Resolved", "Closed"]
initial = "Open"
allow_indefinite_states = ["InProgress", "WaitingOnCustomer", "Resolved", "Closed"]

[[state]]
name = "replies"
type = "counter"
initial = "0"

[[action]]
name = "AssignAgent"
kind = "input"
from = ["Open"]
to = "InProgress"

[[state_timeout]]
state = "Open"
after_seconds = 1
on_timeout = "AssignAgent"
"#;

fn build_state(system_name: &str, store: TursoEventStore) -> ServerState {
    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(CSDL_XML).expect("CSDL should parse");
    registry.register_tenant(
        "tenant-a",
        csdl,
        CSDL_XML.to_string(),
        &[("Ticket", TICKET_WITH_TIMEOUT_IOA)],
    );
    let mut state = ServerState::from_registry(ActorSystem::new(system_name), registry);
    state.set_storage_stack(StorageStack::from_turso(store));
    state
}

async fn open_store(db_url: &str) -> TursoEventStore {
    TursoEventStore::new(db_url, None)
        .await
        .expect("open local turso db")
}

/// Poll the entity status until it matches `expected` or the deadline passes.
async fn wait_for_status(
    state: &ServerState,
    tenant: &TenantId,
    entity_id: &str,
    expected: &str,
    deadline: Duration,
) -> String {
    let start = std::time::Instant::now();
    loop {
        let current = state
            .get_tenant_entity_state(tenant, "Ticket", entity_id)
            .await
            .expect("entity should load")
            .state
            .status;
        if current == expected || start.elapsed() > deadline {
            return current;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Run generation A (create one ticket) on its OWN tokio runtime and then
/// DROP that runtime — aborting every task it spawned, including the timer
/// the creation-arm (ARN-203) legitimately arms in generation A. This makes
/// the "crash" real: nothing from generation A can fire afterwards, so the
/// post-restart transition can only come from generation B's boot resume.
fn run_and_hard_kill_generation_a(system_name: &str, db_url: &str, entity_id: &str) {
    let system_name = system_name.to_string();
    let db_url = db_url.to_string();
    let entity_id = entity_id.to_string();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("build generation-A runtime");
        rt.block_on(async move {
            let tenant = TenantId::from("tenant-a".to_string());
            let state_a = build_state(&system_name, open_store(&db_url).await);
            let created = state_a
                .get_or_create_tenant_entity(&tenant, "Ticket", &entity_id, serde_json::json!({}))
                .await
                .expect("create ticket");
            assert_eq!(created.state.status, "Open");
        });
        // Hard kill: dropping the runtime aborts all spawned tasks.
        rt.shutdown_timeout(Duration::from_millis(100));
    })
    .join()
    .expect("generation A thread");
}

/// An entity that entered a timed state before a restart, with time still
/// left on the budget, must have its timer re-armed at boot and fire on
/// schedule — with NO post-restart dispatch to the entity.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_state_timeout_fires_after_restart() {
    let db_path =
        std::env::temp_dir().join(format!("temper-arn203-pending-{}.db", uuid::Uuid::new_v4()));
    let db_url = format!("file:{}", db_path.display());
    let tenant = TenantId::from("tenant-a".to_string());

    // Generation A: create the ticket (entering the timed `Open` state),
    // then hard-kill its runtime with the 1s budget not yet elapsed. The
    // creation-arm DOES arm an in-memory timer in generation A; the hard
    // kill aborts it, exactly like a crashed server process.
    run_and_hard_kill_generation_a("arn203-gen-a", &db_url, "t-restart-1");

    // Generation B: the boot sequence `temper serve` runs for a tenant.
    let state_b = build_state("arn203-gen-b", open_store(&db_url).await);
    state_b.populate_index_from_store(&tenant).await;

    // Gen-B must own the transition: still Open right after boot resume arm.
    let boot = state_b
        .get_tenant_entity_state(&tenant, "Ticket", "t-restart-1")
        .await
        .expect("load after boot");
    assert_eq!(boot.state.status, "Open", "restart must leave entity Open before resume fires");

    // No dispatch to the entity. The pending timeout alone must fire.
    let status = wait_for_status(
        &state_b,
        &tenant,
        "t-restart-1",
        "InProgress",
        Duration::from_secs(20),
    )
    .await;
    assert_eq!(
        status, "InProgress",
        "pending state timeout must fire after restart without any dispatch to the entity"
    );
}

/// An entity whose timeout budget fully elapsed while the server was down
/// must have `on_timeout` fired promptly at boot (the overdue case).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn overdue_state_timeout_fires_after_restart() {
    let db_path =
        std::env::temp_dir().join(format!("temper-arn203-overdue-{}.db", uuid::Uuid::new_v4()));
    let db_url = format!("file:{}", db_path.display());
    let tenant = TenantId::from("tenant-a".to_string());

    // Hard-kill generation A immediately after creation, before its
    // creation-armed timer can fire.
    run_and_hard_kill_generation_a("arn203-gen-a2", &db_url, "t-overdue-1");

    // The 1s budget expires entirely while the server is "down".
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let state_b = build_state("arn203-gen-b2", open_store(&db_url).await);
    let boot_start = std::time::Instant::now();
    state_b.populate_index_from_store(&tenant).await;

    let boot = state_b
        .get_tenant_entity_state(&tenant, "Ticket", "t-overdue-1")
        .await
        .expect("load after boot");
    assert_eq!(boot.state.status, "Open", "overdue entity still Open right after boot");

    // Overdue: must fire promptly (≪ full 1s budget). Cap wait at 800ms so a
    // full-budget re-arm regression fails instead of silently passing.
    let status = wait_for_status(
        &state_b,
        &tenant,
        "t-overdue-1",
        "InProgress",
        Duration::from_millis(800),
    )
    .await;
    let elapsed = boot_start.elapsed();
    assert_eq!(
        status, "InProgress",
        "an overdue state timeout must fire at boot, not wait another full budget or never fire"
    );
    assert!(
        elapsed < Duration::from_millis(800),
        "overdue fire took {elapsed:?}; expected remaining budget ~0 (≪ 1s)"
    );
}

/// Sibling of the restart cases (same defect class, found during review of
/// the fix): an entity CREATED into a timed initial state has no dispatch
/// to arm its timer, so without arming at creation its `on_timeout` never
/// fires while the server stays up.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn state_timeout_fires_for_entity_created_into_timed_state() {
    let db_path =
        std::env::temp_dir().join(format!("temper-arn203-create-{}.db", uuid::Uuid::new_v4()));
    let db_url = format!("file:{}", db_path.display());
    let tenant = TenantId::from("tenant-a".to_string());

    let state = build_state("arn203-create", open_store(&db_url).await);
    let created = state
        .get_or_create_tenant_entity(&tenant, "Ticket", "t-created-1", serde_json::json!({}))
        .await
        .expect("create ticket");
    assert_eq!(created.state.status, "Open");

    // No restart, no dispatch — the creation itself must arm the timer.
    let status = wait_for_status(
        &state,
        &tenant,
        "t-created-1",
        "InProgress",
        Duration::from_secs(20),
    )
    .await;
    assert_eq!(
        status, "InProgress",
        "a state timeout must fire for an entity created into the timed state, without any dispatch"
    );
}
