//! Live heap stays flat while an app keeps writing.
//!
//! Incident i8 (15 Oct 2026): an app with 25 feeds, each rewriting about 40 KB
//! of state every few seconds, grew its Temper by about 50 MB every 30 seconds
//! until the kernel killed it at about 400 MB. Every dispatched action carries
//! an idempotency key, and each one left a full copy of the entity's state,
//! with its recent events and their parameters, in the idempotency cache for
//! an hour.
//!
//! This drives the same shape through dispatch against a file database and
//! counts live heap bytes with a counting allocator. After a warm-up that fills
//! every bounded window (recent events, the idempotency cache), the same amount
//! of writing again must leave the live heap where it was.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::idempotency::IDEMPOTENCY_CACHE_BYTES_BUDGET;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::{ServerState, StorageStack};
use temper_spec::csdl::parse_csdl;
use temper_store_turso::TursoEventStore;

struct CountingAlloc;

static LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            LIVE_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        }
        ptr
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            LIVE_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        }
        ptr
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        LIVE_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_ptr = unsafe { System.realloc(ptr, layout, new_size) };
        if !new_ptr.is_null() {
            LIVE_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
            LIVE_BYTES.fetch_add(new_size, Ordering::Relaxed);
        }
        new_ptr
    }
}

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

const CSDL_XML: &str = include_str!("../../../test-fixtures/specs/model.csdl.xml");

const FEED_IOA: &str = r#"
[automaton]
name = "Feed"
states = ["Unmade", "Running"]
initial = "Unmade"
allow_indefinite_states = ["Unmade", "Running"]

[[state]]
name = "queue"
type = "string"
initial = ""

[[state]]
name = "seen"
type = "string"
initial = ""

[[action]]
name = "Add"
kind = "input"
from = ["Unmade"]
to = "Running"

[[action]]
name = "Stepped"
kind = "input"
from = ["Running"]
params = ["queue", "seen"]
"#;

const FEEDS: usize = 5;
/// Rounds that fill every bounded window: 55 events per feed passes the
/// recent-event window (50), and 275 replies of about 70 KB fill the
/// idempotency cache.
const WARM_ROUNDS: usize = 55;
/// The same writing again; nothing it adds may stay.
const MEASURED_ROUNDS: usize = 40;
/// Bytes of state each step writes.
const STEP_BYTES: usize = 64 * 1024;
/// Growth allowed over the measured rounds. Before the fix every write kept a
/// reply holding the feed's last 50 events (about 3 MB each), so this budget
/// was gone within two rounds. Keeping only each reply's fields (about 70 KB)
/// without a byte bound would add about 14 MB here; a flat runtime adds about 1.
const GROWTH_BUDGET_BYTES: usize = 8 * 1024 * 1024;

fn text(len: usize, seed: usize) -> String {
    let mut out = String::with_capacity(len + 64);
    let mut x = seed as u64;
    while out.len() < len {
        x = x
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        out.push_str(&format!("https://gallery.example/entry/{x:x}\n"));
    }
    out.truncate(len);
    out
}

async fn feed_state(db_url: &str) -> ServerState {
    let store = TursoEventStore::new(db_url, None)
        .await
        .expect("open file database");
    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(CSDL_XML).expect("CSDL parse");
    registry.register_tenant("default", csdl, CSDL_XML.to_string(), &[("Feed", FEED_IOA)]);
    let mut state = ServerState::from_registry(ActorSystem::new("flat-memory"), registry);
    state.set_storage_stack(StorageStack::from_turso(store));
    state
}

async fn step(state: &ServerState, tenant: &TenantId, round: usize, feed: usize) {
    let params = serde_json::json!({
        "queue": text(STEP_BYTES / 2, round * FEEDS + feed),
        "seen": text(STEP_BYTES / 2, round * FEEDS + feed + 7),
    });
    let response = state
        .dispatch_tenant_action(
            tenant,
            "Feed",
            &format!("feed-{feed}"),
            "Stepped",
            params,
            &AgentContext::default(),
        )
        .await
        .expect("Stepped dispatches");
    assert!(response.success, "Stepped failed: {:?}", response.error);
}

#[tokio::test]
async fn sustained_writes_keep_the_live_heap_flat() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_url = format!("file:{}", dir.path().join("flat.db").display());
    let state = feed_state(&db_url).await;
    let tenant = TenantId::default();

    for feed in 0..FEEDS {
        let id = format!("feed-{feed}");
        state
            .get_or_create_tenant_entity(&tenant, "Feed", &id, serde_json::json!({"Id": id}))
            .await
            .expect("create feed");
        let added = state
            .dispatch_tenant_action(
                &tenant,
                "Feed",
                &id,
                "Add",
                serde_json::json!({}),
                &AgentContext::default(),
            )
            .await
            .expect("Add dispatches");
        assert!(added.success, "Add failed: {:?}", added.error);
    }

    for round in 0..WARM_ROUNDS {
        for feed in 0..FEEDS {
            step(&state, &tenant, round, feed).await;
        }
    }
    let warm = LIVE_BYTES.load(Ordering::Relaxed);

    for round in WARM_ROUNDS..WARM_ROUNDS + MEASURED_ROUNDS {
        for feed in 0..FEEDS {
            step(&state, &tenant, round, feed).await;
        }
        let grown = LIVE_BYTES.load(Ordering::Relaxed).saturating_sub(warm);
        assert!(
            grown <= GROWTH_BUDGET_BYTES,
            "live heap grew {grown} bytes after {} more rounds of {FEEDS} writes of {STEP_BYTES} bytes \
             (warm {warm}, budget {GROWTH_BUDGET_BYTES})",
            round + 1 - WARM_ROUNDS,
        );
    }

    assert!(state.idempotency_cache.held_bytes() <= IDEMPOTENCY_CACHE_BYTES_BUDGET);
    let grown = LIVE_BYTES.load(Ordering::Relaxed).saturating_sub(warm);
    let written = MEASURED_ROUNDS * FEEDS * STEP_BYTES;
    eprintln!(
        "flat-memory: warm {warm} bytes; {written} bytes written after warm-up grew the live heap {grown} bytes"
    );
}
